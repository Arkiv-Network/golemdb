use crate::open::OpenedStore;
use std::sync::Arc;

use golemdb_branch::{Branches, read_head};
use golemdb_immutable_data::{ImmutableDataReader, ImmutableDataStorage};
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher};
use golemdb_query::Queries;
use golemdb_record::Records;
use golemdb_storage::Store;

use crate::{
    Api, ApiError, BranchId, BranchInfo, CommitId, CountOp, Genesis, HashAlgorithm,
    ImmutableDataAddress, ImmutableDataKey, ImmutableDataOrdinal, ImmutableDataReadOptions,
    ImmutableDataRow, ImmutableDataSchema, OpenConfig, OpenInfo, OpenResult, QueryOp, QueryPage,
    ReadTarget, Record, RecordKey, RecordOp, Result, SealInfo, op,
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
        crate::open::open_store(
            golemdb_storage::MemoryStore::new(),
            &OpenConfig::new(genesis.clone()),
        )?
        .into_database(None)
    }

    /// Initialize or validate a caller-supplied store. Separate calls create
    /// separate branch registries, even over shared storage. The engine keeps
    /// clones of `store`, so its clones must share state, as the clones of
    /// `MemoryStore` and `MdbxStore` do. With immutable segments, supply
    /// `OpenConfig::with_immutable_data_directory` and use one engine (or its
    /// clones) for publication; the directory is exclusively owned.
    pub fn from_store<S: Store + Clone + Send + Sync + 'static>(
        store: S,
        config: &OpenConfig,
    ) -> OpenResult<Self> {
        if config.genesis.immutable_data.is_some() && config.immutable_data_directory.is_none() {
            return Err(crate::OpenError::InvalidConfig(
                "from_store requires an immutable-data directory for declared segments".into(),
            ));
        }
        crate::open::open_store(store, config)?
            .into_database(config.immutable_data_directory.clone())
    }

    /// Open MDBX storage with default capacity settings. Clone a live handle to
    /// share its environment; close all handles before independently reopening
    /// the same path. Immutable files default to `<path>/immutable-data` unless
    /// `OpenConfig` supplies a different directory.
    pub fn open(path: impl AsRef<std::path::Path>, config: &OpenConfig) -> OpenResult<Self> {
        Self::open_with_options(path, config, crate::MdbxOptions::default())
    }

    /// Open MDBX with local capacity settings, which do not affect genesis identity.
    pub fn open_with_options(
        path: impl AsRef<std::path::Path>,
        config: &OpenConfig,
        options: crate::MdbxOptions,
    ) -> OpenResult<Self> {
        let directory = config
            .immutable_data_directory
            .clone()
            .unwrap_or_else(|| path.as_ref().join("immutable-data"));
        crate::open::open_mdbx(path, config, options)?.into_database(Some(directory))
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
        immutable_data_directory: Option<std::path::PathBuf>,
    ) -> OpenResult<Self> {
        let genesis = opened.genesis().clone();
        let info = *opened.info();
        let store = opened.into_store();
        let immutable_data = genesis
            .immutable_data
            .as_ref()
            .map(|config| match immutable_data_directory {
                Some(directory) => {
                    ImmutableDataStorage::open(config.clone(), info.genesis_id, directory)
                }
                None => ImmutableDataStorage::in_memory(config.clone(), info.genesis_id),
            })
            .transpose()?;
        let inner: Arc<dyn Api + Send + Sync> = match genesis.hash_function {
            HashAlgorithm::Keccak256 => {
                Arc::new(Inner::new(store, Keccak256Hasher, immutable_data)?)
            }
            HashAlgorithm::Blake3 => Arc::new(Inner::new(store, Blake3Hasher, immutable_data)?),
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

    fn immutable_data_schema(&self, segment: &str) -> Result<ImmutableDataSchema> {
        self.inner.immutable_data_schema(segment)
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
    fn immutable_data_truncate(
        &self,
        branch: BranchId,
        segment: &str,
        from_ordinal: ImmutableDataOrdinal,
    ) -> Result<()> {
        self.inner
            .immutable_data_truncate(branch, segment, from_ordinal)
    }
    fn immutable_data_get(
        &self,
        segment: &str,
        address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions,
    ) -> Result<ImmutableDataRow> {
        self.inner.immutable_data_get(segment, address, options)
    }
    fn immutable_data_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
        options: &ImmutableDataReadOptions,
    ) -> Result<Vec<ImmutableDataRow>> {
        self.inner.immutable_data_rows_of(segment, commit, options)
    }
    fn immutable_data_range_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<std::ops::Range<ImmutableDataOrdinal>> {
        self.inner.immutable_data_range_of(segment, commit)
    }
    fn immutable_data_ordinal_of(
        &self,
        segment: &str,
        key: ImmutableDataKey,
    ) -> Result<Option<ImmutableDataOrdinal>> {
        self.inner.immutable_data_ordinal_of(segment, key)
    }
}

/// All layers share one store, through clones of its handle, and record and
/// branch share one branch registry. Immutable reads use the store and reader
/// directly, with head and metadata taken from the same committed snapshot.
/// Branches own the immutable writer and coordinate publication and recovery.
struct Inner<S, H> {
    store: S,
    immutable_reader: Option<ImmutableDataReader>,
    branches: Branches<S, H>,
    records: Records<S, H>,
    queries: Queries<S, H>,
}

impl<S: Store + Clone, H: HashProvider + Clone> Inner<S, H> {
    fn new(store: S, hasher: H, immutable_data: Option<ImmutableDataStorage>) -> OpenResult<Self> {
        let immutable_reader = immutable_data.as_ref().map(ImmutableDataStorage::reader);
        // Recover before exposing the reader through the completed database handle.
        let branches = Branches::new(store.clone(), hasher.clone(), immutable_data)?;
        let records = Records::new(branches.clone());
        let queries = Queries::new(store.clone(), hasher);
        Ok(Self {
            store,
            immutable_reader,
            branches,
            records,
            queries,
        })
    }
}

impl<S: Store, H: HashProvider> Inner<S, H> {
    fn immutable_reader(&self) -> Result<&ImmutableDataReader> {
        self.immutable_reader.as_ref().ok_or(ApiError::NotFound)
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

    fn immutable_data_schema(&self, segment: &str) -> Result<ImmutableDataSchema> {
        self.immutable_reader()?
            .schema(segment)
            .cloned()
            .map_err(Into::into)
    }
    fn immutable_data_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal> {
        self.branches
            .immutable_data_append(branch, segment, key, row)
            .map_err(Into::into)
    }
    fn immutable_data_truncate(
        &self,
        branch: BranchId,
        segment: &str,
        from_ordinal: ImmutableDataOrdinal,
    ) -> Result<()> {
        self.branches
            .immutable_data_truncate(branch, segment, from_ordinal)
            .map_err(|error| {
                // This operation's bounds error is caller input. Invalid physical
                // writer state during publication must remain an internal error.
                match error {
                    golemdb_branch::BranchError::ImmutableData(
                        golemdb_immutable_data::ImmutableDataStorageError::Invalid(message),
                    ) => ApiError::invalid_argument(message),
                    error => error.into(),
                }
            })
    }
    fn immutable_data_get(
        &self,
        segment: &str,
        address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions,
    ) -> Result<ImmutableDataRow> {
        let reader = self.immutable_reader()?;
        let tx = self.store.begin_read().map_err(ApiError::internal)?;
        let head = read_head(&tx)?;
        reader
            .get(&tx, head.get(), segment, address, options)
            .map_err(Into::into)
    }
    fn immutable_data_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
        options: &ImmutableDataReadOptions,
    ) -> Result<Vec<ImmutableDataRow>> {
        let reader = self.immutable_reader()?;
        let tx = self.store.begin_read().map_err(ApiError::internal)?;
        let head = read_head(&tx)?;
        reader
            .rows_of(&tx, head.get(), segment, commit.get(), options)
            .map_err(Into::into)
    }
    fn immutable_data_range_of(
        &self,
        segment: &str,
        commit: CommitId,
    ) -> Result<std::ops::Range<ImmutableDataOrdinal>> {
        let reader = self.immutable_reader()?;
        let tx = self.store.begin_read().map_err(ApiError::internal)?;
        let head = read_head(&tx)?;
        reader
            .range_of(&tx, head.get(), segment, commit.get())
            .map_err(Into::into)
    }
    fn immutable_data_ordinal_of(
        &self,
        segment: &str,
        key: ImmutableDataKey,
    ) -> Result<Option<ImmutableDataOrdinal>> {
        let reader = self.immutable_reader()?;
        let tx = self.store.begin_read().map_err(ApiError::internal)?;
        let head = read_head(&tx)?;
        reader
            .ordinal_of(&tx, head.get(), segment, key)
            .map_err(Into::into)
    }
}
