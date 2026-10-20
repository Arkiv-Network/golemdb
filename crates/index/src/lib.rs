//! Transactional two-tier index, ordered terms, and canonical bitmap chunks.
//!
//! A record ID splits into a 48-bit path and a 16-bit offset. Persisted chunks
//! encode their six-byte big-endian path followed by canonical portable Roaring
//! bytes. Empty chunks are working state only; persistence removes their leaf.
//! [`Bitmap`] assembles chunks for queries, without owning storage or hashing.
//! [`IndexTerm`] supplies ordered table keys and term leaf hashes. All hashing
//! receives the same configured [`golemdb_merkle::HashProvider`] from the caller.
//! [`Index`] coordinates the flat term table, BitmapTrie, and IndexTrie inside
//! the caller's transaction. Trie adapters remain private implementation details.
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
//!
//! ```
//! use golemdb_cells::{CellType, Width};
//! use golemdb_index::{BitmapContainer, IndexTerm};
//! use golemdb_merkle::Keccak256Hasher;
//! let hasher = Keccak256Hasher;
//! let term = IndexTerm::new("Price", CellType::Int(Width::W4), &100i32.to_be_bytes())?;
//! let chunk = BitmapContainer::from_values(0, [64, 65])?;
//! // A singleton BitmapTrie's root is its only container's leaf hash.
//! let bitmap_root = chunk.leaf_hash(&hasher)?;
//! let index_leaf = term.leaf_hash(&bitmap_root, &hasher);
//! assert_ne!(bitmap_root, index_leaf);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod bitmap;
mod bitmap_trie;
pub mod container;
pub mod error;
mod index;
mod index_trie;
pub mod path;
pub mod tables;
mod term;
mod term_range;

pub use bitmap::Bitmap;
pub use container::BitmapContainer;
pub use error::{BitmapError, IndexError, Result};
pub use index::{Index, IndexUpdate, PostingChange, TermRootChange, TermScan};
pub use term::{IndexTerm, TermError};
pub use term_range::{RangeError, TermRange};

/// IndexTrie routes by a complete term digest (32 bytes).
/// This is a byte width, not the compressed trie's depth.
pub const INDEX_TRIE_PATH_BYTES: usize = size_of::<golemdb_merkle::Hash>();

/// BitmapTrie routes by the high 48 bits of a record ID (6 bytes).
/// A hexary path has two digits per byte, so its maximum depth is 12.
pub const BITMAP_TRIE_PATH_BYTES: usize = PATH_BITS as usize / 8;

/// Domain byte for index term leaf hashes.
///
/// Prepended to the full term routing hash and bitmap root:
/// `H(INDEX_LEAF_DOMAIN || trieKey || bitmapHash)`. The one-byte prefix
/// distinguishes this kind of hash input from cell leaves, bitmap containers,
/// and branch nodes, even if their remaining bytes happen to match.
/// Domain separation relies on the configured hash's collision resistance;
/// canonical field encoding is still required within each domain.
///
/// These assignments are part of the commitment format: changing them changes
/// leaf hashes and all roots that depend on them. The owning crate assigns the
/// domains; the generic Merkle trie receives its branch domain from the caller.
/// See [Domain Separation and Preimage Encoding](https://github.com/Arkiv-Network/golemdb/blob/main/docs/golemdb-design.md#domain-separation-and-preimage-encoding).
pub const INDEX_LEAF_DOMAIN: u8 = 0x02;

/// Domain byte for IndexTrie branch hashes.
///
/// Prepended to the canonical branch payload:
/// `H(INDEX_BRANCH_DOMAIN || prefix_len || prefix || state_mask || tree_mask || child_hashes)`.
/// It separates index branches from leaves and from branches of other tries,
/// which share the same structural encoding. Stored `leaf_paths` are excluded.
/// See [Domain Separation and Preimage Encoding](https://github.com/Arkiv-Network/golemdb/blob/main/docs/golemdb-design.md#domain-separation-and-preimage-encoding).
pub const INDEX_BRANCH_DOMAIN: u8 = 0x03;

/// Domain byte for bitmap container leaf hashes.
///
/// Prepended to the container's six-byte record-ID prefix and canonical Roaring
/// bytes: `H(BITMAP_LEAF_DOMAIN || hi48 || roaring)`. This identifies a bitmap
/// leaf independently of index leaves and branch nodes; including `hi48` binds
/// the container to its routing path.
/// See [Domain Separation and Preimage Encoding](https://github.com/Arkiv-Network/golemdb/blob/main/docs/golemdb-design.md#domain-separation-and-preimage-encoding).
pub const BITMAP_LEAF_DOMAIN: u8 = 0x04;

/// Domain byte for BitmapTrie branch hashes.
///
/// Prepended to the same canonical branch payload as [`INDEX_BRANCH_DOMAIN`],
/// but identifies a branch in the posting-list bitmap trie. Thus identical
/// branch payloads in the two trie kinds have distinct hash inputs.
/// See [Domain Separation and Preimage Encoding](https://github.com/Arkiv-Network/golemdb/blob/main/docs/golemdb-design.md#domain-separation-and-preimage-encoding).
pub const BITMAP_BRANCH_DOMAIN: u8 = 0x05;

pub(crate) const PATH_BITS: u8 = 48;
pub(crate) const PATH_BYTE_OFFSET: usize = 8 - BITMAP_TRIE_PATH_BYTES;
pub(crate) const MAX_PATH: u64 = (1 << PATH_BITS) - 1;
pub(crate) const CHUNK_BITS: u8 = 16;

#[cfg(test)]
mod tests;
