use crate::{CELL_BRANCH_DOMAIN, CELL_LEAF_DOMAIN};
use golemdb_merkle::{BranchRef, Hash, HashProvider, LeafRef, RootRef, Trie};
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

    pub(crate) fn at(self, commit: u64) -> Self {
        Self {
            trie: self.trie.at(commit),
            ..self
        }
    }

    /// Construct a virtual leaf binding the complete routing path to the tagged
    /// cell value: `H(CELL_LEAF_DOMAIN || path || typeTag || value)`.
    /// The caller supplies `path = H(encoded CellKey)`; no leaf row is written.
    /// Experiment: routing path `recordID ‖ H(key)`; the key starts with the record ID.
    fn path(&self, key: &[u8]) -> [u8; CELL_TRIE_PATH_BYTES] {
        let mut path = [0; CELL_TRIE_PATH_BYTES];
        path[..8].copy_from_slice(&key[..8]);
        path[8..].copy_from_slice(&self.hasher.hash(key));
        path
    }

    fn leaf(
        &self,
        path: [u8; CELL_TRIE_PATH_BYTES],
        value: &CellValue,
    ) -> LeafRef<CELL_TRIE_PATH_BYTES> {
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
        let path = self.path(key);
        if self.trie.get(tx, root, &path)? != before.map(|value| self.leaf(path, value)) {
            return Err(CellError::RootMismatch);
        }
        Ok(())
    }

    /// For each key, insert or replace the leaf at `H(key)`, or remove it when
    /// the value is `None`, in one batched trie update. Returns the updated root
    /// and retains old branches for copy-on-write history. The caller updates
    /// the `Cell` table in the same transaction and must abort that transaction
    /// if a mutation fails.
    pub(crate) fn set<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        changes: impl IntoIterator<Item = (&'a [u8], Option<&'a CellValue>)>,
    ) -> Result<RootRef<CELL_TRIE_PATH_BYTES>> {
        let edits = changes.into_iter().map(|(key, value)| {
            let path = self.path(key);
            (path, value.map(|value| self.leaf(path, value).hash))
        });
        Ok(self.trie.apply(tx, root, edits)?)
    }

    /// Recover root metadata from a digest, the commit that stored it if it is
    /// a branch, and the transaction's snapshot.
    /// The empty-root digest requires an empty `Cell` table. An existing branch
    /// is validated at its root row only, without auditing its subtree or the
    /// `Cell` table. Otherwise the `Cell` table must contain exactly one valid
    /// row whose key and value reconstruct the singleton hash.
    /// Historical singleton recovery needs historical cell data or a retained
    /// `RootRef`; an absent branch row alone never establishes a singleton.
    pub(crate) fn reopen(
        &self,
        tx: &impl ReadTransaction,
        hash: Hash,
        branch_commit: Option<u64>,
    ) -> Result<RootRef<CELL_TRIE_PATH_BYTES>> {
        if hash == self.hasher.hash(&[]) {
            if tx.cursor(tables::CELL, b"")?.next()?.is_some() {
                return Err(CellError::RootMismatch);
            }
            return Ok(RootRef::Empty);
        }
        if let Some(commit) = branch_commit {
            let root = RootRef::Branch(BranchRef { hash, commit });
            self.trie.validate_root(tx, root)?;
            return Ok(root);
        }
        // A missing branch is a singleton only when exactly one current row
        // independently reconstructs the requested digest.
        let mut cursor = tx.cursor(tables::CELL, b"")?;
        let (key, value) = cursor.next()?.ok_or(CellError::RootMismatch)?;
        if cursor.next()?.is_some() {
            return Err(CellError::RootMismatch);
        }
        CellKey::decode(&key)?;
        let value = CellValue::parse(value)?;
        let leaf = self.leaf(self.path(&key), &value);
        if leaf.hash != hash {
            return Err(CellError::RootMismatch);
        }
        Ok(RootRef::Leaf(leaf))
    }
}
