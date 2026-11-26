//! Load benchmarks for Golem DB: one scenario per run, results as files.
//! Usage: golemdb-bench <scenario.toml> --out <dir> --db-dir <dir>

mod config;
mod generate;
mod queries;
mod summary;
mod tables;
mod workloads;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use config::Scenario;
use golemdb_api::{
    Database, Genesis, MdbxOptions, OpenConfig, PricingConfig, PruningConfig, RecordLimits,
};
use golemdb_storage_mdbx::MdbxStore;

const USAGE: &str = "usage: golemdb-bench <scenario.toml> --out <dir> --db-dir <dir>";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, dir] = args.as_slice()
        && flag == "--table-stats"
    {
        return tables::print(Path::new(dir));
    }
    let (mut path, mut out, mut db_dir) = (None, None, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = Some(args.next().context(USAGE)?),
            "--db-dir" => db_dir = Some(args.next().context(USAGE)?),
            _ if path.is_none() && !arg.starts_with("--") => path = Some(arg),
            _ => bail!(USAGE),
        }
    }
    let (Some(path), Some(out), Some(db_dir)) = (path, out, db_dir) else {
        bail!(USAGE)
    };
    let mut scenario = Scenario::load(Path::new(&path))?;
    scenario.out = PathBuf::from(out);
    scenario.db_dir = PathBuf::from(db_dir);
    std::fs::create_dir_all(&scenario.out)?;
    // A run that fails must not leave an earlier run's summary beside its own files.
    match std::fs::remove_file(scenario.out.join(summary::FILE)) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    run(&scenario)
}

fn run(scenario: &Scenario) -> Result<()> {
    let dir = scenario.db_dir.as_path();
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("deleting {}", dir.display()));
        }
        _ => {}
    }
    std::fs::create_dir_all(dir)?;
    let mut config = OpenConfig::new(Genesis::new(
        scenario.database.hash,
        scenario.database.limits,
        // Load tests measure engine work, not the per-record caps.
        RecordLimits {
            max_cells: u64::MAX,
            max_indexed_cells: u64::MAX,
        },
        None,
        // Nothing is charged: every operation runs with an unlimited budget.
        PricingConfig::zero(),
    ));
    if let Some(window) = scenario.database.pruning_window {
        config = config.with_pruning(PruningConfig::new(window));
    }
    let options = MdbxOptions {
        max_map_size: (scenario.database.max_map_size_gib << 30) as usize,
        ..Default::default()
    };
    // Opened here rather than by `Database`, to read MDBX's write counters.
    let store = MdbxStore::open_with_options(dir, options)?;
    let db = Database::from_store(store.clone(), &config)?;

    println!("{}: {}", scenario.name, scenario.description);
    // `elapsed_ms` in the commit log counts from here.
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let outcome = workloads::run(&db, &store, scenario)?;
    drop(db);
    drop(store);
    summary::write(scenario, &outcome, started)
}

pub(crate) fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// The smallest value at or above the `q` quantile.
pub(crate) fn quantile(mut values: Vec<f64>, q: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    let rank = (q * values.len() as f64).ceil() as usize;
    values[rank.clamp(1, values.len()) - 1]
}
