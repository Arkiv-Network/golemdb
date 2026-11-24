use golemdb_branch::{BranchError, CommitId, OperationError};
use golemdb_cells::CellName;

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("record not found")]
    NotFound,
    #[error("cell {name} not found")]
    CellNotFound { name: golemdb_cells::CellName },
    #[error("record key already exists")]
    AlreadyExists,
    #[error("reserved records cannot be modified through CRUD")]
    Reserved,
    #[error("invalid argument: {message}")]
    InvalidInput {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
    #[error("record effect count overflow")]
    AccountingOverflow,
    #[error(transparent)]
    Metering(#[from] golemdb_metering::MeteringFailure),
    /// Failure to obtain a usable committed schedule, before admission begins.
    #[error(transparent)]
    Pricing(#[from] golemdb_metering::ModelError),
    #[error("missing or inconsistent initialized record state: {0}")]
    CorruptState(&'static str),
    #[error("record ID space exhausted")]
    RecordIdExhausted,
    #[error("commit {requested} unavailable: only current head {head} is supported")]
    CommitUnavailable { requested: CommitId, head: CommitId },
    #[error(transparent)]
    Cells(#[from] golemdb_cells::CellError),
    #[error(transparent)]
    Storage(#[from] golemdb_storage::StorageError),
    #[error(transparent)]
    Branch(#[from] BranchError),
}

impl RecordError {
    /// `step("name"): message`, the form every step error takes.
    pub(crate) fn invalid_step(
        step: &str,
        name: &CellName,
        message: &str,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        // A length rejection must not scan/copy an arbitrarily large name just to
        // format its diagnostic. The typed source still carries the full length.
        let bytes = name.as_bytes();
        let shown = &bytes[..bytes.len().min(64)];
        let suffix = if shown.len() < bytes.len() { "..." } else { "" };
        Self::InvalidInput {
            message: format!("{step}(\"{}{suffix}\"): {message}", shown.escape_ascii()),
            source,
        }
    }
}

impl From<OperationError<RecordError>> for RecordError {
    fn from(error: OperationError<RecordError>) -> Self {
        match error {
            OperationError::Branch(error) => Self::Branch(error),
            OperationError::Operation(error) => error,
        }
    }
}

pub type Result<T> = std::result::Result<T, RecordError>;
