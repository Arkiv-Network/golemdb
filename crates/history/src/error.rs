use golemdb_storage::{StorageError, Table};

use crate::CommitSetError;

pub type Result<T> = std::result::Result<T, HistoryError>;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("history and changeset tables must differ: {0:?}")]
    AliasedTables(Table),
    #[error("invalid commit set in {table:?} for {key:?}: {source}")]
    InvalidCommits {
        table: Table,
        key: Vec<u8>,
        #[source]
        source: CommitSetError,
    },
    #[error("commit {commit} does not follow {previous} in {table:?} for {key:?}")]
    NonIncreasingCommit {
        table: Table,
        key: Vec<u8>,
        previous: u64,
        commit: u64,
    },
    #[error("pre-image already exists at commit {commit} in {table:?} for {key:?}")]
    ExistingPreimage {
        table: Table,
        key: Vec<u8>,
        commit: u64,
    },
    #[error("missing pre-image at commit {commit} in {table:?} for {key:?}")]
    MissingPreimage {
        table: Table,
        key: Vec<u8>,
        commit: u64,
    },
}
