use crate::Table;
use std::{error::Error, fmt};

#[derive(Debug)]
pub enum StorageError {
    AlreadyExists,
    /// The store has reached its configured capacity.
    Full,
    InvalidTableName(Table),
    InvalidRange,
    /// Backend physical limit, not a required constructor configuration.
    KeyTooLarge {
        actual: usize,
        max: usize,
    },
    /// Backend physical limit, not a required constructor configuration.
    ValueTooLarge {
        actual: usize,
        max: usize,
    },
    Poisoned(&'static str),
    Backend(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("storage capacity exhausted"),
            Self::AlreadyExists => f.write_str("key already exists"),
            Self::InvalidTableName(t) => write!(f, "invalid table name: {:?}", t.0),
            Self::InvalidRange => f.write_str("lower bound exceeds upper bound"),
            Self::KeyTooLarge { actual, max } => write!(f, "key size {actual} exceeds {max}"),
            Self::ValueTooLarge { actual, max } => write!(f, "value size {actual} exceeds {max}"),
            Self::Poisoned(lock) => write!(f, "storage {lock} lock poisoned"),
            Self::Backend(source) => write!(f, "storage backend error: {source}"),
        }
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(source) => Some(source.as_ref()),
            _ => None,
        }
    }
}
