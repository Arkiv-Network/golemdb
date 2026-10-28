use std::error::Error;

use golemdb_branch::CommitId;
use golemdb_cells::{CellError, CellName};
use golemdb_index::{IndexError, TermError};
use golemdb_record::RecordError;

pub type Result<T> = std::result::Result<T, QueryError>;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// A filter leaf or a sort key names an invalid cell: a bad field name.
    /// Their values and types are indexable by construction. Raised at
    /// admission, before index or record reads.
    #[error("invalid argument: field `{field}`: {source}")]
    InvalidArgument {
        field: CellName,
        #[source]
        source: TermError,
    },
    /// The request cannot be run as written: an `And` or `Or` with no child,
    /// or a `limit` of 0. Raised at admission, before index or record reads.
    #[error("invalid query: {reason}")]
    InvalidQuery { reason: &'static str },
    /// The request exceeds a cap, named by `limit`, whose value is `max`: a
    /// filter nested deeper than the `max_query_depth` of its
    /// [`Queries`](crate::Queries) (`"filter depth"`), a `limit` above its
    /// `max_query_limit` (`"query limit"`), or more sort keys than its
    /// `max_sort_keys` (`"sort key count"`). Raised at admission, before index
    /// or record reads.
    #[error("limit exceeded: {limit} is capped at {max}")]
    LimitExceeded { limit: &'static str, max: u32 },
    /// A part of the query API that is specified but not built yet.
    #[error("operation {operation} is not implemented")]
    NotImplemented { operation: &'static str },
    /// Only the head of the read snapshot can be queried for now.
    #[error("commit {requested} unavailable: only current head {head} is supported")]
    CommitUnavailable { requested: CommitId, head: CommitId },
    /// Reading committed state failed, or found it inconsistent; or an
    /// operand's invariant was broken, a bug rather than the caller's error.
    #[error("internal error")]
    Internal(#[source] Box<dyn Error + Send + Sync>),
}

impl From<IndexError> for QueryError {
    fn from(error: IndexError) -> Self {
        Self::Internal(Box::new(error))
    }
}

/// A cell that cannot be read, such as a sort value, is internal to a
/// query, as a posting list that cannot be read is.
impl From<CellError> for QueryError {
    fn from(error: CellError) -> Self {
        Self::Internal(Box::new(error))
    }
}

/// A record's own errors are internal to a query, except for an unavailable
/// commit, which keeps its meaning.
impl From<RecordError> for QueryError {
    fn from(error: RecordError) -> Self {
        match error {
            RecordError::CommitUnavailable { requested, head } => {
                Self::CommitUnavailable { requested, head }
            }
            error => Self::Internal(Box::new(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_error_is_internal_and_kept_as_the_cause() {
        let error = QueryError::from(CellError::RootMismatch);
        let QueryError::Internal(source) = &error else {
            panic!("expected Internal, got {error:?}");
        };
        assert!(matches!(
            source.downcast_ref::<CellError>(),
            Some(CellError::RootMismatch)
        ));
    }
}
