use golemdb_storage::StorageError;

/// Failures recording superseded rows or deleting the due ones.
#[derive(Debug, thiserror::Error)]
pub enum PruningError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("table {0} is not versioned, so it has no superseded rows")]
    UnversionedTable(&'static str),
    #[error("superseded key of {len} bytes is too long to list")]
    KeyTooLong { len: usize },
}

/// A result with a [`PruningError`].
pub type Result<T> = std::result::Result<T, PruningError>;
