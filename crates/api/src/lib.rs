//! Public Rust contracts for GolemDB record CRUD, queries and branch lifecycle.
//!
//! [`Api`] is the synchronous facade contract for CRUD, query and branch operations.
//! Consumers can use `Arc<dyn Api + Send + Sync>`; builders accept explicit cell values.
//! All results own their data and expose no storage transaction or cell writer.
//!
//! [`Database`] implements Api on a cloneable handle with a shared branch registry.
//! Its constructors initialize or validate genesis and select the configured hash.
//! These contracts are unmetered: no receipt, debug flag, or record version. Queries
//! take a budget, which is not enforced yet.
//! Current record storage supports branch reads and committed-head reads only.
//! Immutable-data types and methods are exposed, but every such Database call
//! returns [`ApiError::NotImplemented`] immediately without I/O or state changes.
//!
//! ```
//! use golemdb_api::{Api, CellLimits, CellValue, Genesis, Database,
//!     HashAlgorithm, OpenConfig, ReadTarget, RecordOp, RecordKey};
//!
//! let config = OpenConfig::new(Genesis::new(HashAlgorithm::Keccak256, CellLimits {
//!         max_cell_name_len: 32, max_str_len: 64, max_bytes_len: 128,
//! },
//!     golemdb_api::ImmutableDataConfig::new(500_000, vec![]),
//! ));
//! let db = Database::open_memory(&config.genesis)?;
//! let branch = db.begin()?;
//! let key = RecordKey([0x42; 32]);
//! db.create(branch, RecordOp::create(key)
//!     .attribute("price", CellValue::from_i32(50))?
//!     .field("description", CellValue::from_str("A product"))?)?;
//! let pending = db.clone().get(ReadTarget::Branch(branch), RecordOp::get(key))?;
//! assert_eq!(pending.cells[b"price".as_slice()].as_i32(), Some(50));
//! let commit = db.commit(branch)?;
//! assert_eq!(commit.get(), 1);
//! assert_eq!(db.get(ReadTarget::Head, RecordOp::get(key))?, pending);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod config;
mod database;
mod error;
mod genesis;
mod immutable_data;
mod open;
mod open_error;
mod record_op;
mod types;

pub use config::{Genesis, OpenConfig, OpenMode};
pub use database::Database;
pub use error::{ApiError, Result};
pub use golemdb_cells::CellLimits;
pub use golemdb_immutable_data::{
    ImmutableDataColumn, ImmutableDataCompression, ImmutableDataConfig, ImmutableDataConfigError,
    ImmutableDataSegment,
};
pub use golemdb_merkle::HashAlgorithm;
pub use golemdb_storage_mdbx::MdbxOptions;
pub use immutable_data::{
    ImmutableDataAddress, ImmutableDataKey, ImmutableDataOrdinal, ImmutableDataRow,
};
pub use open::OpenInfo;
pub use open_error::{OpenError, OpenResult};
pub use record_op::{RecordOp, op};
pub use types::SealInfo;

pub use golemdb_branch::{BranchId, BranchInfo, CommitId};
pub use golemdb_cells::{
    CellKind, CellName, CellNameRef, CellType, CellValue, FloatWidth, NotNan32, NotNan64, Width,
    marker,
};
pub use golemdb_query::{
    Bounds, Budget, CountOp, Direction, EqValue, Filter, InValues, QueryOp, QueryPage, SortKey,
    ValuesError,
};
pub use golemdb_record::{ReadTarget, Record, RecordKey};

/// Synchronous record and branch facade. Builders assist callers; implementations
/// enforce deployment limits, reserved-record rules, identity, and atomic mutations.
/// Adding a required method requires updating concrete implementations and mocks.
pub trait Api {
    /// Stage a caller-keyed record with zero or more user cells and return its key.
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Result<RecordKey>;

    /// Read pending or committed state. Head selection and materialization share
    /// one snapshot. Missing projected cells are omitted; an empty projection
    /// still verifies existence. Explicit non-head commits are not yet supported.
    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Result<Record>;

    /// Atomically edit one record. Removing every user cell preserves its identity.
    /// Empty patches and removals of missing cells are valid on an existing record.
    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Result<()>;

    /// Delete the record and its binding without reclaiming the internal ID.
    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Result<()>;

    /// Committed records matching the operation's filter, in record ID order,
    /// projected as `get` projects. Matching and materialization share one
    /// snapshot. `commit` names the commit to read, `None` the head; explicit
    /// non-head commits are not yet supported; sort, limit and offset return
    /// NotImplemented.
    fn query(&self, commit_id: Option<CommitId>, operation: QueryOp) -> Result<QueryPage>;

    /// The number of committed records matching the operation's filter, under
    /// the same commit rules as `query`.
    fn count(&self, commit_id: Option<CommitId>, operation: CountOp) -> Result<u64>;

    // Branch lifecycle. IDs are valid only for their issuing engine.
    fn head(&self) -> Result<CommitId>;
    fn begin(&self) -> Result<BranchId>;
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo>;
    fn checkpoint(&self, branch: BranchId) -> Result<()>;

    /// Undo one frame. Repeated rollback moves to preceding frames.
    fn rollback(&self, branch: BranchId) -> Result<()>;

    /// Freeze without publishing. Sealed branches allow reads but reject writes, checkpoints, and rollback.
    /// The `#roots` cell added by sealing is visible only after commit.
    fn seal(&self, branch: BranchId) -> Result<SealInfo>;

    /// Publish and consume the branch. A live losing branch returns Conflict;
    /// unknown, consumed, or previously invalidated IDs return HandleInvalid.
    fn commit(&self, branch: BranchId) -> Result<CommitId>;
    fn discard(&self, branch: BranchId) -> Result<()>;

    /// Future contract: stage a row on a sealed branch with an optional unique
    /// segment-local key; return a provisional ordinal. Duplicate keys never
    /// overwrite rows. Row and key binding become visible together at commit.
    /// Currently returns NotImplemented immediately, without validating arguments.
    fn immutable_data_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal>;

    /// Future contract: read a committed row by ordinal or caller-provided key.
    /// Currently returns NotImplemented without storage access.
    fn immutable_data_get(
        &self,
        segment: &str,
        address: ImmutableDataAddress,
    ) -> Result<ImmutableDataRow>;

    /// Future contract: the half-open ordinal range appended by this commit.
    /// Currently returns NotImplemented without storage access.
    fn immutable_data_range_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<std::ops::Range<ImmutableDataOrdinal>>;

    /// Future contract: this commit's rows in append order.
    /// Currently returns NotImplemented without storage access.
    fn immutable_data_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<Vec<ImmutableDataRow>>;
}

#[cfg(test)]
mod tests;
