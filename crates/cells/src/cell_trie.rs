use crate::{CELL_BRANCH_DOMAIN, CELL_LEAF_DOMAIN};
use golemdb_merkle::{HashProvider, LeafRef, RootRef, Trie};
use golemdb_storage::{ReadCursor, ReadTransaction, WriteTransaction};

use crate::{CELL_TRIE_PATH_BYTES, CellError, CellKey, CellValue, Result, tables};

/// A cell's routing path, `recordID || H(encoded CellKey)`. Only
/// [`CellTrie::path`] builds one, so every trie access is routed from a `CellKey`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CellPath([u8; CELL_TRIE_PATH_BYTES]);

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

    /// Route a cell by its record ID, then by the hash of its encoded key, so
    /// each record's cells share one subtree.
    pub(crate) fn path(&self, key: &CellKey) -> CellPath {
        let record_id = key.record_id().to_be_bytes();
        // The encoded key is `recordID || name`; hashing its parts avoids allocating it.
        let digest = self.hasher.hash_parts(&[&record_id, key.name().as_bytes()]);
        let mut path = [0; CELL_TRIE_PATH_BYTES];
        path[..record_id.len()].copy_from_slice(&record_id);
        path[record_id.len()..].copy_from_slice(&digest);
        CellPath(path)
    }

    /// Construct a virtual leaf binding the complete routing path to the tagged
    /// cell value: `H(CELL_LEAF_DOMAIN || path || typeTag || value)`.
    /// No leaf row is written.
    fn leaf(&self, CellPath(path): CellPath, value: &CellValue) -> LeafRef<CELL_TRIE_PATH_BYTES> {
        LeafRef::new(
            path,
            self.hasher
                .hash_parts(&[&[CELL_LEAF_DOMAIN], &path, value.encoded_bytes()]),
        )
    }

    /// Verify that each leaf at its path commits to the value read from the
    /// `Cell` table; `None` requires absence. A different or missing commitment
    /// returns `RootMismatch`; storage and branch-validation errors propagate.
    /// This checks these addresses, not the entire state, and also applies to
    /// no-ops. One ascending walk loads and verifies each branch once, rather
    /// than once per key: `checks` holds `(path, value read)`, ascending by
    /// path.
    pub(crate) fn check_sorted(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        checks: &[(CellPath, Option<&CellValue>)],
    ) -> Result<()> {
        let paths: Vec<_> = checks.iter().map(|(path, _)| path.0).collect();
        let found = self.trie.get_sorted(tx, root, &paths)?;
        for ((path, before), found) in checks.iter().zip(found) {
            if found != before.map(|value| self.leaf(*path, value)) {
                return Err(CellError::RootMismatch);
            }
        }
        Ok(())
    }

    /// For each path, insert or replace its leaf, or remove it when the value is
    /// `None`, in one batched trie update. Returns the updated root and retains
    /// old branches for copy-on-write history. The caller updates the `Cell`
    /// table in the same transaction and must abort that transaction if a
    /// mutation fails.
    pub(crate) fn set<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        changes: impl IntoIterator<Item = (CellPath, Option<&'a CellValue>)>,
    ) -> Result<RootRef<CELL_TRIE_PATH_BYTES>> {
        let edits = (changes.into_iter())
            .map(|(path, value)| (path.0, value.map(|value| self.leaf(path, value).hash)));
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
                let key = CellKey::decode(&key)?;
                let value = CellValue::parse(value)?;
                if self.leaf(self.path(&key), &value) != leaf {
                    return Err(CellError::RootMismatch);
                }
            }
        }
        Ok(())
    }
}
