use golemdb_cells::{CellType, CellValueParseError};

/// Input and payload failures. Storage and branch failures belong to later
/// increments; malformed stored payloads remain distinct from invalid input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImmutableDataError {
    #[error("segment name must not be empty")]
    EmptySegmentName,
    #[error(
        "invalid segment name {name:?}: expected 1–64 ASCII characters, starting with a lowercase letter and containing only lowercase letters, digits or underscores"
    )]
    InvalidSegmentName { name: String },
    #[error("a segment must have at least one column")]
    EmptySchema,
    #[error("column {column} has an empty name")]
    EmptyColumnName { column: usize },
    #[error("column {name:?} is declared more than once")]
    DuplicateColumn { name: String },
    #[error("row needs {expected} values, got {actual}")]
    RowArity { expected: usize, actual: usize },
    #[error("column {column} expects {expected:?}, got {actual:?}")]
    TypeMismatch {
        column: usize,
        expected: CellType,
        actual: CellType,
    },
    #[error("an explicit projection must not be empty")]
    EmptyProjection,
    #[error("unknown column {name:?}")]
    UnknownColumn { name: String },
    #[error("column {name:?} was not selected in this row")]
    ColumnNotSelected { name: String },
    #[error("named access requires a decoded read row; append rows have no column metadata")]
    UnnamedRow,
    #[error("column {name:?} is projected more than once")]
    DuplicateProjection { name: String },
    #[error("expected {expected} column payloads, got {actual}")]
    PayloadArity { expected: usize, actual: usize },
    #[error("invalid payload for column {column} ({name:?}): {source}")]
    InvalidPayload {
        column: usize,
        name: String,
        #[source]
        source: CellValueParseError,
    },
    #[error("cannot convert column {column} of type {value_type:?} to the requested Rust value")]
    NativeConversion { column: usize, value_type: CellType },
}

pub type Result<T> = std::result::Result<T, ImmutableDataError>;
