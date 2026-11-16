use golemdb_storage::StorageError;

/// Failures validating pruning metadata, recording superseded rows or pruning.
#[derive(Debug, thiserror::Error)]
pub enum PruningError {
    #[error("invalid history pruning metadata: {0}")]
    InvalidHistoryMetadata(&'static str),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("table {0} is not versioned, so it has no superseded rows")]
    UnversionedTable(&'static str),
}

/// A result with a [`PruningError`].
pub type Result<T> = std::result::Result<T, PruningError>;
