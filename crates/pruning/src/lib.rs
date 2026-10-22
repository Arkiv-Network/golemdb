//! Pruning: discarding what only commits before a node's retention window reach.
//!
//! [`Pruner::record`] and [`Pruner::sweep`] run inside each commit's
//! transaction. They garbage-collect superseded trie rows: a row superseded at
//! commit `s` is reached only by roots before `s`, so it is deleted once
//! `s ≤ head − window`. The caller drops its own per-commit rows, such as
//! `Roots`, before [`Pruner::cutoff`].

mod error;
mod gc;

pub use error::{PruningError, Result};
pub use gc::STALE;

use golemdb_storage::{Table, WriteTransaction};

/// Most commits whose superseded rows one sweep deletes, each in full. Normally
/// one is due per commit; after the window shrinks, the backlog drains over
/// several commits.
pub const MAX_SWEPT_COMMITS: usize = 10;

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

    /// Record the rows `commit` superseded, to delete once they leave the window.
    pub fn record<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
    ) -> Result<()> {
        gc::record(tx, commit, superseded)
    }

    /// Delete recorded rows superseded at or before `head − window`, those of
    /// at most [`MAX_SWEPT_COMMITS`] commits, so a shrunk window drains
    /// gradually.
    pub fn sweep(&self, tx: &mut impl WriteTransaction, head: u64) -> Result<()> {
        gc::sweep(tx, self.cutoff(head), MAX_SWEPT_COMMITS)
    }

    /// Commits before this may be pruned: `head − window`. Not the earliest
    /// retained commit, which can be later after the window grows.
    pub const fn cutoff(&self, head: u64) -> u64 {
        head.saturating_sub(self.config.window)
    }
}
