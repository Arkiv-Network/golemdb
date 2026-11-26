//! MDBX's own per-table statistics, to see where the disk space goes.
//!
//! golem-db's storage must not be opened twice in one process, so the harness
//! re-runs itself (`--table-stats <dir>`) as a separate read-only MDBX reader.
//! It runs between commits, outside every timer, and exits before the next one.

use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use libmdbx::{DatabaseOptions, Mode, NoWriteMap};

pub const HEADER: &str = "table,entries,depth,pages,mib";

/// Child process: one CSV line per table, then the file space outside the tables.
pub fn print(dir: &Path) -> Result<()> {
    let db = libmdbx::Database::<NoWriteMap>::open_with_options(
        dir,
        DatabaseOptions {
            mode: Mode::ReadOnly,
            max_tables: Some(128),
            ..Default::default()
        },
    )?;
    let page = db.stat()?.page_size() as u64;
    let mib = |pages: u64| (pages * page) as f64 / (1 << 20) as f64;
    let txn = db.begin_ro_txn()?;
    // The unnamed main table lists the named ones.
    let mut names = Vec::new();
    let main = txn.open_table(None)?;
    for item in txn.cursor(&main)?.iter::<Vec<u8>, ()>() {
        names.push(String::from_utf8(item?.0)?);
    }
    let mut in_tables = 0;
    for name in &names {
        let stat = txn.table_stat(&txn.open_table(Some(name))?)?;
        let pages = (stat.leaf_pages() + stat.branch_pages() + stat.overflow_pages()) as u64;
        in_tables += pages;
        println!(
            "{name},{},{},{pages},{:.3}",
            stat.entries(),
            stat.depth(),
            mib(pages)
        );
    }
    let used = db.info()?.last_pgno() as u64 + 1;
    let free = db.freelist()? as u64;
    let file = std::fs::metadata(dir.join("mdbx.dat"))?.len() / page;
    // free: pages released by earlier commits, waiting to be reused.
    // other: MDBX's meta pages, table catalog and free list.
    // unused: file space grown ahead of need, never written yet.
    for (name, pages) in [
        ("free", free),
        ("other", used.saturating_sub(in_tables + free)),
        ("unused", file.saturating_sub(used)),
        ("file", file),
    ] {
        println!("{name},,,{pages},{:.3}", mib(pages));
    }
    Ok(())
}

/// Runs `print` in a child process and returns its CSV lines.
pub fn snapshot(dir: &Path) -> Result<Vec<String>> {
    let out = Command::new(std::env::current_exe()?)
        .arg("--table-stats")
        .arg(dir)
        .output()?;
    if !out.status.success() {
        bail!(
            "table stats: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}
