//! Record CRUD over guarded branch cell views.
//!
//! Records may have zero user cells; their `#key`, `#meta` and binding preserve identity
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
//! `#maxStrLen`, and `#maxBytesLen` cells, and u64 `#maxRecordCells` and
//! `#maxRecordIndexedCells` caps; `#alloc.#nextRecordID` must be a u64
//! starting at [`golemdb_cells::system::FIRST_USER_RECORD_ID`]. These system
//! cells are non-indexed. Initialize the cell trie and head consistently and
//! validate configured limits against the storage backend's physical ceilings.
//! No YAML loading, genesis writes, local parameter defaults, or parameter
//! cache lives here.
//!
//! User records carry a [`RecordMeta`] bytes32 field. Counts come from the same
//! effects as receipts and are updated atomically. Reads return stored cells
//! without validating `#meta`. Patch and delete validate metadata before using
//! its counters; missing or malformed metadata and counter underflow are state errors.
//!
//! Every CRUD operation requires an explicit [`golemdb_metering::Budget`] and
//! returns an [`Outcome`] with execution context and actual user effects for
//! API receipts. Pricing and budget enforcement remain deferred.
//! [`ReadTarget::Head`] selects and reads the current head in one snapshot.
//! Commit-targeted reads support the head only and explicitly reject other
//! commits until history is available. Deletes enumerate and tombstone every
//! live cell; sealing derives index changes from the resulting cell diff.
//!
//! ```
//! use golemdb_branch::Branches;
//! use golemdb_cells::{CellKind, CellNameRef, CellValue};
//! use golemdb_merkle::HashProvider;
//! use golemdb_metering::Budget;
//! use golemdb_record::{CreateEntry, ReadTarget, RecordKey, Records};
//! use golemdb_storage::Store;
//!
//! // The connection layer has already initialized the store and parameters.
//! fn example<S: Store, H: HashProvider>(branches: Branches<S, H>)
//!     -> Result<(), Box<dyn std::error::Error>>
//! {
//!     let records = Records::new(branches.clone());
//!     let branch = branches.begin()?;
//!     let key = RecordKey([7; 32]);
//!     let entries = vec![CreateEntry {
//!         name: CellNameRef::raw(b"status").into(),
//!         value: CellValue::from_str("ready"),
//!         kind: CellKind::Attribute,
//!     }];
//!     records.create(branch, key, entries, Budget::Unlimited).result?;
//!     let pending = records.get(ReadTarget::Branch(branch), key, None, Budget::Unlimited).result?;
//!     let commit = branches.commit(branch)?;
//!     let stored = records.get(ReadTarget::Commit(commit), key, None, Budget::Unlimited).result?;
//!     assert_eq!(pending, stored);
//!     Ok(())
//! }
//! ```

mod admission;
mod committed;
mod crud;
mod error;
mod meta;
mod outcome;
mod state;
mod types;

pub use committed::{committed_snapshot, read_committed_by_id};
pub use crud::Records;
pub use error::{RecordError, Result};
pub use meta::{RecordLimits, RecordMeta};
pub use outcome::{Effects, Outcome, cell_size, index_size};
pub use types::{
    CellPatch, CreateEntry, PatchChange, PatchEntry, ReadTarget, Record, RecordCells, RecordKey,
    RecordPatch,
};

#[cfg(test)]
#[path = "tests/crud.rs"]
mod crud_tests;

#[cfg(test)]
#[path = "tests/atomicity.rs"]
mod atomicity_tests;
