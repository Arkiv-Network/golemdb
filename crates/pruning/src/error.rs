use golemdb_storage::StorageError;

/// Failures validating pruning metadata, recording superseded rows or pruning.
#[derive(Debug, thiserror::Error)]
pub enum PruningError {
    #[error("commit {requested} is after current head {head}")]
    CommitInFuture { requested: u64, head: u64 },
    #[error("commit {requested} is outside supported history {earliest}..={head}")]
    CommitOutsideHistory {
        requested: u64,
        earliest: u64,
        head: u64,
    },
    #[error("invalid history pruning metadata: {0}")]
    InvalidHistoryMetadata(&'static str),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("table {0} is not versioned, so it has no superseded rows")]
    UnversionedTable(&'static str),
}

/// A result with a [`PruningError`].
pub type Result<T> = std::result::Result<T, PruningError>;
