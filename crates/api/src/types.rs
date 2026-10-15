use crate::CommitId;

/// Public seal result. The commit ID is provisional until successful commit;
/// internal cell/index diffs and buffered storage rows are not exposed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealInfo {
    pub commit_id: CommitId,
    pub state_root: [u8; 32],
    pub index_root: [u8; 32],
}

impl From<&golemdb_branch::SealedCommit> for SealInfo {
    fn from(sealed: &golemdb_branch::SealedCommit) -> Self {
        Self {
            commit_id: sealed.commit_id,
            state_root: sealed.state_root,
            index_root: sealed.index_root,
        }
    }
}
