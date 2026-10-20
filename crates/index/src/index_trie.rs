use crate::INDEX_BRANCH_DOMAIN;
use golemdb_merkle::{Hash, HashProvider, LeafRef, RootRef, Trie};
use golemdb_storage::{ReadCursor, ReadTransaction, WriteTransaction};

use crate::{BITMAP_TRIE_PATH_BYTES, INDEX_TRIE_PATH_BYTES, IndexError, IndexTerm, Result, tables};

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

    fn leaf(&self, term: &IndexTerm, bitmap: Hash) -> LeafRef<INDEX_TRIE_PATH_BYTES> {
        LeafRef::new(
            term.routing_path(self.hasher),
            term.leaf_hash(&bitmap, self.hasher),
        )
    }

    pub(crate) fn check(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
        term: &IndexTerm,
        before: Option<Hash>,
    ) -> Result<()> {
        if self.trie.get(tx, root, &term.routing_path(self.hasher))?
            != before.map(|hash| self.leaf(term, hash))
        {
            return Err(IndexError::RootMismatch);
        }
        Ok(())
    }

    /// Set each term's bitmap root, removing the term for `None`, in one
    /// batched trie update.
    pub(crate) fn set<'t>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
        changes: impl IntoIterator<Item = (&'t IndexTerm, Option<Hash>)>,
    ) -> Result<RootRef<INDEX_TRIE_PATH_BYTES>> {
        let edits = changes.into_iter().map(|(term, bitmap)| match bitmap {
            Some(hash) => {
                let leaf = self.leaf(term, hash);
                (leaf.path, Some(leaf.hash))
            }
            None => (term.routing_path(self.hasher), None),
        });
        Ok(self.trie.apply(tx, commit, root, edits)?)
    }

    /// Validate a stored root against this snapshot. An empty root requires an
    /// empty `Index` table, a branch is validated at its root row, and a
    /// singleton needs exactly one `Index` row that reconstructs its leaf.
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
                let term = IndexTerm::decode(&key)?;
                let bitmap = decode_root(&value)?.hash(self.hasher);
                if self.leaf(&term, bitmap) != leaf {
                    return Err(IndexError::RootMismatch);
                }
            }
        }
        Ok(())
    }
}

/// An Index row holds the encoded bitmap root (see [`RootRef::encode`]); only
/// its hash is committed, through the term's IndexTrie leaf.
pub(crate) fn decode_root(bytes: &[u8]) -> Result<RootRef<BITMAP_TRIE_PATH_BYTES>> {
    match RootRef::decode(bytes) {
        Ok(RootRef::Empty) => Err(IndexError::Corruption("empty bitmap root in Index row")),
        Ok(root) => Ok(root),
        Err(_) => Err(IndexError::Corruption("malformed bitmap root in Index row")),
    }
}
