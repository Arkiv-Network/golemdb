//! Reversible cell working state, immutable-data staging and coordinated publication.
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
//!     let branches = Branches::new(store, Keccak256Hasher, None)?;
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
//! let branches = Branches::new(store, hash, None)?;
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

//! # Immutable-data staging
//!
//! [`Branches::new`] takes optional
//! [`ImmutableDataStorage`](golemdb_immutable_data::ImmutableDataStorage).
//! `None` disables immutable data. The caller opens memory or file storage with
//! declarations and genesis identity matching the initialized store, then transfers
//! ownership to the manager for staging and publication. The manager is fully
//! configured before it can be shared or used. Append/truncate only change staging;
//! they perform no file or KV writes.
//!
//! [`Branches::immutable_data_append`] owns a complete typed row and optional key,
//! returning a provisional segment ordinal. Its first staged ordinal is the ending ordinal from
//! the latest published commit range at or before the branch origin, resolved
//! lazily in the same snapshot as the head check. Empty commit gaps need no entries.
//! Published keys and this branch's pending keys cannot be reused. Separate
//! competing branches may stage the same key/ordinal; losing branches become stale.
//!
//! [`Branches::immutable_data_truncate`] removes a staged suffix and releases its
//! keys and ordinals. Both operations work before and after seal, leave the sealed
//! roots unchanged, and never participate in cell checkpoints, rollback, or version
//! accounting. Errors leave the pending batch unchanged. Discard and stale-handle
//! invalidation release the batch with the rest of the branch state.
//!
//! # Immutable-data publication and recovery
//!
//! The constructor recovers the supplied immutable storage against the initialized
//! store's durable head and commit ranges before returning.
//! Only this manager and its clones may publish head or immutable metadata while
//! it owns storage. The caller supplies matching deployment configuration and a
//! store whose commit outcomes can be resolved from authoritative snapshots.
//!
//! Commit serializes file appends, synchronization and KV publication. Keys and
//! commit ranges publish atomically with cells, index and head, after files are
//! durable. On an error the publisher resolves the outcome before removing
//! unpublished tails. A known abort with successful recovery preserves the sealed
//! branch for retry. An unresolved outcome, failed recovery or publication panic
//! blocks further publication until reopen and recovery.
//!
//! The caller obtains an independent committed reader with
//! [`storage.reader()`](golemdb_immutable_data::ImmutableDataStorage::reader) before
//! transferring storage into the manager, and uses it after construction succeeds.
//! Its `get` and `rows_of` accept a caller-owned published snapshot and the commit number
//! read from that same snapshot. Reads enforce published bounds and never lock or
//! flush the writer. Appends may proceed concurrently; destructive file cleanup
//! waits for read mappings. The reader keeps directory ownership alive when cloned.
//! The manager owns an optional locked immutable writer alongside the store;
//! `commit.rs` implements publication. Without immutable storage, commits rely
//! on the store's own writer serialization.
//!
//! The API facade owns a store handle and the independent reader for committed
//! reads, and delegates branch writes here.

mod buffer;
mod commit;
mod error;
mod head;
mod immutable;
mod journal;
mod manager;
mod overlay;
mod scan;
mod seal;
mod types;

pub use error::{BranchError, OperationError, Result};
pub use head::{Head, read_head, read_head_state, write_head};
pub use manager::Branches;
pub use overlay::{CellRead, CellWrite};
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
