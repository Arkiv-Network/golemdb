//! Reversible cell working state for GolemDB write branches.
//!
//! [`Branches`] opens head-only branches and validates IDs for every read,
//! write, checkpoint, rollback, seal, commit, discard, and `branch_info` call. Each operation uses one storage
//! snapshot for both its head check and cell reads. The manager is cloneable;
//! clones share the registry, with operations serialized per branch.
//!
//! The private cell overlay keeps puts and tombstones in memory, reads through
//! to the validated origin snapshot, and groups changes into atomic operations
//! and checkpoint frames. Public [`CellRead`] and [`CellWrite`] views provide
//! access only within manager callbacks. Scans match encoded cell-key prefixes.
//! Seal applies the final cell diff and derived index postings to buffered
//! storage, computes both roots, and freezes the branch without durable writes.
//!
//! Genesis initialization, history, and database rewind are not implemented yet.
//! Record validation, bindings, and allocation belong to record operations;
//! their cell writes participate in the same undo journal as ordinary cells.
//!
//! # Guarded branch access
//!
//! The store must already have an initialized `Superblock/head`. This layer
//! never fabricates genesis. Reads return owned values or collect scans inside
//! the callback so they cannot outlive the head check's snapshot.
//!
//! ```
//! use golemdb_branch::{Branches, BranchError};
//! use golemdb_cells::{CellKey, CellNameRef, CellValue};
//! use golemdb_storage::Store;
//! use golemdb_merkle::Keccak256Hasher;
//!
//! fn edit<S: Store>(store: S) -> Result<(), Box<dyn std::error::Error>> {
//!     let branches = Branches::new(store, Keccak256Hasher)?;
//!     let branch = branches.begin()?;
//!     let key = CellKey::new(64, CellNameRef::parse_user(b"status", 64)?);
//!     branches.write(branch, |cell_writer| {
//!         cell_writer.put(key.clone(), CellValue::parse(b"\x02ready".to_vec())?);
//!         Ok::<_, BranchError>(())
//!     })?;
//!     branches.checkpoint(branch)?;
//!     let info = branches.branch_info(branch)?;
//!     assert_eq!(info.branch_id, branch);
//!     assert_eq!(info.version, 1);
//!     assert!(!info.sealed);
//!     let value = branches.read(branch, |cell_reader| cell_reader.get(&key))?;
//!     assert_eq!(value.unwrap().as_str(), Some("ready"));
//!     let cells = branches.read(branch, |cell_reader| {
//!         cell_reader.scan_prefix(&64u64.to_be_bytes())?.collect::<Result<Vec<_>, _>>()
//!     })?;
//!     assert_eq!(cells.len(), 1);
//!     branches.discard(branch)?;
//!     Ok(())
//! }
//! # use golemdb_storage::{MemoryStore, Table, WriteTransaction};
//! # let store = MemoryStore::new();
//! # let mut tx = store.begin_write()?;
//! # // Minimal head fixture, not a production genesis initializer.
//! # tx.put(Table("Superblock"), b"head", &[0; 72])?;
//! # tx.commit()?;
//! # edit(store)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!

//! # Sealing
//!
//! Supply the deployment's hash provider when constructing the manager. A seal
//! reopens both roots from the validated origin, adds the previous commit's
//! `#roots` cell, applies cells, derives postings from actual before/after values,
//! and applies the index. No store writer is opened. History is deferred.
//! The result retains both updates and physical rows for commit.
//!
//! ```
//! use golemdb_branch::Branches;
//! use golemdb_merkle::{HashProvider, Keccak256Hasher};
//! use golemdb_storage::{Store, MemoryStore, Table, WriteTransaction};
//!
//! let store = MemoryStore::new();
//! let hash = Keccak256Hasher;
//! // Minimal empty-head fixture, not a production genesis initializer.
//! let mut tx = store.begin_write()?;
//! let empty = hash.hash(&[]);
//! tx.put(Table("Superblock"), b"head", &[0u64.to_be_bytes().as_slice(), &empty, &empty].concat())?;
//! tx.commit()?;
//! let branches = Branches::new(store, hash)?;
//! let branch = branches.begin()?;
//! let sealed = branches.seal(branch)?;
//! assert_eq!(sealed.commit_id.get(), 1);
//! assert_eq!(sealed.cells.changed_cells.len(), 1); // The lag-one #roots cell.
//! assert!(sealed.index.changed_terms.is_empty());
//! assert!(branches.branch_info(branch)?.sealed);
//! assert_eq!(branches.head()?.get(), 0); // Seal does not publish.
//! assert_eq!(branches.commit(branch)?.get(), 1);
//! assert_eq!(branches.head()?.get(), 1);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//! Repeated seal calls return the cached result after another head check.
//! Sealed branches allow cell reads but reject writes, checkpoints, and rollback.
//! The `#roots` cell added by seal becomes visible only after commit. Metadata,
//! commit, and discard remain available. An error or unwinding panic during
//! computation leaves the branch open with its overlay, version, and frames unchanged.

//! # Committing
//!
//! `commit(branch)` seals an open branch automatically, or reuses its existing
//! seal. It opens one storage writer, rechecks head, replays the buffered rows,
//! and advances head atomically. A successful commit consumes the branch ID;
//! other branches over the old head become stale. A reader already holding a
//! committed snapshot continues to see that snapshot.
//!
//! A seal error leaves the branch open. A storage error after sealing preserves
//! the sealed result for retry or discard; every retry validates head again.
//! The manager does not recompute roots for a sealed branch. History and
//! change-set tables remain deferred; record-layer allocator/binding writes
//! already staged as cells are persisted with all other sealed rows.
//!
//! # Callback errors and panics
//!
//! A [`Branches::write`] callback returning `Err` restores the overlay, version,
//! and checkpoint state from before that call. An unwinding panic escaping the
//! callback does the same, then propagates with its original payload after the
//! branch lock is released without poisoning it. Earlier successful writes remain
//! available if the caller catches the panic. Read callback panics also propagate
//! without poisoning the branch lock. Subsequent calls still validate head.
//!
//! Recovery covers branch state, not external side effects, and requires panic
//! unwinding and successful cleanup. See [`Branches`] for backend panic limits
//! and [`Branches::commit`] for panics during publication.

mod buffer;
mod commit;
mod error;
mod head;
mod journal;
mod manager;
mod overlay;
mod roots;
mod scan;
mod seal;
mod types;

pub use error::{BranchError, OperationError, Result};
pub use head::{Head, read_head, read_head_state, write_head};
pub use manager::Branches;
pub use overlay::{CellRead, CellWrite};
pub use roots::{Roots, read_roots, root_commits, write_roots};
pub use scan::CellScan;
pub use seal::SealedCommit;
pub use types::{BranchId, BranchInfo, CommitId};

#[cfg(test)]
#[path = "tests/buffer.rs"]
mod buffer_tests;
#[cfg(test)]
#[path = "tests/model.rs"]
mod model_tests;
#[cfg(test)]
#[path = "tests/overlay.rs"]
mod overlay_tests;

#[cfg(test)]
#[path = "tests/lifecycle.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "tests/publication.rs"]
mod publication_tests;

#[cfg(test)]
#[path = "tests/synchronization.rs"]
mod test_sync;

#[cfg(test)]
#[path = "tests/concurrency.rs"]
mod concurrency_tests;
