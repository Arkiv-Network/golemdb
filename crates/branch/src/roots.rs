use golemdb_cells::CELL_TRIE_PATH_BYTES;
use golemdb_index::INDEX_TRIE_PATH_BYTES;
use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{BranchError, CommitId, Head, Result};

/// Every published commit's roots, by commit number: physical locators that
/// make old tries walkable. `#roots` commits only their hashes.
pub const ROOTS: Table = Table("Roots");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roots {
    pub state: RootRef<CELL_TRIE_PATH_BYTES>,
    pub index: RootRef<INDEX_TRIE_PATH_BYTES>,
}

impl Roots {
    /// `len(state) ‖ state ‖ index`, each root as [`RootRef::encode`] writes it.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let state = self.state.encode();
        [&[state.len() as u8][..], &state, &self.index.encode()].concat()
    }

    /// `None` for malformed bytes.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let (len, rest) = bytes.split_first()?;
        let (state, index) = rest.split_at_checked(*len as usize)?;
        Some(Self {
            state: RootRef::decode(state).ok()?,
            index: RootRef::decode(index).ok()?,
        })
    }
}

/// Stage a commit's roots with its head. Commits are written in order.
pub(crate) fn put_roots(
    tx: &mut impl WriteTransaction,
    commit: CommitId,
    roots: &Roots,
) -> Result<()> {
    tx.append(ROOTS, &commit.get().to_be_bytes(), &roots.encode())?;
    Ok(())
}

/// The roots `commit` published, if this store has them.
pub fn read_roots(tx: &impl ReadTransaction, commit: CommitId) -> Result<Option<Roots>> {
    tx.get(ROOTS, &commit.get().to_be_bytes())?
        .map(|bytes| Roots::decode(&bytes).ok_or(BranchError::InvalidRoots(commit.get())))
        .transpose()
}

/// The head's roots, from `Roots[head]`, checked against the head's hashes.
/// Only a head whose tries are both empty may lack the row.
pub fn head_roots(
    tx: &impl ReadTransaction,
    head: &Head,
    hasher: &impl HashProvider,
) -> Result<Roots> {
    let commit = head.commit_id.get();
    let Some(roots) = read_roots(tx, head.commit_id)? else {
        let empty = hasher.hash(&[]);
        if head.state_root == empty && head.index_root == empty {
            return Ok(Roots {
                state: RootRef::Empty,
                index: RootRef::Empty,
            });
        }
        return Err(BranchError::MissingRoots(commit));
    };
    if roots.state.hash(hasher) != head.state_root || roots.index.hash(hasher) != head.index_root {
        return Err(BranchError::InvalidRoots(commit));
    }
    Ok(roots)
}
