use std::error::Error;

use golemdb_branch::BranchError;
use golemdb_cells::{CellNameError, CellValueParseError};
use golemdb_query::QueryError;
use golemdb_record::RecordError;

use crate::CommitId;

/// Public failures, independent of how the engine nests its internal errors.
/// Sources retain detailed diagnostics without requiring consumers to match them.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    #[error("store capacity exhausted")]
    StoreFull,
    #[error("operation {operation} is not implemented")]
    NotImplemented { operation: &'static str },
    #[error("record not found")]
    NotFound,
    #[error("record key already exists")]
    AlreadyExists,
    #[error("reserved records cannot be modified through CRUD")]
    Reserved,
    #[error("invalid argument: {message}")]
    InvalidArgument {
        message: String,
        #[source]
        source: Option<Box<dyn Error + Send + Sync>>,
    },
    /// A query that cannot run as written, e.g. one targeting a branch.
    #[error("invalid query: {reason}")]
    InvalidQuery { reason: &'static str },
    #[error("branch handle is unknown, consumed, or invalidated")]
    HandleInvalid,
    #[error("commit lost the publication race")]
    Conflict,
    #[error("branch is sealed")]
    Sealed,
    #[error("no frame remains to roll back")]
    NoFrameToRollback,
    #[error("commit {requested} unavailable: only current head {head} is supported")]
    CommitUnavailable { requested: CommitId, head: CommitId },
    #[error("internal database error")]
    Internal {
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

impl ApiError {
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
            source: None,
        }
    }

    pub fn internal(source: impl Error + Send + Sync + 'static) -> Self {
        if store_full(&source) {
            return Self::StoreFull;
        }
        Self::Internal {
            source: Box::new(source),
        }
    }

    fn invalid_input(message: &str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::InvalidArgument {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl From<CellNameError> for ApiError {
    fn from(error: CellNameError) -> Self {
        Self::invalid_input("invalid cell name", error)
    }
}

impl From<CellValueParseError> for ApiError {
    fn from(error: CellValueParseError) -> Self {
        Self::invalid_input("invalid cell value", error)
    }
}

impl From<RecordError> for ApiError {
    fn from(error: RecordError) -> Self {
        match error {
            RecordError::NotFound => Self::NotFound,
            RecordError::AlreadyExists => Self::AlreadyExists,
            RecordError::Reserved => Self::Reserved,
            RecordError::InvalidArgument(message) => Self::invalid_argument(message),
            RecordError::CommitUnavailable { requested, head } => {
                Self::CommitUnavailable { requested, head }
            }
            RecordError::Branch(error) => error.into(),
            error => Self::internal(error),
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::InvalidArgument { field, source } => Self::InvalidArgument {
                message: format!("invalid filter on cell `{field}`"),
                source: Some(Box::new(source)),
            },
            QueryError::InvalidQuery { reason } => Self::InvalidQuery { reason },
            QueryError::NotImplemented { operation } => Self::NotImplemented { operation },
            QueryError::CommitUnavailable { requested, head } => {
                Self::CommitUnavailable { requested, head }
            }
            QueryError::Internal(source) if store_full(source.as_ref()) => Self::StoreFull,
            QueryError::Internal(source) => Self::Internal { source },
        }
    }
}

impl From<BranchError> for ApiError {
    fn from(error: BranchError) -> Self {
        match error {
            BranchError::HandleInvalid => Self::HandleInvalid,
            BranchError::Conflict => Self::Conflict,
            BranchError::Sealed => Self::Sealed,
            BranchError::NoFrameToRollback => Self::NoFrameToRollback,
            error => Self::internal(error),
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

// Transparent internal errors delegate source() to their wrapped error, which
// can hide a source-less StorageError::Full. Follow those wrappers explicitly.
fn store_full(error: &(dyn Error + 'static)) -> bool {
    use golemdb_cells::CellError;
    use golemdb_index::IndexError;
    use golemdb_merkle::MerkleError;
    use golemdb_storage::StorageError;
    if matches!(
        error.downcast_ref::<StorageError>(),
        Some(StorageError::Full)
    ) {
        return true;
    }
    let wrapped: Option<&(dyn Error + 'static)> =
        if let Some(error) = error.downcast_ref::<RecordError>() {
            match error {
                RecordError::Storage(e) => Some(e),
                RecordError::Cells(e) => Some(e),
                RecordError::Branch(e) => Some(e),
                _ => None,
            }
        } else if let Some(error) = error.downcast_ref::<BranchError>() {
            match error {
                BranchError::Storage(e) => Some(e),
                BranchError::Cells(e) => Some(e),
                BranchError::Index(e) => Some(e),
                _ => None,
            }
        } else if let Some(error) = error.downcast_ref::<CellError>() {
            match error {
                CellError::Storage(e) => Some(e),
                CellError::Merkle(e) => Some(e),
                _ => None,
            }
        } else if let Some(error) = error.downcast_ref::<IndexError>() {
            match error {
                IndexError::Storage(e) => Some(e),
                IndexError::Merkle(e) => Some(e),
                _ => None,
            }
        } else if let Some(MerkleError::Storage(error)) = error.downcast_ref::<MerkleError>() {
            Some(error)
        } else {
            error.source()
        };
    wrapped.is_some_and(store_full)
}
