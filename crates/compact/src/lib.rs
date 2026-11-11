//! Offline pruning in place: delete from a stopped node's store what a node
//! pruning with a given window would no longer hold.
//!
//! [`prune`] marks the rows the retained roots reach, listing in `Stale` those
//! the head does not, then deletes the `Roots` before the window, every
//! unmarked trie row, and the due `Stale` listings. Bitmap rows are kept for
//! the head only, as no read reaches a term's earlier roots. An interrupted
//! run leaves a valid store; rerunning finishes the job.
//!
//! ```
//! use golemdb_compact::{PruneOptions, prune};
//! use golemdb_pruning::PruningConfig;
//! use golemdb_storage::MemoryStore;
//!
//! let (store, marks) = (MemoryStore::new(), MemoryStore::new());
//! let options = PruneOptions::new(PruningConfig::new(10));
//! // A store without a genesis is refused.
//! assert!(prune(&store, &marks, &options).is_err());
//! ```

mod mark;
mod verify;
mod write;

use std::ops::Bound;

use golemdb_branch::{BranchError, ROOTS};
use golemdb_cells::tables::CELL_TRIE;
use golemdb_index::tables::{BITMAP_CONTAINER, BITMAP_TRIE, INDEX_TRIE};
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher, MerkleError};
use golemdb_pruning::{PruningConfig, PruningError, STALE};
use golemdb_storage::{ReadTransaction, StorageError, Store, Table, scan};

use mark::mark;
use verify::verify_head;
use write::{Writer, delete};

const SUPERBLOCK: Table = Table("Superblock");

const TRIES: [Table; 4] = [
    CELL_TRIE.table(),
    INDEX_TRIE.table(),
    BITMAP_TRIE.table(),
    BITMAP_CONTAINER.table(),
];

/// Why [`prune`] failed.
#[derive(Debug, thiserror::Error)]
pub enum PruneError {
    /// A store failed to read or write.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A trie row failed to decode or its hash mismatched.
    #[error(transparent)]
    Merkle(#[from] MerkleError),
    /// The head or a retained root is unreadable.
    #[error(transparent)]
    Branch(#[from] BranchError),
    /// Listing a row in `Stale` failed.
    #[error(transparent)]
    Pruning(#[from] PruningError),
    /// The marks store is not empty.
    #[error("the marks store is not empty")]
    MarksNotEmpty,
    /// The store's `hash_fn` is missing or unknown.
    #[error("the store has no valid hash function: {0:?}")]
    HashFunction(Option<Vec<u8>>),
    /// The window is below the store's `#minRetention`.
    #[error("window {window} is less than #minRetention {min}")]
    BelowMinRetention { window: u64, min: u64 },
    /// A row is malformed.
    #[error("corrupt store: {0}")]
    Corrupt(&'static str),
    /// A reachable row is missing.
    #[error("{table} row of commit {commit} is reachable but missing")]
    MissingRow { table: &'static str, commit: u64 },
}

/// The result of [`prune`].
pub type Result<T> = std::result::Result<T, PruneError>;

/// How [`prune`] prunes.
#[derive(Clone, Copy, Debug)]
pub struct PruneOptions {
    config: PruningConfig,
    batch_rows: usize,
}

impl PruneOptions {
    /// Prune to what a node with `config` keeps, 100,000 rows per transaction.
    pub const fn new(config: PruningConfig) -> Self {
        Self {
            config,
            batch_rows: 100_000,
        }
    }

    /// Rows written or deleted per transaction, at least one.
    pub const fn batch_rows(self, rows: usize) -> Self {
        Self {
            batch_rows: if rows == 0 { 1 } else { rows },
            ..self
        }
    }
}

/// What [`prune`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The store's head commit.
    pub head: u64,
    /// The earliest commit whose roots the store holds.
    pub earliest: u64,
    /// Trie rows kept.
    pub kept: u64,
    /// Kept trie rows listed in `Stale`.
    pub listed: u64,
    /// Trie rows deleted.
    pub deleted: u64,
}

/// Prune `store` in place to what a node pruning with `options`' window keeps.
/// `marks` is an empty scratch store. `store` must not be open elsewhere, or
/// rows a concurrent commit writes are deleted. A store without `#minRetention`,
/// or a window below it, is refused.
pub fn prune(store: &impl Store, marks: &impl Store, options: &PruneOptions) -> Result<Report> {
    let hash_fn = store.begin_read()?.get(SUPERBLOCK, b"hash_fn")?;
    match hash_fn.as_deref() {
        Some([0, 1]) => prune_with(store, marks, options, &Keccak256Hasher),
        Some([0, 2]) => prune_with(store, marks, options, &Blake3Hasher),
        _ => Err(PruneError::HashFunction(hash_fn)),
    }
}

fn prune_with<H: HashProvider>(
    store: &impl Store,
    marks: &impl Store,
    options: &PruneOptions,
    hasher: &H,
) -> Result<Report> {
    let batch = options.batch_rows;
    let (mut report, cutoff) = mark(store, marks, options, hasher)?;

    let mut out = Writer::new(store, batch)?;
    for row in scan(
        &marks.begin_read()?,
        STALE,
        Bound::Unbounded,
        Bound::Unbounded,
    )? {
        out.put(STALE, &row?.0, &[])?;
    }
    out.finish()?;
    // Roots first, so no root names a deleted row.
    let before_window = Bound::Excluded(cutoff.to_be_bytes().to_vec());
    delete(store, ROOTS, before_window, batch, |_| Ok(true))?;
    let marked = marks.begin_read()?;
    for table in TRIES {
        report.deleted += delete(store, table, Bound::Unbounded, batch, |key| {
            Ok(marked.get(table, key)?.is_none())
        })?;
    }
    let due = (cutoff.checked_add(1)).map_or(Bound::Unbounded, |next| {
        Bound::Excluded(next.to_be_bytes().to_vec())
    });
    delete(store, STALE, due, batch, |_| Ok(true))?;
    verify_head(store, hasher)?;
    Ok(report)
}
