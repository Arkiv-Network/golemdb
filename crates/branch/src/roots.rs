use golemdb_cells::CELL_TRIE_PATH_BYTES;
use golemdb_index::INDEX_TRIE_PATH_BYTES;
use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{BranchError, CommitId, Head, Result};

/// Every published commit's roots, by commit number: physical locators that
/// make old tries walkable. `#roots` commits only their hashes.
const ROOTS: Table = Table("Roots");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roots {
    pub state: RootRef<CELL_TRIE_PATH_BYTES>,
    pub index: RootRef<INDEX_TRIE_PATH_BYTES>,
}

/// Stage a commit's roots with its head. Commits are written in order.
pub fn write_roots(tx: &mut impl WriteTransaction, commit: CommitId, roots: &Roots) -> Result<()> {
    let state = roots.state.encode();
    let value = [&[state.len() as u8][..], &state, &roots.index.encode()].concat();
    tx.append(ROOTS, &commit.get().to_be_bytes(), &value)?;
    Ok(())
}

/// The roots `commit` published, if this store has them.
pub fn read_roots(tx: &impl ReadTransaction, commit: CommitId) -> Result<Option<Roots>> {
    let Some(bytes) = tx.get(ROOTS, &commit.get().to_be_bytes())? else {
        return Ok(None);
    };
    let invalid = || BranchError::InvalidRoots(commit.get());
    let (len, rest) = bytes.split_first().ok_or_else(invalid)?;
    let (state, index) = rest.split_at_checked(*len as usize).ok_or_else(invalid)?;
    Ok(Some(Roots {
        state: RootRef::decode(state).map_err(|_| invalid())?,
        index: RootRef::decode(index).map_err(|_| invalid())?,
    }))
}

/// The commits that stored the head's branch roots, from `Roots[head]`, checked
/// against the head's hashes. Only a head whose tries are both empty may lack
/// the row.
pub fn root_commits(
    tx: &impl ReadTransaction,
    head: &Head,
    hasher: &impl HashProvider,
) -> Result<(Option<u64>, Option<u64>)> {
    let commit = head.commit_id.get();
    let Some(roots) = read_roots(tx, head.commit_id)? else {
        let empty = hasher.hash(&[]);
        if head.state_root == empty && head.index_root == empty {
            return Ok((None, None));
        }
        return Err(BranchError::MissingRoots(commit));
    };
    // Every commit writes a `#roots` cell, so a branch state root is always
    // written at the head's own commit.
    if roots.state.hash(hasher) != head.state_root
        || roots.index.hash(hasher) != head.index_root
        || roots
            .state
            .branch_commit()
            .is_some_and(|stored| stored != commit)
    {
        return Err(BranchError::InvalidRoots(commit));
    }
    Ok((roots.state.branch_commit(), roots.index.branch_commit()))
}
