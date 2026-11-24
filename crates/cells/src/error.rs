//! [`CellValueParseError`]: why bytes are not a valid cell value.

use core::fmt;

use crate::types::CellType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellValueParseError {
    /// No metadata byte.
    Empty,
    /// Type id `0x00`, the absent marker. See the crate docs.
    AbsentTag,
    /// A type id the spec reserves.
    ReservedType(u8),
    /// A fixed-width value of the wrong width.
    LengthMismatch {
        ty: CellType,
        expected: usize,
        actual: usize,
    },
    /// A `bool` byte other than 0 or 1.
    InvalidBool(u8),
    /// A `str` that stops being UTF-8 at byte `valid_up_to`.
    InvalidUtf8 { valid_up_to: usize },
    /// A NaN float: many bit patterns, no order.
    FloatNaN,
    /// A `-0.0` float: zero is stored as `+0.0` only.
    NegativeZero,
    /// The indexable bit on a `bytes` cell, which has no order.
    NotIndexable,
    /// A `0x00` byte in an attribute `str`: the index ends a string value
    /// with `0x00`.
    NulInStrAttributeValue,
}

impl fmt::Display for CellValueParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "cell is empty"),
            Self::AbsentTag => write!(f, "type id 0 is the absent marker, not a type"),
            Self::ReservedType(id) => write!(f, "type id {id} is reserved"),
            Self::LengthMismatch {
                ty,
                expected,
                actual,
            } => write!(f, "{ty:?} expects {expected} value bytes, got {actual}"),
            Self::InvalidBool(b) => write!(f, "bool byte must be 0 or 1, got {b}"),
            Self::InvalidUtf8 { valid_up_to } => {
                write!(f, "str is not valid UTF-8 at byte {valid_up_to}")
            }
            Self::FloatNaN => write!(f, "float is NaN, which has no order"),
            Self::NegativeZero => write!(f, "float is -0.0; zero is stored as +0.0"),
            Self::NotIndexable => write!(f, "a bytes cell cannot be indexable"),
            Self::NulInStrAttributeValue => {
                write!(f, "an attribute str cannot contain a 0x00 byte")
            }
        }
    }
}

impl core::error::Error for CellValueParseError {}

/// Failures reading or updating cell state. Mutation errors require aborting
/// the caller's write transaction; previously applied changes are not undone.
#[derive(Debug, thiserror::Error)]
pub enum CellError {
    #[error(transparent)]
    Storage(#[from] golemdb_storage::StorageError),
    #[error(transparent)]
    Merkle(#[from] golemdb_merkle::MerkleError),
    #[error("invalid stored cell key: {0}")]
    Key(#[from] crate::CellKeyError),
    #[error("invalid stored cell value: {0}")]
    Value(#[from] CellValueParseError),
    #[error("cell state does not match the supplied root")]
    RootMismatch,
}

pub type Result<T> = std::result::Result<T, CellError>;
