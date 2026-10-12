/// Failures admitting branch operations, reading cells, or moving through frames.
#[derive(Debug, thiserror::Error)]
pub enum BranchError {
    #[error("branch ID is unknown, discarded, or no longer over the head")]
    HandleInvalid,
    #[error("branch is sealed")]
    Sealed,
    #[error("commit number space exhausted")]
    CommitNumberExhausted,
    #[error("the previous commit's #roots cell already exists")]
    RootsCellExists,
    #[error("garbage collection: {0}")]
    GarbageCollection(&'static str),
    #[error("unsupported database format")]
    UnsupportedDatabaseFormat,
    #[error("retaining {0} commits before head is not implemented")]
    UnsupportedRetention(u64),
    #[error("genesis requires empty engine tables; {0:?} is not empty")]
    DatabaseNotEmpty(golemdb_storage::Table),
    #[error(transparent)]
    Merkle(#[from] golemdb_merkle::MerkleError),
    #[error(transparent)]
    Cells(#[from] golemdb_cells::CellError),
    #[error(transparent)]
    Index(#[from] golemdb_index::IndexError),
    #[error(transparent)]
    Term(#[from] golemdb_index::TermError),
    #[error("database has no Superblock/head row; initialize genesis before opening branches")]
    MissingHead,
    #[error("Superblock/head must be 72 bytes, got {actual}")]
    InvalidHead { actual: usize },
    #[error("branch ID space exhausted")]
    BranchNumberExhausted,
    #[error("branch {0} lock poisoned")]
    Poisoned(&'static str),
    #[error(transparent)]
    Storage(#[from] golemdb_storage::StorageError),
    #[error("invalid stored cell key: {0}")]
    Key(#[from] golemdb_cells::CellKeyError),
    #[error("invalid stored cell value: {0}")]
    Value(#[from] golemdb_cells::CellValueParseError),
    #[error("no frame remains to roll back")]
    NoFrameToRollback,
}

/// Distinguishes handle/head admission failures from a callback's own error.
/// The latter is returned unchanged; it need not be a branch error.
#[derive(Debug, thiserror::Error)]
pub enum OperationError<E> {
    #[error(transparent)]
    Branch(BranchError),
    #[error(transparent)]
    Operation(E),
}

pub type Result<T> = std::result::Result<T, BranchError>;
