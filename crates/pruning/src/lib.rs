//! Pruning: discarding what only commits before a node's retention window reach.
//!
//! [`Pruner::record`] and [`Pruner::sweep`] run inside each commit's
//! transaction. They garbage-collect superseded trie rows: a row superseded at
//! commit `s` is reached only by roots before `s`, so it may be deleted once
//! `s ≤ head − window`; rows are deleted by epochs of [`EPOCH_COMMITS`]
//! commits. The caller drops its own per-commit rows, such as `Roots`, before
//! [`Pruner::cutoff`].

mod error;
mod gc;

pub use error::{PruningError, Result};
pub use gc::STALE;

use std::sync::{Mutex, PoisonError};

use golemdb_storage::{Table, WriteTransaction};

/// Commits per epoch. An epoch's superseded rows are deleted together, sorted
/// by table and key, so rows sharing a page are deleted together.
pub const EPOCH_COMMITS: u64 = 50;

/// Fewest rows a sweep deletes of a due epoch, so quiet commits still drain it.
pub const MIN_SWEPT_ROWS: usize = 1024;

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
#[derive(Debug)]
pub struct Pruner {
    config: PruningConfig,
    /// The due epoch being drained, sorted once; rebuilt from `Stale` after a
    /// restart or a commit that did not land.
    drain: Mutex<Option<gc::Drain>>,
}

impl Pruner {
    /// A pruner with `config`.
    pub const fn new(config: PruningConfig) -> Self {
        Self {
            config,
            drain: Mutex::new(None),
        }
    }

    /// Record the rows `commit` superseded, to delete once their epoch leaves
    /// the window; returns how many.
    pub fn record<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
    ) -> Result<usize> {
        gc::record(tx, commit, superseded)
    }

    /// Delete the next slice of the oldest due epoch: twice the `recorded` rows
    /// of this commit, at least [`MIN_SWEPT_ROWS`]. An epoch of
    /// [`EPOCH_COMMITS`] commits is due once its last commit is at or before
    /// `head − window`. Deleting twice what each commit records drains an epoch
    /// in about half an epoch, keeping up with recording with room for a
    /// backlog. Call it once per commit, with that commit's own number as
    /// `head`: the cached epoch is reused only by the next commit.
    pub fn sweep(&self, tx: &mut impl WriteTransaction, head: u64, recorded: usize) -> Result<()> {
        let mut drain = self.drain.lock().unwrap_or_else(PoisonError::into_inner);
        let slice = recorded.saturating_mul(2).max(MIN_SWEPT_ROWS);
        gc::sweep(
            tx,
            &mut drain,
            head,
            self.cutoff(head),
            EPOCH_COMMITS,
            slice,
        )
    }

    /// Commits before this may be pruned: `head − window`. Not the earliest
    /// retained commit, which can be later after the window grows.
    pub const fn cutoff(&self, head: u64) -> u64 {
        head.saturating_sub(self.config.window)
    }
}
