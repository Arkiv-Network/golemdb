use crate::{CELL_BRANCH_DOMAIN, CELL_LEAF_DOMAIN};
use golemdb_merkle::{Hash, HashProvider, LeafRef, RootRef, Trie};
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
        LeafRef {
            path,
            hash: self
                .hasher
                .hash_parts(&[&[CELL_LEAF_DOMAIN], &path, value.encoded_bytes()]),
        }
    }

    /// Verify that each leaf at `H(key)` commits to the value read from the
    /// `Cell` table; `None` requires absence. A different or missing commitment
    /// returns `RootMismatch`; storage and branch-validation errors propagate.
    /// This checks these addresses, not the entire state, and also applies to
    /// no-ops. One ascending walk loads and verifies each branch once, rather
    /// than once per key: `checks` holds `(H(key), value read)`, ascending by
    /// path.
    pub(crate) fn check_sorted(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        checks: &[(Hash, Option<&CellValue>)],
    ) -> Result<()> {
        let paths: Vec<_> = checks.iter().map(|(path, _)| *path).collect();
        let found = self.trie.get_sorted(tx, root, &paths)?;
        for ((path, before), found) in checks.iter().zip(found) {
            if found != before.map(|value| self.leaf(*path, value)) {
                return Err(CellError::RootMismatch);
            }
        }
        Ok(())
    }

    /// The trie path of an encoded cell key.
    pub(crate) fn path(&self, key: &[u8]) -> Hash {
        self.hasher.hash(key)
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
            let path = self.hasher.hash(key);
            (path, value.map(|value| self.leaf(path, value).hash))
        });
        Ok(self.trie.apply(tx, root, edits)?)
    }

    /// Recover root metadata from a digest and the transaction's snapshot.
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
    ) -> Result<RootRef<CELL_TRIE_PATH_BYTES>> {
        if hash == self.hasher.hash(&[]) {
            if tx.cursor(tables::CELL, b"")?.next()?.is_some() {
                return Err(CellError::RootMismatch);
            }
            return Ok(RootRef::Empty);
        }
        if tx.get(tables::CELL_TRIE, &hash)?.is_some() {
            let root = RootRef::Branch(hash);
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
        let leaf = self.leaf(self.hasher.hash(&key), &value);
        if leaf.hash != hash {
            return Err(CellError::RootMismatch);
        }
        Ok(RootRef::Leaf(leaf))
    }
}
