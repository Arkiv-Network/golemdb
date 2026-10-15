use std::collections::BTreeMap;

use golemdb_branch::{BranchId, CommitId};
use golemdb_cells::{CellName, CellValue};

/// Caller-assigned logical identity, independent of the internal record ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey(pub [u8; 32]);

impl From<[u8; 32]> for RecordKey {
    fn from(key: [u8; 32]) -> Self {
        Self(key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTarget {
    /// Resolve the current head and read its cells from one committed snapshot.
    Head,
    /// Current work in progress, including all successful uncommitted mutations.
    Branch(BranchId),
    /// An explicit committed snapshot. Only the head of the acquired storage
    /// snapshot is supported; older and future IDs return
    /// [`crate::RecordError::CommitUnavailable`].
    Commit(CommitId),
}

pub type RecordCells = BTreeMap<CellName, CellValue>;
/// Caller-requested edits to one record, keyed by within-record cell names.
pub type RecordPatch = BTreeMap<CellName, CellPatch>;

/// One edit in a record patch, before resolving the record's internal ID.
/// Storage-level mutations use `golemdb_cells::CellChange` with complete keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellPatch {
    Set(CellValue),
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: RecordKey,
    /// Byte-ordered names; raw binary names are possible in reserved records.
    pub cells: RecordCells,
}
