//! Record CRUD over guarded branch cell views.
//!
//! Records may have zero user cells; their `#key` and binding preserve identity
//! until explicit deletion.
//!
//! Caller-assigned keys only. Each mutation is one atomic branch operation,
//! including allocator and binding changes. Checkpoints, rollback and commit
//! remain on the shared [`Branches`](golemdb_branch::Branches) manager supplied to [`Records::new`].
//! Direct low-level cell writes are trusted engine operations and can bypass
//! these invariants; expose Records, not raw branch writes, to data clients.
//!
//! Opening/initialization belongs to the caller. Every record in
//! [`golemdb_cells::system::ALL`] must have its bytes32 `#key` and a u64 binding
//! in `#recordKeys`. The `#params` record must contain u32 `#maxCellNameLen`,
//! `#maxStrLen`, and `#maxBytesLen` cells; `#alloc.#nextRecordID` must be a u64
//! starting at [`golemdb_cells::system::FIRST_USER_RECORD_ID`]. These system
//! cells are non-indexed. Initialize the cell trie and head consistently and
//! validate configured limits against the storage backend's physical ceilings.
//! No YAML loading, genesis writes, local parameter defaults, or parameter
//! cache lives here.
//!
//! This iteration is unmetered: no budgets, receipts, debug options, or OCC.
//! [`ReadTarget::Head`] selects and reads the current head in one snapshot.
//! Commit-targeted reads support the head only and explicitly reject other
//! commits until history is available. Deletes enumerate and tombstone every
//! live cell; sealing derives index changes from the resulting cell diff.
//!
//! ```
//! use golemdb_branch::Branches;
//! use golemdb_cells::{CellNameRef, CellType, CellValueRef};
//! use golemdb_merkle::HashProvider;
//! use golemdb_record::{ReadTarget, RecordKey, Records};
//! use golemdb_storage::Store;
//!
//! // The connection layer has already initialized the store and parameters.
//! fn example<S: Store, H: HashProvider>(branches: Branches<S, H>)
//!     -> Result<(), Box<dyn std::error::Error>>
//! {
//!     let records = Records::new(branches.clone());
//!     let branch = branches.begin()?;
//!     let key = RecordKey([7; 32]);
//!     let name = CellNameRef::parse_user(b"status", 32)?.into();
//!     let value = CellValueRef::new(CellType::Str, b"ready", true)?.into();
//!     records.create(branch, key, [(name, value)].into())?;
//!     let pending = records.get(ReadTarget::Branch(branch), key, None)?;
//!     let commit = branches.commit(branch)?;
//!     let stored = records.get(ReadTarget::Commit(commit), key, None)?;
//!     assert_eq!(pending, stored);
//!     Ok(())
//! }
//! ```

mod committed;
mod crud;
mod error;
mod state;
mod types;

pub use committed::{committed_bindings, committed_snapshot, read_committed_by_id};
pub use crud::Records;
pub use error::{RecordError, Result};
pub use types::{CellPatch, ReadTarget, Record, RecordCells, RecordKey, RecordPatch};

#[cfg(test)]
#[path = "tests/crud.rs"]
mod crud_tests;

#[cfg(test)]
#[path = "tests/atomicity.rs"]
mod atomicity_tests;
