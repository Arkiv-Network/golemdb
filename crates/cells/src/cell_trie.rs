use crate::{CELL_BRANCH_DOMAIN, CELL_LEAF_DOMAIN};
use golemdb_merkle::{Hash, HashProvider, LeafRef, RootRef, Trie, TrieUpdate};
use golemdb_storage::{ReadCursor, ReadTransaction, WriteTransaction};

use crate::{CELL_TRIE_PATH_BYTES, CellError, CellKey, CellValue, Result, tables};

/// Cell-specific routing, leaf hashing and recovery over branch-only storage.
pub(crate) struct CellTrie<'h, H: HashProvider> {
    trie: Trie<'h, H, CELL_TRIE_PATH_BYTES>,
    hasher: &'h H,
}

impl<'h, H: HashProvider> CellTrie<'h, H> {
    /// Configure branch storage and the cell branch domain using the caller's
    /// hash provider. This does not open a transaction or load a root.
    pub(crate) fn new(hasher: &'h H) -> Self {
        Self {
            trie: Trie::new(tables::CELL_TRIE, CELL_BRANCH_DOMAIN, hasher),
            hasher,
        }
    }

    /// Construct a virtual leaf binding the complete routing path to the tagged
    /// cell value: `H(CELL_LEAF_DOMAIN || path || typeTag || value)`.
    /// The caller supplies `path = H(encoded CellKey)`; no leaf row is written.
    fn leaf(&self, path: Hash, value: &CellValue) -> LeafRef<CELL_TRIE_PATH_BYTES> {
        LeafRef::new(
            path,
            self.hasher
                .hash_parts(&[&[CELL_LEAF_DOMAIN], &path, value.encoded_bytes()]),
        )
    }

    /// Verify that the leaf at `H(key)` commits to the value read from the `Cell` table.
    /// `None` requires absence from the trie. A different or missing commitment
    /// returns `RootMismatch`; storage and branch-validation errors propagate.
    /// This checks one address, not the entire state, and also applies to no-ops.
    pub(crate) fn check(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        key: &[u8],
        before: Option<&CellValue>,
    ) -> Result<()> {
        let path = self.hasher.hash(key);
        if self.trie.get(tx, root, &path)? != before.map(|value| self.leaf(path, value)) {
            return Err(CellError::RootMismatch);
        }
        Ok(())
    }

    /// For each key, insert or replace the leaf at `H(key)`, or remove it when
    /// the value is `None`, in one batched trie update. Returns the updated root
    /// and the rows it superseded; older commits' rows stay for their roots, and
    /// rows this commit wrote and then replaced are deleted. The caller updates
    /// the `Cell` table in the same transaction and must abort that transaction
    /// if a mutation fails.
    pub(crate) fn set<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        changes: impl IntoIterator<Item = (&'a [u8], Option<&'a CellValue>)>,
    ) -> Result<TrieUpdate<CELL_TRIE_PATH_BYTES>> {
        let edits = changes.into_iter().map(|(key, value)| {
            let path = self.hasher.hash(key);
            (path, value.map(|value| self.leaf(path, value).hash))
        });
        Ok(self.trie.apply(tx, commit, root, edits)?)
    }

    /// Validate a stored root against the transaction's snapshot.
    /// An empty root requires an empty `Cell` table. A branch is validated at
    /// its root row only, without auditing its subtree or the `Cell` table. A
    /// singleton needs exactly one valid `Cell` row that reconstructs its leaf.
    /// Historical singletons need historical cell data.
    pub(crate) fn validate_root(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
    ) -> Result<()> {
        match root {
            RootRef::Branch(_) => self.trie.validate_root(tx, root)?,
            RootRef::Empty => {
                if tx.cursor(tables::CELL, b"")?.next()?.is_some() {
                    return Err(CellError::RootMismatch);
                }
            }
            RootRef::Leaf(leaf) => {
                let mut cursor = tx.cursor(tables::CELL, b"")?;
                let (key, value) = cursor.next()?.ok_or(CellError::RootMismatch)?;
                if cursor.next()?.is_some() {
                    return Err(CellError::RootMismatch);
                }
                CellKey::decode(&key)?;
                let value = CellValue::parse(value)?;
                if self.leaf(self.hasher.hash(&key), &value) != leaf {
                    return Err(CellError::RootMismatch);
                }
            }
        }
        Ok(())
    }
}
