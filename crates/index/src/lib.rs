//! Transactional flat index, ordered terms, and canonical bitmap containers.
//!
//! A record ID splits into its top 48 bits, a [`Hi48`] naming its container,
//! and a 16-bit offset. Each term's containers are leaves of the
//! one index trie at `H(term) ‖ hi48`, so a term's postings are the subtree under its hash.
//! Containers are live rows of `Index`, keyed in term order and overwritten
//! in place like cells. [`Bitmap`] assembles containers for queries, without
//! owning storage or hashing. All hashing receives the same configured
//! [`golemdb_merkle::HashProvider`] from the caller.
//!
//! ```
//! use golemdb_cells::CellType;
//! use golemdb_index::{Index, IndexTerm, PostingChange};
//! use golemdb_merkle::{Keccak256Hasher, RootRef};
//! use golemdb_storage::{Store, MemoryStore, WriteTransaction};
//!
//! let store = MemoryStore::new();
//! let hasher = Keccak256Hasher;
//! let index = Index::new(&hasher);
//! let term = IndexTerm::new("color", CellType::Str, b"blue")?;
//! let mut tx = store.begin_write()?;
//! let update = index.apply(&mut tx, 1, RootRef::Empty, [
//!     PostingChange::Add { term: term.clone(), record_id: 42 },
//! ])?;
//! // The engine can write its head and history in this same transaction.
//! tx.commit()?;
//! let read = store.begin_read()?;
//! assert!(index.bitmap(&read, &term)?.unwrap().treemap().contains(42));
//! index.validate_root(&read, update.root)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod bitmap;
pub mod container;
pub mod error;
mod hi48;
mod index;
mod index_trie;
pub mod tables;
mod term;
mod term_range;

pub use bitmap::Bitmap;
pub use container::BitmapContainer;
pub use error::{BitmapError, IndexError, Result};
pub use hi48::Hi48;
pub use index::{Index, IndexUpdate, PostingChange, TermScan, term_exists};
pub use term::{IndexTerm, TermError};
pub use term_range::{RangeError, TermRange};

/// Bits of a record ID held by one container, as offsets below its [`Hi48`].
pub(crate) const OFFSET_BITS: u32 = 16;

/// The index trie routes a container by its term's digest, then its `hi48`.
/// This is a byte width, not the compressed trie's depth.
pub const INDEX_TRIE_PATH_BYTES: usize = size_of::<golemdb_merkle::Hash>() + Hi48::BYTES;

/// Domain byte for index trie branch hashes.
///
/// Prepended to the canonical branch payload:
/// `H(INDEX_BRANCH_DOMAIN || prefix_len || prefix || state_mask || tree_mask || child_hashes)`.
/// Stored `leaf_paths` are excluded.
pub const INDEX_BRANCH_DOMAIN: u8 = 0x03;

/// Domain byte for container leaf hashes:
/// `H(CONTAINER_LEAF_DOMAIN || H(term) || hi48 || roaring)`, binding the
/// leaf's complete trie path.
pub const CONTAINER_LEAF_DOMAIN: u8 = 0x04;

pub(crate) const MAX_OFFSET: u32 = (1 << OFFSET_BITS) - 1;

#[cfg(test)]
mod tests;
