use crate::{ImmutableDataConfigError, ImmutableDataError};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImmutableDataStorageError {
    #[error(transparent)]
    Input(#[from] ImmutableDataError),
    #[error(transparent)]
    Configuration(#[from] ImmutableDataConfigError),
    #[error(transparent)]
    Store(#[from] golemdb_storage::StorageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    NippyJar(#[from] reth_nippy_jar::NippyJarError),
    #[error("corrupt immutable data: {0}")]
    Corrupt(&'static str),
    #[error("invalid immutable storage operation: {0}")]
    Invalid(&'static str),
    #[error("immutable key, ordinal or commit range is already recorded")]
    AlreadyExists,
    #[error("immutable data not found")]
    NotFound,
    #[error("immutable ordinal, commit or file index overflow")]
    Overflow,
}

pub type StorageResult<T> = Result<T, ImmutableDataStorageError>;
