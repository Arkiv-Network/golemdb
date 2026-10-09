use golemdb_index::{IndexError, TermError};

pub type Result<T> = std::result::Result<T, QueryError>;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The predicate names no valid index term: a bad field name, or a value
    /// the index cannot hold (NaN).
    #[error("invalid predicate on `{field}`: {source}")]
    InvalidPredicate {
        field: String,
        #[source]
        source: TermError,
    },
    #[error(transparent)]
    Index(#[from] IndexError),
}
