use golemdb_merkle::{
    CheckedEdit, Hash, HashProvider, LeafRef, MerkleError, RootRef, Trie, TrieUpdate,
};
use golemdb_storage::{ReadCursor, ReadTransaction, WriteTransaction};

use crate::{
    BitmapContainer, CONTAINER_LEAF_DOMAIN, ContainerId, INDEX_BRANCH_DOMAIN,
    INDEX_TRIE_PATH_BYTES, IndexError, IndexKey, Result, tables,
};

/// The index trie over every term's containers. Leaves are virtual: each
/// hash is recomputed from the container's row.
pub(crate) struct IndexTrie<'h, H: HashProvider> {
    trie: Trie<'h, H, INDEX_TRIE_PATH_BYTES>,
    hasher: &'h H,
}

/// A container's index trie path: its term's routing path, then its [`ContainerId`].
/// Only [`IndexTriePath::new`] builds one, so every trie access is routed from a
/// term and a container.
#[derive(Clone, Copy)]
pub(crate) struct IndexTriePath([u8; INDEX_TRIE_PATH_BYTES]);

impl IndexTriePath {
    pub(crate) fn new(term_path: &Hash, container_id: ContainerId) -> Self {
        let mut out = [0; INDEX_TRIE_PATH_BYTES];
        out[..term_path.len()].copy_from_slice(term_path);
        out[term_path.len()..].copy_from_slice(&container_id.to_bytes());
        Self(out)
    }
}

impl AsRef<[u8]> for IndexTriePath {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A container's leaf hash before a batch and after it (`None` for absent),
/// for [`IndexTrie::set`].
pub(crate) struct ContainerEdit {
    pub(crate) path: IndexTriePath,
    pub(crate) before: Option<Hash>,
    pub(crate) after: Option<Hash>,
}

impl<'h, H: HashProvider> IndexTrie<'h, H> {
    pub(crate) fn new(hasher: &'h H) -> Self {
        Self {
            trie: Trie::new(tables::INDEX_TRIE, INDEX_BRANCH_DOMAIN, hasher),
            hasher,
        }
    }

    /// `H(0x04 || path || roaring)`: the leaf of the container at trie `path`
    /// whose row holds the Roaring bytes `roaring`.
    pub(crate) fn leaf_hash(&self, path: &IndexTriePath, roaring: &[u8]) -> Hash {
        self.hasher
            .hash_parts(&[&[CONTAINER_LEAF_DOMAIN], path.as_ref(), roaring])
    }

    /// Replace each container's leaf hash `before` with `after` (`None` for
    /// absent) in one batched update, checking on the way that each leaf
    /// hashed the container bytes read before the batch. A mismatch is
    /// [`IndexError::RootMismatch`].
    pub(crate) fn set(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
        edits: impl IntoIterator<Item = ContainerEdit>,
    ) -> Result<TrieUpdate<INDEX_TRIE_PATH_BYTES>> {
        let edits = (edits.into_iter()).map(|edit| CheckedEdit {
            path: edit.path.0,
            before: edit.before,
            after: edit.after,
        });
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
                let IndexKey { term, container_id } = IndexKey::decode(&key)?;
                let path = IndexTriePath::new(&term.routing_path(self.hasher), container_id);
                BitmapContainer::decode(container_id, &value)?;
                if LeafRef::new(path.0, self.leaf_hash(&path, &value)) != leaf {
                    return Err(IndexError::RootMismatch);
                }
            }
        }
        Ok(())
    }
}
