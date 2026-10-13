//! Times `seal` and `commit` of record batches through the public `Api`, on an
//! MDBX database preloaded with `GOLEMDB_BENCH_RECORDS` records (default 10,000).

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, SystemTime};

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use golemdb_api::{Api, Database, ReadTarget, RecordOp};
use golemdb_integration_tests::workload::{Batch, Workload, execute, key, open, preload};
use tempfile::TempDir;

/// Where Criterion keeps its results, set explicitly so the summary can read them.
const RESULTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/criterion");

/// Runs of identical code can differ by ~10%, so smaller changes count as noise.
/// Criterion's confidence intervals cover only the variation within a run.
const NOISE: f64 = 0.10;

const CASES: [(Phase, &str, Batch); 6] = [
    // Batch::new(creates, patches, deletes)
    (Phase::Seal, "create", Batch::new(200, 0, 0)),
    (Phase::Seal, "patch", Batch::new(0, 200, 0)),
    (Phase::Seal, "delete", Batch::new(0, 0, 200)),
    (Phase::Seal, "mixed", Batch::new(70, 60, 70)),
    (Phase::Commit, "patch", Batch::new(0, 200, 0)),
    (Phase::Commit, "mixed", Batch::new(70, 60, 70)),
];

#[derive(Clone, Copy)]
enum Phase {
    /// The batch is discarded after sealing, so all seal cases share one database.
    Seal,
    /// Each commit case gets its own database. Its batches must keep the record
    /// count constant, or the database grows or shrinks over the iterations.
    Commit,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Seal => "seal",
            Self::Commit => "commit",
        }
    }
}

struct Fixture {
    db: Database,
    workload: Workload,
    // Must stay after `db`: the database has to close before its directory is deleted.
    _dir: TempDir,
}

impl Fixture {
    /// Creates the database under `target/` rather than `/tmp`, which is often
    /// RAM-backed and would hide the cost of writing to disk.
    fn build(records: u64) -> Self {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        std::fs::create_dir_all(target).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("golemdb-bench-")
            .tempdir_in(target)
            .unwrap();
        let db = open(dir.path()).unwrap();
        let workload = preload(&db, records).unwrap();
        let read = db.get(ReadTarget::Head, RecordOp::get(key(records / 2)));
        assert!(read.unwrap().cells.contains_key(b"payload".as_slice()));
        Self {
            db,
            workload,
            _dir: dir,
        }
    }

    fn seal(&self, batch: Batch) -> Duration {
        let (ops, _) = self.workload.plan(batch);
        let branch = execute(&self.db, ops).unwrap();
        let start = cpu_time();
        black_box(self.db.seal(branch).unwrap());
        let elapsed = cpu_time() - start;
        self.db.discard(branch).unwrap();
        elapsed
    }

    fn commit(&mut self, batch: Batch) -> Duration {
        let (ops, after) = self.workload.plan(batch);
        let branch = execute(&self.db, ops).unwrap();
        self.db.seal(branch).unwrap();
        let start = cpu_time();
        self.db.commit(branch).unwrap();
        let elapsed = cpu_time() - start;
        self.workload = after;
        elapsed
    }

    /// Bytes written per commit of `batch`, over the next 10 commits.
    fn written_per_commit(&mut self, batch: Batch) -> Option<u64> {
        let before = bytes_written()?;
        for _ in 0..10 {
            self.commit(batch);
        }
        Some((bytes_written()? - before) / 10)
    }
}

fn batches(c: &mut Criterion) {
    let started = SystemTime::now();
    let records: u64 = std::env::var("GOLEMDB_BENCH_RECORDS")
        .map(|v| v.parse().expect("GOLEMDB_BENCH_RECORDS must be a number"))
        .unwrap_or(10_000);
    // The fixture is built on first use, so runs filtered to other benchmarks
    // skip the preload.
    let mut shared: Option<Fixture> = None;
    // The record count is part of every case's name, so Criterion never compares
    // runs on databases of different sizes.
    let group_name = format!("{records} records");
    let mut group = c.benchmark_group(&group_name);
    // An iteration takes milliseconds, so a few equal samples are enough.
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .noise_threshold(NOISE);
    let mut written = Vec::new();
    for (phase, name, batch) in CASES {
        // With fewer records, the batch would quietly do fewer operations than listed.
        assert!(
            records >= batch.patches + batch.deletes,
            "GOLEMDB_BENCH_RECORDS is too small for the {name} batch"
        );
        let case = format!("{}/{name}", phase.name());
        let mut own: Option<Fixture> = None;
        group.bench_function(&case, |b| match phase {
            Phase::Seal => {
                let fixture = shared.get_or_insert_with(|| Fixture::build(records));
                b.iter_custom(|iters| (0..iters).map(|_| fixture.seal(batch)).sum())
            }
            Phase::Commit => {
                let fixture = own.get_or_insert_with(|| {
                    let mut fixture = Fixture::build(records);
                    // Measured before timing starts, because Criterion picks how many
                    // commits to time. This way every run measures the same commits
                    // and, with the same code, writes the same bytes.
                    let bytes = fixture.written_per_commit(batch);
                    written.extend(bytes.map(|bytes| (case.clone(), bytes)));
                    fixture
                });
                b.iter_custom(|iters| (0..iters).map(|_| fixture.commit(batch)).sum())
            }
        });
    }
    group.finish();
    summarize(&group_name, started, &written);
}

/// CPU time used by this process. Cases report it rather than elapsed time, so
/// waiting on the disk, which depends on the machine and whatever else is using
/// it, does not skew the results.
fn cpu_time() -> Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid `timespec` for the call to write into.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now) };
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

/// Bytes this process has passed to the OS to write, or `None` outside Linux.
fn bytes_written() -> Option<u64> {
    let io = std::fs::read_to_string("/proc/self/io").ok()?;
    io.lines()
        .find_map(|line| line.strip_prefix("wchar: "))?
        .parse()
        .ok()
}

/// Prints one line per case measured in this run: its time, and whether it got
/// slower or faster than in the run it is compared with. Then the same for the
/// bytes written per commit.
fn summarize(group: &str, started: SystemTime, written: &[(String, u64)]) {
    // Criterion stores the case `seal/patch` in the directory `seal_patch`.
    let dir = |case: &str| Path::new(RESULTS).join(group).join(case.replace('/', "_"));
    let written_since_start = |file: &Path| {
        std::fs::metadata(file)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified >= started)
    };
    let mut times = Vec::new();
    for (phase, name, _) in CASES {
        let case = format!("{}/{name}", phase.name());
        let (time, change) = (
            dir(&case).join("new/estimates.json"),
            dir(&case).join("change/estimates.json"),
        );
        if !written_since_start(&time) {
            continue;
        }
        let verdict = match written_since_start(&change).then(|| mean(&change)) {
            Some((change, low, _)) if low > NOISE => format!("{:.0}% slower", change * 100.0),
            Some((change, _, high)) if high < -NOISE => format!("{:.0}% faster", -change * 100.0),
            Some(_) => "no change".to_string(),
            None => "no earlier run to compare with".to_string(),
        };
        times.push(format!(
            "  {case:<14} {:>7.1} ms   {verdict}",
            mean(&time).0 / 1e6
        ));
    }

    // Criterion keeps each baseline in a directory of that name, next to `new`.
    // The bytes are stored in the same directory, so they are compared with the
    // same run as the times.
    let (baseline, replaced) = baseline();
    let mut bytes = Vec::new();
    for (case, now) in written {
        // Runs that save no times, such as `--test`, don't save bytes either.
        if !written_since_start(&dir(case).join("new/estimates.json")) {
            continue;
        }
        let file = dir(case).join(&baseline).join("written");
        let earlier = std::fs::read_to_string(&file).ok();
        let verdict = match earlier.and_then(|earlier| earlier.parse::<u64>().ok()) {
            Some(earlier) if earlier == *now => "no change".to_string(),
            Some(earlier) if earlier < *now => {
                format!("{:.1}% more", (*now as f64 / earlier as f64 - 1.0) * 100.0)
            }
            Some(earlier) => format!("{:.1}% less", (1.0 - *now as f64 / earlier as f64) * 100.0),
            None => "no earlier run to compare with".to_string(),
        };
        if replaced {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, now.to_string()).unwrap();
        }
        bytes.push(format!(
            "  {case:<14} {:>7.2} MiB  {verdict}",
            *now as f64 / (1024.0 * 1024.0)
        ));
    }

    let compared_with = if replaced {
        "the previous run".to_string()
    } else {
        format!("baseline `{baseline}`")
    };
    if !times.is_empty() {
        println!("\nSummary for {group}, compared with {compared_with}:");
        println!("{}", times.join("\n"));
    }
    if !bytes.is_empty() {
        println!("\nBytes written per commit, compared with {compared_with}:");
        println!("{}", bytes.join("\n"));
    }
}

/// The name of the baseline Criterion compares this run with, and whether this
/// run replaces it. `--baseline <name>` only compares; otherwise the run is saved
/// as `--save-baseline <name>`, or as `base`.
fn baseline() -> (String, bool) {
    let after = |flag: &str| std::env::args().skip_while(|arg| arg != flag).nth(1);
    match after("--baseline") {
        Some(name) => (name, false),
        None => (after("--save-baseline").unwrap_or("base".to_string()), true),
    }
}

/// The mean from a Criterion estimates file, with its 95% confidence interval.
fn mean(file: &Path) -> (f64, f64, f64) {
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let mean = &json["mean"];
    let bound = |key: &str| mean["confidence_interval"][key].as_f64().unwrap();
    let point = mean["point_estimate"].as_f64().unwrap();
    (point, bound("lower_bound"), bound("upper_bound"))
}

criterion_group! {
    name = benches;
    // Results vary more between runs than within one, so a few seconds per case
    // are enough. `--warm-up-time` and `--measurement-time` override these.
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2))
        .output_directory(Path::new(RESULTS));
    targets = batches
}
criterion_main!(benches);
