use std::error::Error;

use golemdb_branch::BranchError;
use golemdb_cells::{CellNameError, CellValueParseError};
use golemdb_immutable_data::{ImmutableDataError, ImmutableDataStorageError};
use golemdb_metering::ModelError;
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
/// `InValues::try_from`, `Bounds::try_new`, `ExistsType::try_from`,
/// `SortKey::try_new`) check caller input.
impl From<ValuesError> for ApiError {
    fn from(error: ValuesError) -> Self {
        Self::invalid_input("invalid query operand", error)
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
            error @ (RecordError::AccountingOverflow
            | RecordError::Pricing(_)
            | RecordError::CorruptState(_)
            | RecordError::RecordIdExhausted
            | RecordError::Cells(_)
            | RecordError::Storage(_)) => Self::internal(error),
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::InvalidArgument { field, source } => Self::InvalidArgument {
                message: format!("invalid cell `{field}` in a filter or sort key"),
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
            error @ (BranchError::InvalidScanState(_)
            | BranchError::CommitNumberExhausted
            | BranchError::RootsCellExists
            | BranchError::Cells(_)
            | BranchError::Index(_)
            | BranchError::Term(_)
            | BranchError::MissingHead
            | BranchError::InvalidHead { .. }
            | BranchError::MissingRoots(_)
            | BranchError::InvalidRoots(_)
            | BranchError::InvalidRootsKey(_)
            | BranchError::CommitNotNewer { .. }
            | BranchError::HeadExists
            | BranchError::Pruning(_)
            | BranchError::BranchNumberExhausted
            | BranchError::Poisoned(_)
            | BranchError::Storage(_)
            | BranchError::Key(_)
            | BranchError::Value(_)
            | BranchError::RecoveryRequired
            | BranchError::PublicationRecovery { .. }) => Self::internal(error),
        }
    }
}

impl From<ModelError> for ApiError {
    fn from(error: ModelError) -> Self {
        match error {
            ModelError::ModelNotFound(_) => Self::NotFound,
            error @ (ModelError::InvalidConfig(_)
            | ModelError::CorruptState(_)
            | ModelError::UnsupportedGolemDbCode(_)
            | ModelError::UnsupportedHostCode(_)
            | ModelError::ReloadRequired(_)
            | ModelError::Cells(_)
            | ModelError::Schedule(_)) => Self::internal(error),
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
            // ImmutableDataError is non-exhaustive outside its owning crate.
            ImmutableDataStorageError::Input(error) => {
                Self::invalid_input(&error.to_string(), error)
            }
            // ImmutableDataStorageError is also non-exhaustive.
            error => Self::internal(error),
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

// Transparent internal errors delegate source() to their wrapped error, which
// can hide a source-less StorageError::Full. Follow those wrappers explicitly.
// Keep matches exhaustive so new variants require an explicit classification.
fn store_full(error: &(dyn Error + 'static)) -> bool {
    use golemdb_cells::CellError;
    use golemdb_history::HistoryError;
    use golemdb_index::IndexError;
    use golemdb_merkle::MerkleError;
    use golemdb_pruning::PruningError;
    use golemdb_storage::StorageError;
    let wrapped: Option<&(dyn Error + 'static)> =
        if let Some(error) = error.downcast_ref::<StorageError>() {
            match error {
                StorageError::Full => return true,
                StorageError::Backend(source) => Some(source.as_ref()),
                StorageError::AlreadyExists
                | StorageError::OutOfOrder
                | StorageError::InvalidTableName(_)
                | StorageError::InvalidRange
                | StorageError::KeyTooLarge { .. }
                | StorageError::ValueTooLarge { .. }
                | StorageError::Poisoned(_) => None,
            }
        } else if let Some(error) = error.downcast_ref::<RecordError>() {
            match error {
                RecordError::Storage(e) => Some(e),
                RecordError::Cells(e) => Some(e),
                RecordError::Branch(e) => Some(e),
                RecordError::Pricing(e) => Some(e),
                RecordError::NotFound
                | RecordError::CellNotFound { .. }
                | RecordError::AlreadyExists
                | RecordError::Reserved
                | RecordError::InvalidInput { .. }
                | RecordError::AccountingOverflow
                | RecordError::Metering(_)
                | RecordError::CorruptState(_)
                | RecordError::RecordIdExhausted
                | RecordError::CommitUnavailable { .. } => None,
            }
        } else if let Some(error) = error.downcast_ref::<BranchError>() {
            match error {
                BranchError::Storage(e) => Some(e),
                BranchError::Cells(e) => Some(e),
                BranchError::Index(e) => Some(e),
                BranchError::ImmutableData(e) => Some(e),
                BranchError::Pruning(e) => Some(e),
                BranchError::InvalidScanState(_)
                | BranchError::HandleInvalid
                | BranchError::Conflict
                | BranchError::Sealed
                | BranchError::CommitNumberExhausted
                | BranchError::RootsCellExists
                | BranchError::Term(_)
                | BranchError::MissingHead
                | BranchError::InvalidHead { .. }
                | BranchError::MissingRoots(_)
                | BranchError::InvalidRoots(_)
                | BranchError::InvalidRootsKey(_)
                | BranchError::CommitNotNewer { .. }
                | BranchError::HeadExists
                | BranchError::BranchNumberExhausted
                | BranchError::Poisoned(_)
                | BranchError::Key(_)
                | BranchError::Value(_)
                | BranchError::NoFrameToRollback
                | BranchError::RecoveryRequired
                | BranchError::PublicationRecovery { .. } => None,
            }
        } else if let Some(error) = error.downcast_ref::<ImmutableDataStorageError>() {
            match error {
                ImmutableDataStorageError::Store(error) => Some(error),
                // This enum is non-exhaustive; retain generic source traversal.
                _ => error.source(),
            }
        } else if let Some(error) = error.downcast_ref::<ModelError>() {
            match error {
                ModelError::Cells(error) => Some(error),
                ModelError::InvalidConfig(_)
                | ModelError::CorruptState(_)
                | ModelError::UnsupportedGolemDbCode(_)
                | ModelError::UnsupportedHostCode(_)
                | ModelError::ModelNotFound(_)
                | ModelError::ReloadRequired(_)
                | ModelError::Schedule(_) => None,
            }
        } else if let Some(error) = error.downcast_ref::<CellError>() {
            match error {
                CellError::History(e) => Some(e),
                CellError::Storage(e) => Some(e),
                CellError::Merkle(e) => Some(e),
                CellError::HistoricalScanUnavailable
                | CellError::Key(_)
                | CellError::Value(_)
                | CellError::RootMismatch => None,
            }
        } else if let Some(error) = error.downcast_ref::<IndexError>() {
            match error {
                IndexError::History(e) => Some(e),
                IndexError::Storage(e) => Some(e),
                IndexError::Merkle(e) => Some(e),
                IndexError::Bitmap(_)
                | IndexError::Term(_)
                | IndexError::Corruption(_)
                | IndexError::RootMismatch => None,
            }
        } else if let Some(error) = error.downcast_ref::<HistoryError>() {
            match error {
                HistoryError::Storage(error) => Some(error),
                HistoryError::EmptyPreimage
                | HistoryError::InvalidCommits { .. }
                | HistoryError::NonIncreasingCommit { .. }
                | HistoryError::ExistingPreimage { .. }
                | HistoryError::MissingPreimage { .. } => None,
            }
        } else if let Some(error) = error.downcast_ref::<MerkleError>() {
            match error {
                MerkleError::Storage(error) => Some(error),
                MerkleError::InvalidNode(_)
                | MerkleError::MissingBranch(_)
                | MerkleError::HashMismatch
                | MerkleError::LeafMismatch => None,
            }
        } else if let Some(error) = error.downcast_ref::<PruningError>() {
            match error {
                PruningError::Storage(error) => Some(error),
                PruningError::UnversionedTable(_) => None,
            }
        } else {
            error.source()
        };
    wrapped.is_some_and(store_full)
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{CellError, CellType, CellValue, Width};
    use golemdb_history::HistoryError;
    use golemdb_index::IndexError;
    use golemdb_metering::ModelError;
    use golemdb_query::InValues;
    use golemdb_storage::{StorageError, Table};

    use super::*;

    #[test]
    fn history_capacity_errors_remain_public_through_wrappers() {
        let full = || HistoryError::Storage(StorageError::Full);
        for error in [
            ApiError::internal(full()),
            ApiError::internal(CellError::History(full())),
            ApiError::internal(IndexError::History(full())),
            ApiError::from(RecordError::Cells(CellError::History(full()))),
            ApiError::from(BranchError::Cells(CellError::History(full()))),
            ApiError::from(BranchError::Index(IndexError::History(full()))),
            ApiError::from(RecordError::Branch(BranchError::Index(
                IndexError::History(full()),
            ))),
            ApiError::from(ModelError::Cells(CellError::History(full()))),
            ApiError::from(RecordError::Pricing(ModelError::Cells(CellError::History(
                full(),
            )))),
            ApiError::from(QueryError::Internal(Box::new(CellError::History(full())))),
            ApiError::from(QueryError::Internal(Box::new(IndexError::History(full())))),
        ] {
            assert!(matches!(error, ApiError::StoreFull), "{error:?}");
        }
    }

    #[test]
    fn non_capacity_history_errors_keep_diagnostic_sources() {
        for history in [
            HistoryError::EmptyPreimage,
            HistoryError::MissingPreimage {
                table: Table("CellChangeSet"),
                key: b"item".to_vec(),
                commit: 10,
            },
            HistoryError::Storage(StorageError::Backend("disk error".into())),
        ] {
            let message = history.to_string();
            let error = ApiError::from(RecordError::Cells(CellError::History(history)));
            assert!(matches!(&error, ApiError::Internal { .. }));
            let source = error.source().unwrap();
            assert_eq!(source.to_string(), message);
            assert!(matches!(
                source.downcast_ref::<RecordError>(),
                Some(RecordError::Cells(CellError::History(_)))
            ));
        }
    }

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
