use golemdb_cells::CellName;
use golemdb_index::{IndexError, TermError};

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
    /// Reading the index failed.
    #[error("internal index error")]
    Internal(#[from] IndexError),
}
