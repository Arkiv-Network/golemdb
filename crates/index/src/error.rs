use crate::ContainerId;
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
    #[error("index state does not match the supplied root")]
    RootMismatch,
}

#[derive(Debug)]
pub enum BitmapError {
    OffsetOutOfRange {
        value: u32,
    },
    DuplicateContainer {
        container_id: ContainerId,
    },
    UnsortedContainers {
        previous: ContainerId,
        container_id: ContainerId,
    },
    EmptyContainer,
    NonCanonicalEncoding,
    Decode(std::io::Error),
}

impl fmt::Display for BitmapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OffsetOutOfRange { value } => write!(f, "bitmap offset {value} exceeds 16 bits"),
            Self::DuplicateContainer { container_id } => {
                write!(f, "duplicate container {container_id}")
            }
            Self::UnsortedContainers {
                previous,
                container_id,
            } => {
                write!(
                    f,
                    "container {container_id} follows larger container {previous}"
                )
            }
            Self::EmptyContainer => f.write_str("empty containers have no persisted encoding"),
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
