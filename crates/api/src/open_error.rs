/// Opening errors are separate from operation failures: callers can distinguish
/// configuration mismatch, unsupported formats, and incomplete stored state.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    #[error("cannot read configuration or database path: {0}")]
    Io(#[source] std::io::Error),
    #[error("invalid genesis YAML: {0}")]
    Yaml(#[source] serde_saphyr::Error),
    #[error("invalid opening configuration: {0}")]
    InvalidConfig(String),
    #[error("database is not initialized")]
    NotInitialized,
    #[error("database is already initialized")]
    AlreadyInitialized,
    #[error("requested genesis differs from the persisted deployment")]
    GenesisMismatch,
    #[error("incomplete or corrupt database: {0}")]
    CorruptState(&'static str),
    #[error("unsupported database format {0}")]
    UnsupportedFormat(u32),
    #[error("unsupported hash function ID {0}")]
    UnsupportedHash(u16),
    #[error("unsupported Roaring format {0}")]
    UnsupportedRoaring(u16),
    #[error(transparent)]
    Storage(#[from] golemdb_storage::StorageError),
    #[error(transparent)]
    Cells(#[from] golemdb_cells::CellError),
    #[error(transparent)]
    Branch(#[from] golemdb_branch::BranchError),
    #[error(transparent)]
    Index(#[from] golemdb_index::IndexError),
}

pub type OpenResult<T> = std::result::Result<T, OpenError>;
