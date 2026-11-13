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
//! The caller supplies matching immutable limits and a shared pricing provider through
//! [`WritePricing`]. No YAML loading, genesis writes or local limit defaults
//! live here; admission does not reread parameters from the branch.
//!
//! User records carry a [`RecordMeta`] bytes32 field. Counts come from the same
//! effects as receipts and are updated atomically. Projections return stored cells;
//! full `get`, patch and delete validate metadata before using its counters.
//! Missing or malformed metadata and counter underflow are state errors.
//!
//! Every CRUD operation requires an explicit [`golemdb_metering::Budget`] and
//! returns an [`Outcome`] with execution context and actual user effects for
//! API receipts. Outcomes can carry the meter's cost and ledger, including on
//! failure. CUD always uses [`golemdb_metering::WriteMeter`] through a shared
//! admission/plan/approve/apply path. Prices are captured once at the validated
//! branch origin. Explicit zero weights keep writes free with full ledgers;
//! missing weights stop only the operations that need them. `get` uses
//! [`golemdb_metering::ReadMeter`], charging admission, lookups, scans and returned bytes.
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
//! use golemdb_record::{CreateEntry, ReadTarget, RecordKey, Records, WritePricing};
//! use golemdb_storage::Store;
//!
//! // The connection layer initialized the store and supplied matching pricing/limits.
//! fn example<S: Store, H: HashProvider>(branches: Branches<S, H>, pricing: WritePricing)
//!     -> Result<(), Box<dyn std::error::Error>>
//! {
//!     let records = Records::new(branches.clone(), pricing);
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
mod plan;
mod pricing;
mod read;
mod state;
mod types;

pub use committed::{committed_snapshot, read_committed_by_id};
pub use crud::Records;
pub use error::{RecordError, Result};
pub use meta::{RecordLimits, RecordMeta};
pub use outcome::{Effects, Outcome, cell_size, index_size};
pub use pricing::WritePricing;
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

#[cfg(test)]
#[path = "tests/metering.rs"]
mod metering_tests;
