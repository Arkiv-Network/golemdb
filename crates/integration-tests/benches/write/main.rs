//! Times `seal` and `commit` of record batches on an MDBX database with
//! 100,000 records.

#[path = "../common/mod.rs"]
mod common;
mod workload;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};
use std::{env, fmt};

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use golemdb_api::{Api, BranchId, Database, OpenMode};
use golemdb_storage::{Store, StoreStats};
use golemdb_storage_mdbx::{MdbxOptions, MdbxStore};
use tempfile::TempDir;

use common::{bench_dir, config};
use workload::{Batch, Workload};

const RECORDS: u64 = 100_000;
const PRELOAD_BATCH: u64 = 5_000;
const _: () = assert!(RECORDS.is_multiple_of(PRELOAD_BATCH));
/// Commits after preloading, so cases run on a database with a history rather
/// than a freshly loaded one.
const HISTORY: u64 = 200;

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
        let db = Database::from_store(store.clone(), &config().with_mode(mode)).unwrap();
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

    fn commit_storage(mut self, batch: Batch) -> Storage {
        let before = self.stats();
        self.commit(batch);
        let after = self.stats();
        Storage {
            written: after.session_bytes_written - before.session_bytes_written,
            syncs: after.session_syncs - before.session_syncs,
            db_size: after.db_size,
        }
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

fn write(c: &mut Criterion) {
    let prefix = format!("write/{}k", RECORDS / 1000);
    let mut group = c.benchmark_group(&prefix);
    group.sampling_mode(SamplingMode::Flat);
    // Criterion calls each closure many times, and a filtered run may call none.
    let mut snapshot: Option<Snapshot> = None;
    let mut storages = Vec::new();

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
            storages.push((format!("{prefix}/commit/{name}"), storage));
        }
    }
    group.finish();
    if let Ok(path) = env::var("BENCH_STORAGE_JSON") {
        fs::write(path, Storage::json(&storages)).unwrap();
    }
}

/// Storage work of one commit, and the database size after it.
struct Storage {
    written: u64,
    syncs: u64,
    db_size: u64,
}

impl Storage {
    /// The `customSmallerIsBetter` format of github-action-benchmark.
    fn json(storages: &[(String, Storage)]) -> String {
        let entries: Vec<String> = storages
            .iter()
            .flat_map(|(name, storage)| {
                [
                    ("written", "bytes", storage.written),
                    ("syncs", "syncs", storage.syncs),
                ]
                .map(|(metric, unit, value)| {
                    format!(r#"{{"name":"{name} {metric}","unit":"{unit}","value":{value}}}"#)
                })
            })
            .collect();
        format!("[{}]", entries.join(","))
    }
}

impl fmt::Display for Storage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:.2} MiB written and {} sync(s) per commit, database {:.0} MiB",
            self.written as f64 / MIB,
            self.syncs,
            self.db_size as f64 / MIB,
        )
    }
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
