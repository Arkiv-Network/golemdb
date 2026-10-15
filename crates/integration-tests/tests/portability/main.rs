//! Checks that databases and their roots are the same on every platform.
//!
//! `create` writes a fixed workload into one fresh database per hash
//! algorithm under `$PORTABILITY_DIR`. `extend` reopens those databases,
//! possibly copied from another platform, and writes more commits. Both write
//! a report to `$PORTABILITY_REPORT`: each line shows a commit's roots, and the
//! `head` and `end` lines also hash every record read back. The Portability
//! workflow runs both on every platform and requires all reports to match, so
//! they are ignored by default. Tests run in the crate directory, so give
//! absolute paths.
//!
//! Only the public API is used, plus the storage adapter for the backend's
//! key and value limits.
//!
//! ```text
//! PORTABILITY_DIR=<dir> PORTABILITY_REPORT=<file> \
//!     cargo test -p golemdb-integration-tests --test portability -- --ignored --exact create
//! ```

use std::{
    error::Error,
    fs::File,
    io::Write,
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
};

use golemdb_api::{
    Api, ApiError, CellLimits, CellValue, Database, Genesis, HashAlgorithm, OpenConfig, OpenMode,
    ReadTarget, RecordKey, RecordOp,
};
use golemdb_storage::Store;
use golemdb_storage_mdbx::MdbxStore;

/// Workload and storage errors alike end the check.
type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Commits written by `create`, and by `create` and `extend` together.
const CREATED_COMMITS: u64 = 10;
const TOTAL_COMMITS: u64 = 15;
/// Records created by each commit.
const RECORDS_PER_COMMIT: u64 = 100;

/// Hash algorithms under test; each gets its own database.
const HASHES: [HashAlgorithm; 2] = [HashAlgorithm::Keccak256, HashAlgorithm::Blake3];

/// A string attribute whose name has the maximum length. Its longest values
/// give 1,934-byte index keys, close to the 2,022-byte limit of 4 KiB pages.
const TITLE: &str = "description_of_the_listing_title";
const MAX_STR_LEN: usize = 1900;
/// Large enough to need overflow pages in a page-based store.
const MAX_BYTES_LEN: usize = 32 * 1024;
/// One record in this many has a title and a blob of the maximum length.
const LONG_ONE_IN: u64 = 10;
/// The other records' titles and blobs are shorter than these lengths.
const SHORT_STR_LEN: u64 = 64;
const SHORT_BYTES_LEN: u64 = 256;
/// Turns a record ID into a price; negative, to cover signed integers.
const PRICE_FACTOR: i64 = -7;
/// Turns a record ID into a ratio with a fractional part.
const RATIO_DIVISOR: f64 = 3.0;
/// The previous commit's records are changed in a pattern that repeats every
/// this many IDs: the first is patched, the second deleted, the rest kept.
const PATCH_DELETE_PERIOD: u64 = 4;

/// The storage backend under test. Replacing MDBX means changing only this.
fn open_store(path: &Path) -> Result<impl Store + Send + Sync + 'static> {
    Ok(MdbxStore::open(path)?)
}

/// Report label and directory name of a hash algorithm.
fn label(hash: HashAlgorithm) -> &'static str {
    match hash {
        HashAlgorithm::Keccak256 => "keccak256",
        HashAlgorithm::Blake3 => "blake3",
    }
}

/// The workload's genesis.
fn config(hash: HashAlgorithm, mode: OpenMode) -> OpenConfig {
    OpenConfig::new(Genesis::new(
        hash,
        CellLimits {
            max_cell_name_len: TITLE.len() as u32,
            max_str_len: MAX_STR_LEN as u32,
            max_bytes_len: MAX_BYTES_LEN as u32,
        },
    ))
    .with_mode(mode)
}

/// A record key whose leading bytes vary, so that keys spread out.
fn key(id: u64) -> RecordKey {
    const SPREAD: u64 = 0x9E37_79B9_7F4A_7C15;
    let (spread, id) = (id.wrapping_mul(SPREAD).to_be_bytes(), id.to_be_bytes());
    let mut key = [0; 32];
    let tail = key.len() - id.len();
    key[..spread.len()].copy_from_slice(&spread);
    key[tail..].copy_from_slice(&id);
    RecordKey(key)
}

/// IDs of the records created by the given commit, counting commits from 1.
fn ids(commit: u64) -> Range<u64> {
    (commit - 1) * RECORDS_PER_COMMIT..commit * RECORDS_PER_COMMIT
}

/// A head commit ID with its state and index roots.
type Head = (u64, [u8; 32], [u8; 32]);

/// Create this commit's records, patch or delete some of the previous commit's,
/// and commit.
fn write_commit(db: &Database, commit: u64) -> Result<Head> {
    let branch = db.begin()?;
    for id in ids(commit) {
        let long = id % LONG_ONE_IN == 0;
        let title_len = if long {
            MAX_STR_LEN
        } else {
            (id % SHORT_STR_LEN) as usize
        };
        let blob_len = if long {
            MAX_BYTES_LEN
        } else {
            (id % SHORT_BYTES_LEN) as usize
        };
        let record = RecordOp::create(key(id))
            .attribute("price", CellValue::from_i64(PRICE_FACTOR * id as i64))?
            .attribute("ratio", CellValue::from_f64(id as f64 / RATIO_DIVISOR)?)?
            .attribute(TITLE, CellValue::from_str(&format!("{id:0>title_len$}")))?
            .field("blob", CellValue::from_bytes(&vec![id as u8; blob_len]))?;
        db.create(branch, record)?;
    }
    if commit > 1 {
        for id in ids(commit - 1) {
            match id % PATCH_DELETE_PERIOD {
                0 => {
                    let patch = RecordOp::patch(key(id))
                        .attribute("price", CellValue::from_i64(id as i64))?
                        .remove(TITLE)?;
                    db.patch(branch, patch)?;
                }
                1 => db.delete(branch, RecordOp::delete(key(id)))?,
                _ => {}
            }
        }
    }
    let sealed = db.seal(branch)?;
    db.commit(branch)?;
    Ok((sealed.commit_id.get(), sealed.state_root, sealed.index_root))
}

/// FNV-1a: a small hash with the same output on every platform.
fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    bytes.iter().fold(hash, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// Hash of every record created up to `commit`, read back through the API.
/// Deleted records count as absent.
fn contents(db: &Database, commit: u64) -> Result<u64> {
    const OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
    let mut hash = OFFSET_BASIS;
    for id in 0..commit * RECORDS_PER_COMMIT {
        match db.get(ReadTarget::Head, RecordOp::get(key(id))) {
            Ok(record) => {
                for (name, value) in &record.cells {
                    for part in [name.as_bytes(), value.encoded_bytes()] {
                        hash = fnv1a(hash, &(part.len() as u64).to_be_bytes());
                        hash = fnv1a(hash, part);
                    }
                }
            }
            Err(ApiError::NotFound) => hash = fnv1a(hash, b"absent"),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(hash)
}

/// Lowercase hex, for report lines.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Report a head's roots, and the hash of its records if they were read back.
fn report_head(
    report: &mut File,
    hash: HashAlgorithm,
    event: &str,
    head: Head,
    contents: Option<u64>,
) -> Result<()> {
    let (commit, state, index) = head;
    let contents = contents.map_or(String::new(), |hash| format!(" contents={hash:016x}"));
    writeln!(
        report,
        "{} {event} commit={commit} state_root={} index_root={}{contents}",
        label(hash),
        hex(&state),
        hex(&index),
    )?;
    Ok(())
}

/// For each hash, open the database under `$PORTABILITY_DIR`, write
/// `commits`, and write the report. Records are read back only at the `head`
/// line, which is the state `create` leaves and `extend` must find on
/// reopening, and at the end.
fn run(mode: OpenMode, commits: RangeInclusive<u64>) -> Result<()> {
    let dir = env_path("PORTABILITY_DIR")?;
    let mut report = File::create(env_path("PORTABILITY_REPORT")?)?;
    for hash in HASHES {
        let store = open_store(&dir.join(label(hash)))?;
        writeln!(
            report,
            "{} limits max_key_size={} max_value_size={}",
            label(hash),
            store.max_key_size(),
            store.max_value_size()
        )?;
        let db = Database::from_store(store, &config(hash, mode))?;
        let opened = *db.info();
        let mut head = (opened.commit_id.get(), opened.state_root, opened.index_root);
        if mode == OpenMode::ExistingOnly {
            report_head(
                &mut report,
                hash,
                "head",
                head,
                Some(contents(&db, head.0)?),
            )?;
        }
        for commit in commits.clone() {
            head = write_commit(&db, commit)?;
            report_head(&mut report, hash, "sealed", head, None)?;
        }
        let event = if mode == OpenMode::CreateNew {
            "head"
        } else {
            "end"
        };
        report_head(&mut report, hash, event, head, Some(contents(&db, head.0)?))?;
    }
    Ok(())
}

/// A path given by the workflow in an environment variable.
fn env_path(name: &str) -> Result<PathBuf> {
    Ok(std::env::var_os(name).ok_or(format!("set {name}"))?.into())
}

#[test]
#[ignore = "run by the Portability workflow"]
fn create() -> Result<()> {
    run(OpenMode::CreateNew, 1..=CREATED_COMMITS)
}

#[test]
#[ignore = "run by the Portability workflow"]
fn extend() -> Result<()> {
    run(OpenMode::ExistingOnly, CREATED_COMMITS + 1..=TOTAL_COMMITS)
}
