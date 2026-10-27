use std::error::Error;

use golemdb_branch::BranchError;
use golemdb_cells::{CellNameError, CellValueParseError};
use golemdb_immutable_data::{ImmutableDataError, ImmutableDataStorageError};
use golemdb_query::{QueryError, ValuesError};
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
    #[error("requested record, immutable segment or row not found")]
    NotFound,
    #[error("cell {name} not found")]
    CellNotFound { name: crate::CellName },
    /// Spent is receipt.cost. Required is known only after complete write planning.
    #[error("operation exceeds its budget (required: {required:?})")]
    OutOfBudget { required: Option<u64> },
    #[error("key already exists")]
    AlreadyExists,
    #[error("reserved records cannot be modified through CRUD")]
    Reserved,
    #[error("invalid argument: {message}")]
    InvalidArgument {
        message: String,
        #[source]
        source: Option<Box<dyn Error + Send + Sync>>,
    },
    /// A query that cannot run as written, e.g. an `And` or `Or` with no
    /// child, or a `limit` of 0.
    #[error("invalid query: {reason}")]
    InvalidQuery { reason: &'static str },
    /// A request over a cap, named by `limit`, whose value is `max`, e.g. a
    /// query's filter nested deeper than the query depth cap, or a query's
    /// `limit` above the page size.
    #[error("limit exceeded: {limit} is capped at {max}")]
    LimitExceeded { limit: &'static str, max: u32 },
    #[error("branch handle is unknown, consumed, or invalidated")]
    HandleInvalid,
    #[error("commit lost the publication race")]
    Conflict,
    #[error("branch is sealed")]
    Sealed,
    #[error("no frame remains to roll back")]
    NoFrameToRollback,
    #[error("commit {requested} is unavailable for this operation; current head is {head}")]
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

/// The runtime constructors of the query operands (`EqValue::try_from`,
/// `InValues::try_from`, `Bounds::try_new`, `ExistsType::try_from`) check
/// caller input.
impl From<ValuesError> for ApiError {
    fn from(error: ValuesError) -> Self {
        Self::invalid_input("invalid filter values", error)
    }
}

impl From<RecordError> for ApiError {
    fn from(error: RecordError) -> Self {
        match error {
            RecordError::NotFound => Self::NotFound,
            RecordError::CellNotFound { name } => Self::CellNotFound { name },
            RecordError::InvalidInput { message, source } => {
                Self::InvalidArgument { message, source }
            }
            RecordError::AlreadyExists => Self::AlreadyExists,
            RecordError::Reserved => Self::Reserved,
            RecordError::Metering(failure) => Self::OutOfBudget {
                required: failure.required,
            },
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
            QueryError::LimitExceeded { limit, max } => Self::LimitExceeded { limit, max },
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
            BranchError::ImmutableData(error) => error.into(),
            error => Self::internal(error),
        }
    }
}

impl From<golemdb_metering::ModelError> for ApiError {
    fn from(error: golemdb_metering::ModelError) -> Self {
        match error {
            golemdb_metering::ModelError::ModelNotFound(_) => Self::NotFound,
            error => Self::internal(error),
        }
    }
}

impl From<ImmutableDataStorageError> for ApiError {
    fn from(error: ImmutableDataStorageError) -> Self {
        match error {
            ImmutableDataStorageError::NotFound => Self::NotFound,
            ImmutableDataStorageError::AlreadyExists => Self::AlreadyExists,
            ImmutableDataStorageError::CommitUnavailable {
                requested,
                published,
            } => Self::CommitUnavailable {
                requested: CommitId::new(requested),
                head: CommitId::new(published),
            },
            // Decoding stored payloads is an internal integrity failure, not bad input.
            ImmutableDataStorageError::Input(
                error @ (ImmutableDataError::InvalidPayload { .. }
                | ImmutableDataError::PayloadArity { .. }),
            ) => Self::internal(error),
            ImmutableDataStorageError::Input(error) => {
                Self::invalid_input(&error.to_string(), error)
            }
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
    use golemdb_pruning::PruningError;
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
                RecordError::Pricing(e) => Some(e),
                _ => None,
            }
        } else if let Some(error) = error.downcast_ref::<BranchError>() {
            match error {
                BranchError::Storage(e) => Some(e),
                BranchError::Cells(e) => Some(e),
                BranchError::Index(e) => Some(e),
                BranchError::ImmutableData(e) => Some(e),
                BranchError::Pruning(e) => Some(e),
                _ => None,
            }
        } else if let Some(ImmutableDataStorageError::Store(error)) =
            error.downcast_ref::<ImmutableDataStorageError>()
        {
            Some(error)
        } else if let Some(golemdb_metering::ModelError::Cells(error)) =
            error.downcast_ref::<golemdb_metering::ModelError>()
        {
            Some(error)
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
        } else if let Some(PruningError::Storage(error)) = error.downcast_ref::<PruningError>() {
            Some(error)
        } else {
            error.source()
        };
    wrapped.is_some_and(store_full)
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{CellType, CellValue, Width};
    use golemdb_query::InValues;

    use super::*;

    #[test]
    fn filter_value_errors_are_invalid_arguments() {
        fn in_values(values: Vec<CellValue>) -> crate::Result<InValues> {
            Ok(InValues::try_from(values)?)
        }
        let error =
            in_values(vec![CellValue::from_u32(1), CellValue::from_u64(1)]).expect_err("two types");
        assert!(matches!(&error, ApiError::InvalidArgument { .. }));
        assert_eq!(
            error.source().unwrap().downcast_ref::<ValuesError>(),
            Some(&ValuesError::MixedTypes {
                expected: CellType::Uint(Width::W4),
                found: CellType::Uint(Width::W8),
            })
        );
    }

    #[test]
    fn a_query_over_a_cap_keeps_the_cap() {
        let error = ApiError::from(QueryError::LimitExceeded {
            limit: "filter depth",
            max: 64,
        });
        assert!(matches!(
            &error,
            ApiError::LimitExceeded {
                limit: "filter depth",
                max: 64
            }
        ));
        assert_eq!(
            error.to_string(),
            "limit exceeded: filter depth is capped at 64"
        );
    }
}
