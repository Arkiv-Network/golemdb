use crate::Hash;
use golemdb_storage::StorageError;

#[derive(Debug, thiserror::Error)]
pub enum MerkleError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("invalid Merkle branch: {0}")]
    InvalidNode(&'static str),
    #[error("missing Merkle branch {0:02x?}")]
    MissingBranch(Hash),
    #[error("Merkle branch does not match its content hash")]
    HashMismatch,
    #[error("Merkle leaf differs from the expected one")]
    LeafMismatch,
}

pub type Result<T> = std::result::Result<T, MerkleError>;
