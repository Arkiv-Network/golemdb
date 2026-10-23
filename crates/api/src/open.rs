use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use golemdb_branch::Head;
use golemdb_merkle::{Blake3Hasher, HashAlgorithm, Keccak256Hasher};
use golemdb_pruning::PruningConfig;
use golemdb_storage::{Store, WriteTransaction};

use crate::{CommitId, Genesis, OpenConfig, OpenResult};

/// Snapshot of opening, not a live head monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenInfo {
    pub created: bool,
    pub commit_id: CommitId,
    pub state_root: [u8; 32],
    pub index_root: [u8; 32],
    pub genesis_id: [u8; 32],
}

impl OpenInfo {
    pub(crate) fn new(created: bool, head: Head, genesis_id: [u8; 32]) -> Self {
        Self {
            created,
            commit_id: head.commit_id,
            state_root: head.state_root,
            index_root: head.index_root,
            genesis_id,
        }
    }
}

/// Validated storage ready for engine construction. This setup boundary is for
/// the facade constructors; it is not exposed to library consumers.
/// Consume this handle with into_database to connect it to the public facade.
pub(crate) struct OpenedStore<S> {
    store: S,
    genesis: Genesis,
    info: OpenInfo,
    pruning: Option<PruningConfig>,
}

impl<S> OpenedStore<S> {
    pub fn info(&self) -> &OpenInfo {
        &self.info
    }
    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }
    /// The pruning settings the store was opened with.
    pub fn pruning(&self) -> Option<PruningConfig> {
        self.pruning
    }
    /// Hand initialized storage to the engine; low-level writes remain trusted.
    pub fn into_store(self) -> S {
        self.store
    }
}

impl<S: Store + Clone + Send + Sync + 'static> OpenedStore<S> {
    /// Construct one engine using the validated hash selection. The returned
    /// facade's clones share its branch registry and storage lifetime.
    pub fn into_database(
        self,
        immutable_data_directory: Option<std::path::PathBuf>,
    ) -> OpenResult<crate::Database> {
        crate::Database::from_opened(self, immutable_data_directory)
    }
}

/// Open a caller-supplied backend. Clone an existing backend to share its
/// environment; never open the same MDBX directory twice within one process.
/// Inspect, validate, and initialize under one writer to serialize with other
/// initializers and committers. Reopening does not commit or rewrite any rows.
pub(crate) fn open_store<S: Store>(store: S, config: &OpenConfig) -> OpenResult<OpenedStore<S>> {
    config
        .genesis
        .validate(store.max_key_size(), store.max_value_size())?;
    let mut tx = store.begin_write()?;
    let prepared = catch_unwind(AssertUnwindSafe(|| match config.genesis.hash_function {
        HashAlgorithm::Keccak256 => crate::genesis::prepare(&mut tx, config, &Keccak256Hasher),
        HashAlgorithm::Blake3 => crate::genesis::prepare(&mut tx, config, &Blake3Hasher),
    }));
    let info = match prepared {
        Ok(result) => result?,
        Err(panic) => {
            drop(tx);
            resume_unwind(panic)
        }
    };
    if info.created {
        tx.commit()?;
    } else {
        tx.abort();
    }
    Ok(OpenedStore {
        store,
        genesis: config.genesis.clone(),
        info,
        pruning: config.pruning,
    })
}

/// Capacity settings are local backend options and do not affect genesis identity.
/// ExistingOnly may open an empty environment but will never initialize it.
pub(crate) fn open_mdbx(
    path: impl AsRef<std::path::Path>,
    config: &OpenConfig,
    options: golemdb_storage_mdbx::MdbxOptions,
) -> OpenResult<OpenedStore<golemdb_storage_mdbx::MdbxStore>> {
    // Do not create a missing directory for ExistingOnly. Initialization itself
    // is always determined from the contents under the storage writer lock.
    if config.mode == crate::OpenMode::ExistingOnly
        && !path.as_ref().try_exists().map_err(crate::OpenError::Io)?
    {
        return Err(crate::OpenError::NotInitialized);
    }
    let store = golemdb_storage_mdbx::MdbxStore::open_with_options(path, options)?;
    open_store(store, config)
}
