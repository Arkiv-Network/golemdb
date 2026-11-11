//! Prune a MDBX store in place, removing older history and compacting the store file.

use std::{
    error::Error,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use golemdb_compact::{PruneOptions, prune};
use golemdb_pruning::PruningConfig;
use golemdb_storage_mdbx::{MdbxOptions, MdbxStore};

/// Prune a stopped node's MDBX store in place to what a node pruning with
/// `--retention-window` keeps, then compact it.
#[derive(Parser)]
#[command(name = "golemdb-compact")]
struct Args {
    /// The store's directory.
    store: PathBuf,
    /// Commits before the head whose roots are kept.
    #[arg(long, value_name = "COMMITS")]
    retention_window: u64,
    /// Skip compacting: the freed pages are left for later writes.
    #[arg(long)]
    prune_only: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("golemdb-compact: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let path = args.store;
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()).into());
    }
    let data = path.join("mdbx.dat");
    // Inside the store, so on its file system; removed after the store closes.
    let scratch = tempfile::Builder::new()
        .prefix("compact-")
        .tempdir_in(&path)?;
    // A store past the size limit opens, but writes fail.
    let defaults = MdbxOptions::default();
    let size = std::fs::metadata(&data)?.len();
    let room = usize::try_from(size)? + defaults.growth_step;
    let options = MdbxOptions {
        exclusive: true,
        max_map_size: defaults.max_map_size.max(room),
        ..defaults
    };
    let store = MdbxStore::open_with_options(&path, options)?;

    let options = PruneOptions::new(PruningConfig::new(args.retention_window));
    let report = prune(
        &store,
        &MdbxStore::open(scratch.path().join("marks"))?,
        &options,
    )?;
    println!(
        "head {}, roots kept from {}: {} trie rows kept, {} listed as stale, {} deleted",
        report.head, report.earliest, report.kept, report.listed, report.deleted
    );
    if args.prune_only {
        return Ok(());
    }

    // Swapped while open, so no node opens the store in between.
    let copy = scratch.path().join("mdbx.dat");
    store.copy_compact(&copy)?;
    keep_access(&data, &copy)?;
    std::fs::rename(&copy, &data)?;
    let after = std::fs::metadata(&data)?.len();
    println!("data file compacted from {size} to {after} bytes");
    Ok(())
}

fn keep_access(data: &Path, copy: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(data)?;
    std::fs::set_permissions(copy, metadata.permissions())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = std::fs::metadata(copy)?;
        if (current.uid(), current.gid()) != (metadata.uid(), metadata.gid()) {
            std::os::unix::fs::chown(copy, Some(metadata.uid()), Some(metadata.gid()))?;
        }
    }
    Ok(())
}
