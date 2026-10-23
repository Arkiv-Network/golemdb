//! Pruning: discarding what only commits before a node's retention window reach.
//!
//! [`Pruner::register`] and [`Pruner::sweep`] run inside each commit's
//! transaction. They garbage-collect superseded trie rows: a row superseded at
//! commit `s` is reached only by roots before `s`, so it may be deleted once
//! `s ≤ head − window`. Due rows are deleted in batches, sorted by table and
//! key, so rows sharing a page are deleted together. [`Pruner::drop_before`]
//! drops a caller's own rows keyed by commit, such as `Roots`, outside the
//! window.

mod error;
mod gc;

pub use error::{PruningError, Result};
pub use gc::STALE;

use std::ops::Bound;

use golemdb_storage::{Table, WriteTransaction, scan};

/// Due rows a sweep waits for before deleting a batch, so each batch is large
/// enough for rows sharing a page to be deleted together.
pub const MIN_BATCH_ROWS: usize = 75_000;

/// Most rows one sweep deletes, bounding the commit that carries a batch.
pub const MAX_BATCH_ROWS: usize = 150_000;

/// Commits a due row waits at most: past this, a sweep deletes a smaller batch,
/// so a quiet node still drains.
pub const MAX_WAIT_COMMITS: u64 = 50;

/// Node-local pruning settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruningConfig {
    window: u64,
}

impl PruningConfig {
    /// Keep every root of the last `window` commits, and the head, readable.
    pub const fn new(window: u64) -> Self {
        Self { window }
    }

    /// Commits whose roots stay readable.
    pub const fn window(&self) -> u64 {
        self.window
    }
}

/// Prunes inside each commit's transaction, by its configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pruner {
    config: PruningConfig,
}

impl Pruner {
    /// A pruner with `config`.
    pub const fn new(config: PruningConfig) -> Self {
        Self { config }
    }

    /// Register the rows `commit` superseded, to delete once they leave the
    /// window.
    pub fn register<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
    ) -> Result<()> {
        gc::register(tx, commit, superseded)
    }

    /// Delete the oldest rows superseded at or before `head − window`, at most
    /// [`MAX_BATCH_ROWS`], once at least [`MIN_BATCH_ROWS`] are due or the
    /// oldest has been due for [`MAX_WAIT_COMMITS`] commits; otherwise delete
    /// nothing. Commits that delete a batch take longer than the others.
    pub fn sweep(&self, tx: &mut impl WriteTransaction, head: u64) -> Result<()> {
        let batch = gc::Batch {
            min: MIN_BATCH_ROWS,
            max: MAX_BATCH_ROWS,
            max_wait: MAX_WAIT_COMMITS,
        };
        gc::sweep(tx, self.cutoff(head), batch)
    }

    /// Delete every row of `table` keyed by a big-endian commit before
    /// `cutoff(head)`, with no limit: a window shrink drops them all in one
    /// commit.
    pub fn drop_before(
        &self,
        tx: &mut impl WriteTransaction,
        table: Table,
        head: u64,
    ) -> Result<()> {
        let upper = Bound::Excluded(self.cutoff(head).to_be_bytes().to_vec());
        let keys = scan(tx, table, Bound::Unbounded, upper)?
            .map(|row| row.map(|(key, _)| key))
            .collect::<golemdb_storage::Result<Vec<_>>>()?;
        for key in keys {
            tx.delete(table, &key)?;
        }
        Ok(())
    }

    /// Commits before this may be pruned: `head − window`. Not the earliest
    /// retained commit, which can be later after the window grows.
    pub const fn cutoff(&self, head: u64) -> u64 {
        head.saturating_sub(self.config.window)
    }
}

#[cfg(test)]
mod tests {
    use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction, scan_prefix};

    use crate::{Pruner, PruningConfig};

    /// Rows keyed by commits before the cutoff go, the rest stay.
    #[test]
    fn drop_before_keeps_the_window() {
        let db = MemoryStore::new();
        let table = Table("PerCommit");
        let mut tx = db.begin_write().unwrap();
        for commit in 1..=5u64 {
            tx.put(table, &commit.to_be_bytes(), b"row").unwrap();
        }
        Pruner::new(PruningConfig::new(2))
            .drop_before(&mut tx, table, 5)
            .unwrap();
        let kept: Vec<_> = (scan_prefix(&tx, table, vec![]).unwrap())
            .map(|row| row.unwrap().0)
            .collect();
        let window: Vec<_> = (3..=5u64).map(|c| c.to_be_bytes().to_vec()).collect();
        assert_eq!(kept, window);
    }
}
