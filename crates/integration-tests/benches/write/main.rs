//! Times `seal` and `commit` of record batches on an MDBX database with
//! 100,000 records.

mod workload;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use golemdb_api::{
    Api, BranchId, CellLimits, Database, Genesis, HashAlgorithm, OpenConfig, OpenMode,
    PricingConfig, RecordLimits,
};
use golemdb_storage::{Store, StoreStats};
use golemdb_storage_mdbx::{MdbxOptions, MdbxStore};
use tempfile::TempDir;
use workload::{Batch, Workload};

const RECORDS: u64 = 100_000;
const PRELOAD_BATCH: u64 = 5_000;
const _: () = assert!(RECORDS.is_multiple_of(PRELOAD_BATCH));
/// Commits after preloading, so cases run on a database with a history rather
/// than a freshly loaded one.
const HISTORY: u64 = 200;

/// On a real disk, unlike `/tmp`, which is often RAM-backed and hides writes.
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const DATA: &str = "mdbx.dat";
const MIB: f64 = 1024.0 * 1024.0;

#[rustfmt::skip]
const MIXED: Batch = Batch { creates: 70, patches: 60, deletes: 70 };
#[rustfmt::skip]
const BATCHES: [(&str, Batch); 4] = [
    ("create", Batch { creates: 200, patches: 0, deletes: 0 }),
    ("patch", Batch { creates: 0, patches: 200, deletes: 0 }),
    ("delete", Batch { creates: 0, patches: 0, deletes: 200 }),
    ("mixed", MIXED),
];

/// Seal cases run on the preloaded database, since sealing writes nothing;
/// commit cases run on a copy reset to it.
struct Snapshot {
    fixture: Fixture,
    // After `fixture`, which must close before its directory is deleted.
    dir: TempDir,
    copy_dir: TempDir,
}

impl Snapshot {
    fn build() -> Self {
        let dir = bench_dir();
        let mut fixture = Fixture::open(dir.path(), OpenMode::CreateNew, Workload::default());
        for _ in 0..RECORDS / PRELOAD_BATCH {
            fixture.commit(Batch {
                creates: PRELOAD_BATCH,
                patches: 0,
                deletes: 0,
            });
        }
        for _ in 0..HISTORY {
            fixture.commit(MIXED);
        }
        // Not `fs::copy`, which can clone: a clone makes the timed commits
        // allocate blocks. Copied now, the disk settles before commits are timed.
        let copy_dir = bench_dir();
        let copy = copy_dir.path().join(DATA);
        fs::write(&copy, fs::read(dir.path().join(DATA)).unwrap()).unwrap();
        File::open(&copy).unwrap().sync_all().unwrap();
        Self {
            fixture,
            dir,
            copy_dir,
        }
    }

    /// The copy, reset to the snapshot and opened.
    fn fresh_copy(&self) -> Fixture {
        restore(self.dir.path(), self.copy_dir.path());
        Fixture::open(
            self.copy_dir.path(),
            OpenMode::ExistingOnly,
            self.fixture.workload,
        )
    }
}

struct Fixture {
    db: Database,
    store: MdbxStore,
    workload: Workload,
}

impl Fixture {
    fn open(dir: &Path, mode: OpenMode, workload: Workload) -> Self {
        let store = MdbxStore::open(dir).unwrap();
        let db = Database::from_store(store.clone(), &config(mode)).unwrap();
        Self {
            db,
            store,
            workload,
        }
    }

    /// Times sealing `batch`, then discards it. Asserts the database is unchanged,
    /// since commit cases copy it while open.
    fn time_seal(&self, batch: Batch) -> Duration {
        let before = self.stats();
        let branch = self.apply(batch);
        let start = Instant::now();
        self.db.seal(branch).unwrap();
        let elapsed = start.elapsed();
        self.db.discard(branch).unwrap();
        assert_eq!(self.stats(), before, "sealing wrote to the database");
        elapsed
    }

    fn time_commit(self, batch: Batch) -> Duration {
        let branch = self.apply(batch);
        self.db.seal(branch).unwrap();
        let start = Instant::now();
        self.db.commit(branch).unwrap();
        start.elapsed()
    }

    /// Bytes written and syncs of one commit, and the database size after it.
    fn commit_storage(mut self, batch: Batch) -> String {
        let before = self.stats();
        self.commit(batch);
        let after = self.stats();
        let written = (after.session_bytes_written - before.session_bytes_written) as f64;
        format!(
            "{:.2} MiB written and {} sync(s) per commit, database {:.0} MiB",
            written / MIB,
            after.session_syncs - before.session_syncs,
            after.db_size as f64 / MIB,
        )
    }

    fn apply(&self, batch: Batch) -> BranchId {
        self.workload.apply(&self.db, batch).unwrap()
    }

    fn commit(&mut self, batch: Batch) {
        self.workload.commit(&self.db, batch).unwrap();
    }

    fn stats(&self) -> StoreStats {
        self.store.stats().unwrap().unwrap()
    }
}

fn config(mode: OpenMode) -> OpenConfig {
    let cells = CellLimits {
        max_cell_name_len: 32,
        max_str_len: 128,
        max_bytes_len: 128 * 1024,
    };
    let records = RecordLimits {
        max_cells: 16,
        max_indexed_cells: 16,
    };
    let no_immutable_data = None;
    let genesis = Genesis::new(
        HashAlgorithm::Keccak256,
        cells,
        records,
        no_immutable_data,
        PricingConfig::zero(),
    );
    OpenConfig::new(genesis).with_mode(mode)
}

fn write(c: &mut Criterion) {
    let mut group = c.benchmark_group(format!("write/{}k", RECORDS / 1000));
    group.sampling_mode(SamplingMode::Flat);
    // Criterion calls each closure many times, and a filtered run may call none.
    let mut snapshot: Option<Snapshot> = None;

    for (name, batch) in BATCHES {
        group.bench_function(format!("seal/{name}"), |b| {
            let snapshot = snapshot.get_or_insert_with(Snapshot::build);
            b.iter_custom(|iters| (0..iters).map(|_| snapshot.fixture.time_seal(batch)).sum())
        });
    }

    for (name, batch) in BATCHES {
        let mut storage = None;
        group.bench_function(format!("commit/{name}"), |b| {
            let snapshot = snapshot.get_or_insert_with(Snapshot::build);
            storage.get_or_insert_with(|| snapshot.fresh_copy().commit_storage(batch));
            b.iter_custom(|iters| {
                (0..iters)
                    .map(|_| snapshot.fresh_copy().time_commit(batch))
                    .sum()
            })
        });
        if let Some(storage) = storage {
            println!("commit/{name}: {storage}\n");
        }
    }
    group.finish();
}

fn bench_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("golemdb-bench-")
        .tempdir_in(TMP)
        .unwrap()
}

/// Makes `to`'s data file equal to `from`'s, rewriting only the pages that differ.
fn restore(from: &Path, to: &Path) {
    const BLOCK: usize = 1 << 20;
    let page = MdbxOptions::default().page_size;
    let source = File::open(from.join(DATA)).unwrap();
    let copy = OpenOptions::new()
        .read(true)
        .write(true)
        .open(to.join(DATA))
        .unwrap();
    let len = source.metadata().unwrap().len();
    copy.set_len(len).unwrap();
    let (mut source_block, mut copy_block) = (vec![0; BLOCK], vec![0; BLOCK]);
    for offset in (0..len).step_by(BLOCK) {
        let n = BLOCK.min((len - offset) as usize);
        source
            .read_exact_at(&mut source_block[..n], offset)
            .unwrap();
        copy.read_exact_at(&mut copy_block[..n], offset).unwrap();
        let pages = source_block[..n]
            .chunks(page)
            .zip(copy_block[..n].chunks(page));
        for (index, (want, have)) in pages.enumerate() {
            if want != have {
                copy.write_all_at(want, offset + (index * page) as u64)
                    .unwrap();
            }
        }
    }
    copy.sync_all().unwrap();
}

criterion_group! {
    name = benches;
    // Criterion plans iterations from wall time, including each commit's
    // untimed reset, so 10 s of measurement runs fewer commits than seals.
    config = Criterion::default()
        .sample_size(10)
        .noise_threshold(0.10) // Runs of identical code can differ by ~10%.
        .measurement_time(Duration::from_secs(10));
    targets = write
}
criterion_main!(benches);
