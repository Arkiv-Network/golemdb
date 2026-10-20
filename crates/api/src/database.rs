use crate::open::OpenedStore;
use std::sync::Arc;

use golemdb_branch::Branches;
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher};
use golemdb_query::Queries;
use golemdb_record::Records;
use golemdb_storage::Store;

use crate::{
    Api, ApiError, BranchId, BranchInfo, CommitId, CountOp, Genesis, HashAlgorithm,
    ImmutableDataAddress, ImmutableDataKey, ImmutableDataOrdinal, ImmutableDataRow, OpenConfig,
    OpenInfo, OpenResult, QueryOp, QueryPage, ReadTarget, Record, RecordKey, RecordOp, Result,
    SealInfo, op,
};

/// Cloneable public database handle. Clones share one engine and branch registry;
/// a branch begun through one clone can be used through any other clone.
/// Dropping the last clone discards pending work; only commit publishes it.
///
/// Backend and hash-provider types stay inside the engine. Dispatch occurs once
/// per API operation; hashing within an operation uses a concrete provider.
/// No storage writer or unrestricted cell access is exposed by this handle.
#[derive(Clone)]
pub struct Database {
    inner: Arc<dyn Api + Send + Sync>,
    genesis: Genesis,
    info: OpenInfo,
}

impl Database {
    /// Initialize a fresh memory store. Clone the handle to share its branches.
    pub fn open_memory(genesis: &Genesis) -> OpenResult<Self> {
        Self::from_store(
            golemdb_storage::MemoryStore::new(),
            &OpenConfig::new(*genesis),
        )
    }

    /// Initialize or validate a caller-supplied store. Separate calls create
    /// separate branch registries, even over shared storage. The engine keeps
    /// clones of `store`, so its clones must share state, as the clones of
    /// `MemoryStore` and `MdbxStore` do.
    pub fn from_store<S: Store + Clone + Send + Sync + 'static>(
        store: S,
        config: &OpenConfig,
    ) -> OpenResult<Self> {
        crate::open::open_store(store, config)?.into_database()
    }

    /// Open MDBX storage with default capacity settings. Clone a live handle to
    /// share its environment; close all handles before independently reopening
    /// the same path.
    pub fn open(path: impl AsRef<std::path::Path>, config: &OpenConfig) -> OpenResult<Self> {
        Self::open_with_options(path, config, crate::MdbxOptions::default())
    }

    /// Open MDBX with local capacity settings, which do not affect genesis identity.
    pub fn open_with_options(
        path: impl AsRef<std::path::Path>,
        config: &OpenConfig,
        options: crate::MdbxOptions,
    ) -> OpenResult<Self> {
        crate::open::open_mdbx(path, config, options)?.into_database()
    }

    /// Immutable deployment configuration validated at opening.
    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    /// Opening snapshot, shared in meaning by all clones. Use Api::head for the
    /// current head, or ReadTarget::Head for a consistent current record read.
    pub fn info(&self) -> &OpenInfo {
        &self.info
    }

    pub(crate) fn from_opened<S: Store + Clone + Send + Sync + 'static>(
        opened: OpenedStore<S>,
    ) -> OpenResult<Self> {
        let genesis = *opened.genesis();
        let info = *opened.info();
        let store = opened.into_store();
        let inner: Arc<dyn Api + Send + Sync> = match genesis.hash_function {
            HashAlgorithm::Keccak256 => Arc::new(Inner::new(store, Keccak256Hasher)?),
            HashAlgorithm::Blake3 => Arc::new(Inner::new(store, Blake3Hasher)?),
        };
        Ok(Self {
            inner,
            genesis,
            info,
        })
    }
}

impl Api for Database {
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Result<RecordKey> {
        self.inner.create(branch, operation)
    }
    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Result<Record> {
        self.inner.get(target, operation)
    }
    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Result<()> {
        self.inner.patch(branch, operation)
    }
    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Result<()> {
        self.inner.delete(branch, operation)
    }
    fn query(&self, commit_id: Option<CommitId>, operation: QueryOp) -> Result<QueryPage> {
        self.inner.query(commit_id, operation)
    }
    fn count(&self, commit_id: Option<CommitId>, operation: CountOp) -> Result<u64> {
        self.inner.count(commit_id, operation)
    }
    fn head(&self) -> Result<CommitId> {
        self.inner.head()
    }
    fn begin(&self) -> Result<BranchId> {
        self.inner.begin()
    }
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo> {
        self.inner.branch_info(branch)
    }
    fn checkpoint(&self, branch: BranchId) -> Result<()> {
        self.inner.checkpoint(branch)
    }
    fn rollback(&self, branch: BranchId) -> Result<()> {
        self.inner.rollback(branch)
    }
    fn seal(&self, branch: BranchId) -> Result<SealInfo> {
        self.inner.seal(branch)
    }
    fn commit(&self, branch: BranchId) -> Result<CommitId> {
        self.inner.commit(branch)
    }
    fn discard(&self, branch: BranchId) -> Result<()> {
        self.inner.discard(branch)
    }

    fn immutable_data_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal> {
        self.inner.immutable_data_append(branch, segment, key, row)
    }
    fn immutable_data_get(
        &self,
        segment: &str,
        address: ImmutableDataAddress,
    ) -> Result<ImmutableDataRow> {
        self.inner.immutable_data_get(segment, address)
    }
    fn immutable_data_range_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<std::ops::Range<ImmutableDataOrdinal>> {
        self.inner.immutable_data_range_of(segment, commit)
    }
    fn immutable_data_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<Vec<ImmutableDataRow>> {
        self.inner.immutable_data_rows_of(segment, commit)
    }
}

/// The most `And` / `Or` levels a query's filter may nest. Hardcoded for now:
/// it will be read from genesis (`#maxQueryDepth`) in the scope of issues #48
/// and #49.
const MAX_QUERY_DEPTH: u32 = 64;

/// All layers share one store, through clones of its handle, and record and
/// branch one branch registry. Business rules, locking, and transaction
/// boundaries remain in record, query and branch, not in this adapter.
struct Inner<S, H> {
    branches: Branches<S, H>,
    records: Records<S, H>,
    queries: Queries<S, H>,
}

impl<S: Store + Clone, H: HashProvider + Clone> Inner<S, H> {
    fn new(store: S, hasher: H) -> OpenResult<Self> {
        let branches = Branches::new(store.clone(), hasher.clone())?;
        let records = Records::new(branches.clone());
        let queries = Queries::new(store, hasher, Some(MAX_QUERY_DEPTH));
        Ok(Self {
            branches,
            records,
            queries,
        })
    }
}

impl<S: Store, H: HashProvider> Api for Inner<S, H> {
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Result<RecordKey> {
        let (key, cells) = operation.into_create();
        self.records.create(branch, key, cells).map_err(Into::into)
    }
    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Result<Record> {
        self.records
            .get(target, operation.record_key(), operation.names())
            .map_err(Into::into)
    }
    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Result<()> {
        let (key, patch) = operation.into_patch();
        self.records.patch(branch, key, patch).map_err(Into::into)
    }
    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Result<()> {
        self.records
            .delete(branch, operation.record_key())
            .map_err(Into::into)
    }
    fn query(&self, commit: Option<CommitId>, operation: QueryOp) -> Result<QueryPage> {
        self.queries.query(commit, operation).map_err(Into::into)
    }
    fn count(&self, commit: Option<CommitId>, operation: CountOp) -> Result<u64> {
        self.queries.count(commit, operation).map_err(Into::into)
    }
    fn head(&self) -> Result<CommitId> {
        self.branches.head().map_err(Into::into)
    }
    fn begin(&self) -> Result<BranchId> {
        self.branches.begin().map_err(Into::into)
    }
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo> {
        self.branches.branch_info(branch).map_err(Into::into)
    }
    fn checkpoint(&self, branch: BranchId) -> Result<()> {
        self.branches.checkpoint(branch).map_err(Into::into)
    }
    fn rollback(&self, branch: BranchId) -> Result<()> {
        self.branches.rollback(branch).map_err(Into::into)
    }
    fn seal(&self, branch: BranchId) -> Result<SealInfo> {
        self.branches
            .seal(branch)
            .map(|sealed| SealInfo::from(sealed.as_ref()))
            .map_err(Into::into)
    }
    fn commit(&self, branch: BranchId) -> Result<CommitId> {
        self.branches.commit(branch).map_err(Into::into)
    }
    fn discard(&self, branch: BranchId) -> Result<()> {
        self.branches.discard(branch).map_err(Into::into)
    }

    fn immutable_data_append(
        &self,
        _branch: BranchId,
        _segment: &str,
        _key: Option<ImmutableDataKey>,
        _row: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_append",
        })
    }
    fn immutable_data_get(
        &self,
        _segment: &str,
        _address: ImmutableDataAddress,
    ) -> Result<ImmutableDataRow> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_get",
        })
    }
    fn immutable_data_range_of(
        &self,
        _segment: &str,
        _commit: CommitId,
    ) -> Result<std::ops::Range<ImmutableDataOrdinal>> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_range_of",
        })
    }
    fn immutable_data_rows_of(
        &self,
        _segment: &str,
        _commit: CommitId,
    ) -> Result<Vec<ImmutableDataRow>> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_rows_of",
        })
    }
}
