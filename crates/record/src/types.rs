use std::collections::BTreeMap;

use golemdb_branch::{BranchId, CommitId};
use golemdb_cells::{CellKind, CellName, CellValue};

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
/// Prepared edits keyed by cell name, produced by execution-time admission.
/// Callers supply ordered [`PatchEntry`] values to [`crate::Records::patch`].
pub type RecordPatch = BTreeMap<CellName, CellPatch>;

/// One unvalidated create entry. `kind` is the requested kind; `value` retains
/// its original encoding until admission. Names may contain arbitrary bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateEntry {
    pub name: CellName,
    pub value: CellValue,
    pub kind: CellKind,
}

/// One unvalidated patch entry, preserved in caller order, including duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchEntry {
    pub name: CellName,
    pub change: PatchChange,
}

/// Requested patch action before admission or stored-kind resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchChange {
    Write { value: CellValue, kind: CellKind },
    Set(CellValue),
    Remove,
}

/// One prepared edit, before resolving the record's internal ID and stored kind.
/// Storage-level mutations use `golemdb_cells::CellChange` with complete keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellPatch {
    /// Explicitly write both value and kind, creating a missing cell.
    Write(CellValue),
    /// Replace an existing value, preserving its stored kind.
    Set(CellValue),
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: RecordKey,
    /// Byte-ordered names; raw binary names are possible in reserved records.
    pub cells: RecordCells,
}
