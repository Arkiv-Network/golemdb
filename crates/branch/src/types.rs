/// Canonical commit number: zero is genesis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct CommitId(u64);

impl CommitId {
    pub const GENESIS: Self = Self(0);
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for CommitId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}
impl From<CommitId> for u64 {
    fn from(value: CommitId) -> Self {
        value.get()
    }
}
impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Volatile, process-local branch identifier.
///
/// An ID is valid only in the manager that issued it (or one of its clones),
/// while that branch exists and its origin remains the head. Do not persist
/// IDs or reuse them after a process restart. IDs are allocated
/// monotonically across managers in this process and are never reused.
/// The origin is retained by the manager and returned by `branch_info`.
///
/// Branch and commit IDs cannot be interchanged:
/// ```compile_fail
/// use golemdb_branch::{BranchId, CommitId};
/// let branch: BranchId = CommitId::GENESIS;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct BranchId(u64);

impl BranchId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for BranchId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}
impl From<BranchId> for u64 {
    fn from(value: BranchId) -> Self {
        value.get()
    }
}
impl std::fmt::Display for BranchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A consistent snapshot of one live branch's metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    /// Head over which this branch was opened.
    pub commit_id: CommitId,
    pub branch_id: BranchId,
    /// Current number of undo entries, not a monotonically increasing revision.
    /// Rollback reduces this count; failed operations restore it. An identical
    /// write to an existing overlay entry adds no undo entry.
    pub version: u64,
    /// Whether seal has computed and frozen the branch.
    pub sealed: bool,
}
