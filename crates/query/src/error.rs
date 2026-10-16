use std::error::Error;

use golemdb_branch::CommitId;
use golemdb_cells::CellName;
use golemdb_index::{IndexError, TermError};
use golemdb_record::RecordError;

pub type Result<T> = std::result::Result<T, QueryError>;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// A leaf names no valid index term: a bad field name, or a value the
    /// index cannot hold (variable-length `bytes`). Raised at admission,
    /// before any state read.
    #[error("invalid argument: field `{field}`: {source}")]
    InvalidArgument {
        field: CellName,
        #[source]
        source: TermError,
    },
    /// The request cannot be run as written, e.g. it targets a branch.
    #[error("invalid query: {reason}")]
    InvalidQuery { reason: &'static str },
    /// A part of the query API that is specified but not built yet.
    #[error("operation {operation} is not implemented")]
    NotImplemented { operation: &'static str },
    /// Only the head of the read snapshot can be queried for now.
    #[error("commit {requested} unavailable: only current head {head} is supported")]
    CommitUnavailable { requested: CommitId, head: CommitId },
    /// Reading committed state failed, or found it inconsistent.
    #[error("internal error")]
    Internal(#[source] Box<dyn Error + Send + Sync>),
}

impl From<IndexError> for QueryError {
    fn from(error: IndexError) -> Self {
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
