use std::{error::Error, fmt};

pub type Result<T> = std::result::Result<T, IndexError>;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Storage(#[from] golemdb_storage::StorageError),
    #[error(transparent)]
    Merkle(#[from] golemdb_merkle::MerkleError),
    #[error(transparent)]
    Bitmap(#[from] BitmapError),
    #[error(transparent)]
    Term(#[from] crate::TermError),
    #[error("corrupt index: {0}")]
    Corruption(&'static str),
    #[error("index root disagrees with the flat term table")]
    RootMismatch,
}

#[derive(Debug)]
pub enum BitmapError {
    PathOutOfRange { path: u64 },
    OffsetOutOfRange { value: u32 },
    DuplicatePath { path: u64 },
    UnsortedPaths { previous: u64, path: u64 },
    EmptyContainer,
    TruncatedPath,
    NonCanonicalEncoding,
    Decode(std::io::Error),
}

impl fmt::Display for BitmapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathOutOfRange { path } => write!(f, "bitmap path {path} exceeds 48 bits"),
            Self::OffsetOutOfRange { value } => write!(f, "bitmap offset {value} exceeds 16 bits"),
            Self::DuplicatePath { path } => write!(f, "duplicate bitmap path {path}"),
            Self::UnsortedPaths { previous, path } => {
                write!(f, "bitmap path {path} follows larger path {previous}")
            }
            Self::EmptyContainer => f.write_str("empty containers have no persisted encoding"),
            Self::TruncatedPath => f.write_str("container is missing its six-byte path"),
            Self::NonCanonicalEncoding => f.write_str("noncanonical container encoding"),
            Self::Decode(source) => write!(f, "invalid Roaring container: {source}"),
        }
    }
}

impl Error for BitmapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(source) => Some(source),
            _ => None,
        }
    }
}
