use std::collections::BTreeMap;

use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{Hash, HashProvider, LeafRef, Result, RootRef, Trie, TrieTable, Walk};

/// A new leaf for a [`PayloadTrie`]: its hash, and the bytes of its row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafPayload {
    pub hash: Hash,
    pub bytes: Vec<u8>,
}

/// A trie whose leaves carry payloads, each stored as a row of its own in
/// `leaf_table` under `commit ‖ namespace ‖ path`. Branches record each leaf's
/// commit, so old roots keep reading the payloads they were built with. The
/// trie writes and reads the rows itself; callers pass payloads, not keys.
pub struct PayloadTrie<'h, H: HashProvider, const N: usize, const NS: usize = 0> {
    trie: Trie<'h, H, N, NS>,
    leaf_table: Table,
}

impl<'h, H: HashProvider, const N: usize> PayloadTrie<'h, H, N> {
    /// Branches go to `branch_table` and payloads to `leaf_table`, which must
    /// differ: a leaf's key can equal a branch's.
    pub fn new(branch_table: TrieTable, leaf_table: TrieTable, domain: u8, hasher: &'h H) -> Self {
        Self::namespaced(branch_table, leaf_table, [], domain, hasher)
    }
}

impl<'h, H: HashProvider, const N: usize, const NS: usize> PayloadTrie<'h, H, N, NS> {
    /// As [`PayloadTrie::new`], keying branches and payloads alike under
    /// `namespace`, as [`Trie::namespaced`] does.
    pub fn namespaced(
        branch_table: TrieTable<NS>,
        leaf_table: TrieTable<NS>,
        namespace: [u8; NS],
        domain: u8,
        hasher: &'h H,
    ) -> Self {
        assert!(
            leaf_table != branch_table,
            "leaf rows need a table of their own"
        );
        Self {
            trie: Trie::namespaced(branch_table, namespace, domain, hasher).with_leaf_rows(),
            leaf_table: leaf_table.table(),
        }
    }

    /// As [`Trie::node_key`].
    pub fn node_key(&self, commit: u64, route: &[u8]) -> Vec<u8> {
        self.trie.node_key(commit, route)
    }

    /// Row key in `leaf_table` of the payload at `path` written at `commit`.
    pub fn leaf_key(&self, commit: u64, path: &[u8; N]) -> Vec<u8> {
        self.trie.leaf_key(commit, path)
    }

    /// As [`Trie::get`].
    pub fn get(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<N>,
        path: &[u8; N],
    ) -> Result<Option<LeafRef<N>>> {
        self.trie.get(tx, root, path)
    }

    /// As [`Trie::walk`].
    pub fn walk<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        root: RootRef<N>,
    ) -> Walk<'a, T, H, N, NS> {
        self.trie.walk(tx, root)
    }

    /// The payload `leaf` was stored with, or `None` if its row is missing.
    pub fn payload(&self, tx: &impl ReadTransaction, leaf: LeafRef<N>) -> Result<Option<Vec<u8>>> {
        Ok(tx.get(
            self.leaf_table,
            &self.trie.leaf_key(leaf.commit, &leaf.path),
        )?)
    }

    /// As [`Trie::apply`], storing each new leaf's payload under `commit` in
    /// `tx`. A leaf whose hash is unchanged keeps its row and commit. Call once
    /// per commit for each namespace: a leaf removed by a later call in the same
    /// commit leaves its row behind for good, unreached and never reclaimed.
    pub fn apply(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<N>,
        edits: impl IntoIterator<Item = ([u8; N], Option<LeafPayload>)>,
    ) -> Result<RootRef<N>> {
        // Keyed by path to find each created leaf's payload; the last edit of a
        // path wins, as in the trie.
        let edits: BTreeMap<_, _> = edits.into_iter().collect();
        let hashes =
            (edits.iter()).map(|(path, payload)| (*path, payload.as_ref().map(|p| p.hash)));
        let (root, created) = self.trie.apply_leaves(tx, commit, root, hashes)?;
        for path in created {
            // Always `Some`: only edits with a payload create leaves.
            if let Some(Some(payload)) = edits.get(&path) {
                tx.put(
                    self.leaf_table,
                    &self.trie.leaf_key(commit, &path),
                    &payload.bytes,
                )?;
            }
        }
        Ok(root)
    }
}
