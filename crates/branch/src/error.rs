/// Failures admitting branch operations, accessing cells or immutable staging,
/// moving through frames, or publishing a branch.
#[derive(Debug, thiserror::Error)]
pub enum BranchError {
    #[error("branch ID is unknown, discarded, or no longer over the head")]
    HandleInvalid,
    #[error("branch origin is no longer the head; commit lost the publication race")]
    Conflict,
    #[error("branch is sealed")]
    Sealed,
    #[error("commit number space exhausted")]
    CommitNumberExhausted,
    #[error("the previous commit's #roots cell already exists")]
    RootsCellExists,
    #[error(transparent)]
    Cells(#[from] golemdb_cells::CellError),
    #[error(transparent)]
    Index(#[from] golemdb_index::IndexError),
    #[error(transparent)]
    Term(#[from] golemdb_index::TermError),
    #[error("database has no Superblock/head row; initialize genesis before opening branches")]
    MissingHead,
    #[error("Superblock/head must be 72 bytes, got {actual}")]
    InvalidHead { actual: usize },
    #[error("no Roots row for head commit {0}")]
    MissingRoots(u64),
    #[error("Roots row for commit {0} is malformed or does not match the head")]
    InvalidRoots(u64),
    #[error("branch ID space exhausted")]
    BranchNumberExhausted,
    #[error("branch {0} lock poisoned")]
    Poisoned(&'static str),
    #[error(transparent)]
    Storage(#[from] golemdb_storage::StorageError),
    #[error("invalid stored cell key: {0}")]
    Key(#[from] golemdb_cells::CellKeyError),
    #[error("invalid stored cell value: {0}")]
    Value(#[from] golemdb_cells::CellValueParseError),
    #[error("no frame remains to roll back")]
    NoFrameToRollback,
    #[error(transparent)]
    ImmutableData(#[from] golemdb_immutable_data::ImmutableDataStorageError),
    #[error(
        "immutable publication outcome or rollback is unresolved; reopen and recover before continuing"
    )]
    RecoveryRequired,
    #[error("immutable publication failed ({publication}); recovery also failed ({recovery})")]
    PublicationRecovery {
        publication: Box<BranchError>,
        #[source]
        recovery: Box<BranchError>,
    },
}

/// Distinguishes handle/head admission failures from a callback's own error.
/// The latter is returned unchanged; it need not be a branch error.
#[derive(Debug, thiserror::Error)]
pub enum OperationError<E> {
    #[error(transparent)]
    Branch(BranchError),
    #[error(transparent)]
    Operation(E),
}

pub type Result<T> = std::result::Result<T, BranchError>;
