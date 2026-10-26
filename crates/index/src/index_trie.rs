use golemdb_merkle::{CheckedEdit, Hash, HashProvider, LeafRef, MerkleError, RootRef, Trie};
use golemdb_storage::{ReadCursor, ReadTransaction, WriteTransaction};

use crate::{
    BitmapContainer, HI48_BYTES, INDEX_BRANCH_DOMAIN, INDEX_TRIE_PATH_BYTES, IndexError, IndexTerm,
    Result, container, path, tables,
};

/// A container's index trie path: its term's routing path, then its `hi48`.
pub(crate) type TriePath = [u8; INDEX_TRIE_PATH_BYTES];

pub(crate) fn trie_path(term_path: &Hash, container: u64) -> Result<TriePath> {
    let mut out = [0; INDEX_TRIE_PATH_BYTES];
    out[..term_path.len()].copy_from_slice(term_path);
    out[term_path.len()..].copy_from_slice(&path::path_bytes(container)?);
    Ok(out)
}

/// A container row's key: its term's key, then its `hi48`.
pub(crate) fn row_key(term_key: &[u8], container: u64) -> Result<Vec<u8>> {
    Ok([term_key, &path::path_bytes(container)?].concat())
}

/// The container's `hi48` after a term's key in a row key.
pub(crate) fn row_container(rest: &[u8]) -> Result<u64> {
    let tail: &[u8; HI48_BYTES] =
        (rest.try_into()).map_err(|_| IndexError::Corruption("malformed container key"))?;
    Ok(path::from_bytes(tail))
}

/// The index trie over every term's containers. Leaves are virtual: each
/// hash is recomputed from the container's row.
pub(crate) struct IndexTrie<'h, H: HashProvider> {
    trie: Trie<'h, H, INDEX_TRIE_PATH_BYTES>,
    hasher: &'h H,
}

impl<'h, H: HashProvider> IndexTrie<'h, H> {
    pub(crate) fn new(hasher: &'h H) -> Self {
        Self {
            trie: Trie::new(tables::INDEX_TRIE, INDEX_BRANCH_DOMAIN, hasher),
            hasher,
        }
    }

    /// `H(0x04 || path || roaring)`, with `path` the complete trie path.
    pub(crate) fn leaf_hash(&self, path: &TriePath, roaring: &[u8]) -> Hash {
        let (term_path, path) = path.split_at(size_of::<Hash>());
        container::leaf_hash(self.hasher, term_path, path, roaring)
    }

    /// Set each container leaf in one batched update, checking on the way
    /// that each leaf hashed the container bytes read before the batch. A
    /// mismatch is [`IndexError::RootMismatch`].
    pub(crate) fn set(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
        edits: Vec<CheckedEdit<INDEX_TRIE_PATH_BYTES>>,
    ) -> Result<RootRef<INDEX_TRIE_PATH_BYTES>> {
        match self.trie.apply_checked(tx, commit, root, edits) {
            Err(MerkleError::LeafMismatch) => Err(IndexError::RootMismatch),
            result => Ok(result?),
        }
    }

    /// Validate a stored root against this snapshot. An empty root requires an
    /// empty container table, a branch is validated at its root row, and a
    /// singleton needs exactly one container row that reconstructs its leaf.
    pub(crate) fn validate_root(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
    ) -> Result<()> {
        match root {
            RootRef::Branch(_) => self.trie.validate_root(tx, root)?,
            RootRef::Empty => {
                if tx.cursor(tables::INDEX, b"")?.next()?.is_some() {
                    return Err(IndexError::RootMismatch);
                }
            }
            RootRef::Leaf(leaf) => {
                let mut cursor = tx.cursor(tables::INDEX, b"")?;
                let (key, value) = cursor.next()?.ok_or(IndexError::RootMismatch)?;
                if cursor.next()?.is_some() {
                    return Err(IndexError::RootMismatch);
                }
                let (term, rest) = IndexTerm::split_key(&key)?;
                let container = row_container(rest)?;
                let path = trie_path(&term.routing_path(self.hasher), container)?;
                BitmapContainer::decode_values(container, &value)?;
                if LeafRef::new(path, self.leaf_hash(&path, &value)) != leaf {
                    return Err(IndexError::RootMismatch);
                }
            }
        }
        Ok(())
    }
}
