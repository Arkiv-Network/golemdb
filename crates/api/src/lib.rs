//! Public Rust contracts for GolemDB record CRUD, queries and branch lifecycle.
//!
//! [`Api`] is the synchronous facade contract for CRUD, query and branch operations.
//! Consumers can use `Arc<dyn Api + Send + Sync>`; builders accept explicit cell values.
//! All results own their data and expose no storage transaction or cell writer.
//!
//! [`Database`] implements Api on a cloneable handle with a shared branch registry.
//! Its constructors initialize or validate genesis and select the configured hash.
//! CRUD, query and count return [`Metered`] results on success and failure.
//! CRUD enforces budgets using committed prices and returns charged work on failure.
//! Query and count remain unmetered and their budgets are not yet enforced.
//! Current record storage supports branch reads and committed-head reads only.
//! Immutable rows are staged on branches and read from committed storage, with
//! named column projections and native typed access.
//!
//! ```
//! use golemdb_api::{Api, CellLimits, CellValue, Genesis, Database,
//!     HashAlgorithm, OpenConfig, ReadTarget, RecordOp, RecordKey};
//!
//! let config = OpenConfig::new(Genesis::new(HashAlgorithm::Keccak256, CellLimits {
//!         max_cell_name_len: 32, max_str_len: 64, max_bytes_len: 128,
//! },
//!     golemdb_api::RecordLimits { max_cells: 100, max_indexed_cells: 100 },
//!     None,
//!     golemdb_api::PricingConfig::zero(),
//! ));
//! let db = Database::open_memory(&config.genesis)?;
//! let branch = db.begin()?;
//! let key = RecordKey([0x42; 32]);
//! db.create(branch, RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))
//!     .attribute("price", CellValue::from_i32(50))
//!     .field("description", CellValue::from_str("A product"))).into_result()?;
//! let pending = db.clone().get(ReadTarget::Branch(branch), RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
//! assert_eq!(pending.cells[b"price".as_slice()].as_i32(), Some(50));
//! let commit = db.commit(branch)?;
//! assert_eq!(commit.get(), 1);
//! assert_eq!(db.get(ReadTarget::Head, RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?, pending);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod config;
mod database;
mod error;
mod genesis;
mod metered;
mod open;
mod open_error;
mod pricing;
mod record_op;
mod types;

pub use config::{Genesis, OpenConfig, OpenMode};
pub use database::Database;
pub use error::{ApiError, Result};
pub use golemdb_cells::CellLimits;
pub use golemdb_immutable_data::{
    FromImmutableDataValue, ImmutableDataAddress, ImmutableDataColumn, ImmutableDataCompression,
    ImmutableDataConfig, ImmutableDataConfigError, ImmutableDataError, ImmutableDataKey,
    ImmutableDataOrdinal, ImmutableDataReadOptions, ImmutableDataRow, ImmutableDataSchema,
    ImmutableDataSegment,
};
pub use golemdb_merkle::HashAlgorithm;
pub use golemdb_metering::{Budget, HostMeteringSchema, Ledger, PricingConfig, WeightName};
pub use golemdb_pruning::PruningConfig;
pub use golemdb_storage_mdbx::MdbxOptions;
pub use metered::{Details, Metered, Receipt};
pub use open::OpenInfo;
pub use open_error::{OpenError, OpenResult};
pub use pricing::{MeteringModelInfo, MeteringModelMetadata, ModelWeights};
pub use record_op::{RecordOp, op};
pub use types::SealInfo;

pub use golemdb_branch::{BranchId, BranchInfo, CommitId};
pub use golemdb_cells::{
    CellKind, CellName, CellNameRef, CellType, CellValue, FloatWidth, NotNan32, NotNan64, Width,
    marker,
};
pub use golemdb_query::{
    Bounds, CountOp, Direction, EqValue, ExistsType, Filter, InValues, QueryOp, QueryPage, SortKey,
    ValuesError,
};
pub use golemdb_record::{
    CreateEntry, PatchChange, PatchEntry, ReadTarget, Record, RecordKey, RecordLimits, RecordMeta,
};

/// Synchronous record and branch facade. Builders assist callers; implementations
/// enforce deployment limits, reserved-record rules, identity, and atomic mutations.
/// Adding a required method requires updating concrete implementations and mocks.
pub trait Api {
    /// Stage a caller-keyed record with zero or more user cells and return its key.
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Metered<RecordKey>;

    /// Read pending or committed state. Head selection and materialization share
    /// one snapshot. Missing projected cells are omitted; an empty projection
    /// still verifies existence. Explicit non-head commits are not yet supported.
    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Metered<Record>;

    /// Atomically edit one record. Removing every user cell preserves its identity.
    /// Empty patches and removals of missing cells are valid on an existing record.
    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Metered<()>;

    /// Delete the record and its binding without reclaiming the internal ID.
    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Metered<()>;

    /// Committed records matching the operation's filter, in the order of its
    /// sort keys, ties and an unsorted query in record ID order, projected as
    /// `get` projects. A page skips the operation's offset, then holds the
    /// next matches in that order, up to the operation's limit, or with no
    /// limit up to the implementation's page size; `total_matched` counts
    /// every match. An offset past the last match gives an empty page. A
    /// limit of 0 returns InvalidQuery; one above the page size, or more sort
    /// keys than the implementation's cap, LimitExceeded, naming the cap.
    /// Matching and materialization share one snapshot. `commit` names the
    /// commit to read, `None` the head; explicit non-head commits are not yet
    /// supported.
    fn query(&self, commit_id: Option<CommitId>, operation: QueryOp) -> Metered<QueryPage>;

    /// The number of committed records matching the operation's filter, under
    /// the same commit rules as `query`.
    fn count(&self, commit_id: Option<CommitId>, operation: CountOp) -> Metered<u64>;

    /// Unmetered committed pricing inspection. None selects head; explicit
    /// non-head commits return CommitUnavailable until historical reads exist.
    fn metering_model(&self, commit_id: Option<CommitId>) -> Result<MeteringModelInfo>;

    /// Weights (including host.*) at the selected commit. None selects its active
    /// model. Use the same explicit commit for related model/weight lookups.
    fn model_weights(
        &self,
        commit_id: Option<CommitId>,
        version: Option<u64>,
    ) -> Result<ModelWeights>;

    // Branch lifecycle. IDs are valid only for their issuing engine.
    fn head(&self) -> Result<CommitId>;
    fn begin(&self) -> Result<BranchId>;
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo>;
    fn checkpoint(&self, branch: BranchId) -> Result<()>;

    /// Undo one frame. Repeated rollback moves to preceding frames.
    fn rollback(&self, branch: BranchId) -> Result<()>;

    /// Freeze live state without publishing. Sealed branches allow reads and immutable
    /// appends/truncation, but reject live-state writes, checkpoints, and rollback.
    /// The `#roots` cell added by sealing is visible only after commit.
    fn seal(&self, branch: BranchId) -> Result<SealInfo>;

    /// Publish and consume the branch. A live losing branch returns Conflict;
    /// unknown, consumed, or previously invalidated IDs return HandleInvalid.
    fn commit(&self, branch: BranchId) -> Result<CommitId>;
    fn discard(&self, branch: BranchId) -> Result<()>;

    /// Discover a segment's ordered column names and logical types from genesis.
    /// Unknown segments return NotFound, including when no segments are configured.
    fn immutable_data_schema(&self, segment: &str) -> Result<ImmutableDataSchema>;

    /// Stage a complete typed row before or after seal, returning a provisional
    /// segment ordinal. An optional key must be unique within committed bindings
    /// and this branch's staged rows. Rows and keys become visible together at commit.
    fn immutable_data_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal>;

    /// Remove this branch's staged suffix beginning at `from_ordinal`, releasing
    /// its keys and ordinals. Cannot truncate committed rows. Works after seal and
    /// remains independent of live-state checkpoints and rollback.
    fn immutable_data_truncate(
        &self,
        branch: BranchId,
        segment: &str,
        from_ordinal: ImmutableDataOrdinal,
    ) -> Result<()>;

    /// Read a committed row by ordinal or key using one current published snapshot.
    /// Projections preserve caller order and reject empty, duplicate or unknown columns.
    fn immutable_data_get(
        &self,
        segment: &str,
        address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions,
    ) -> Result<ImmutableDataRow>;

    /// Read this commit's rows in append order using one published snapshot.
    /// Existing commits with no rows (including genesis) return an empty vector;
    /// future commits return CommitUnavailable. Projection follows the get rules.
    fn immutable_data_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
        options: &ImmutableDataReadOptions,
    ) -> Result<Vec<ImmutableDataRow>>;
}

#[cfg(test)]
mod tests;
