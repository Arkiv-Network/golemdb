//! Pruning: discarding what only commits before a node's retention window reach.
//!
//! [`Pruner::prune`] runs inside each commit's transaction. It garbage-collects
//! superseded trie rows: a row superseded at commit `s` is reached only by roots
//! before `s`, so it is deleted once `s ≤ head − window`. The caller drops its
//! own per-commit rows, such as `Roots`, before [`Pruner::cutoff`].

mod error;
mod gc;

pub use error::{PruningError, Result};
pub use gc::STALE;

use golemdb_storage::{Table, WriteTransaction};

/// Node-local pruning settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruningConfig {
    window: u64,
    batch: u64,
    max_deletes: usize,
    chunk: usize,
}

impl PruningConfig {
    /// Keep every root of the last `window` commits, and the head, readable.
    pub const fn new(window: u64) -> Self {
        Self {
            window,
            batch: 50,
            max_deletes: 100_000,
            chunk: 1000,
        }
    }

    /// Commits per epoch, whose rows are deleted together; at least 1.
    pub const fn with_batch(mut self, batch: u64) -> Self {
        self.batch = if batch == 0 { 1 } else { batch };
        self
    }

    /// Most rows one commit deletes; at least 1.
    pub const fn with_max_deletes(mut self, max_deletes: usize) -> Self {
        self.max_deletes = if max_deletes == 0 { 1 } else { max_deletes };
        self
    }

    /// Entries per `Stale` row; at least 1.
    pub const fn with_chunk(mut self, chunk: usize) -> Self {
        self.chunk = if chunk == 0 { 1 } else { chunk };
        self
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

    /// The settings this pruner runs with.
    pub const fn config(&self) -> &PruningConfig {
        &self.config
    }

    /// Prune inside the commit's transaction: record the rows `commit` superseded,
    /// then delete what has left the window.
    pub fn prune<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
    ) -> Result<()> {
        gc::collect(tx, &self.config, commit, superseded)
    }

    /// Commits before this may be pruned: `head − window`. Not the earliest
    /// retained commit, which can be later after the window grows.
    pub const fn cutoff(&self, head: u64) -> u64 {
        head.saturating_sub(self.config.window)
    }
}
