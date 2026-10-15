use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    BranchNodeCompact, Hash, HashProvider, Keccak256Hasher, LeafRef, MerkleError, RootRef, Trie,
};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};
use proptest::prelude::*;

const TABLE: Table = Table("TestBranches");
const TEST_LEAF_DOMAIN: u8 = 0x04;
const TEST_BRANCH_DOMAIN: u8 = 0x05;

const HASH: Keccak256Hasher = Keccak256Hasher;

fn leaf<const N: usize>(path: [u8; N], value: u32) -> LeafRef<N> {
    LeafRef {
        path,
        hash: HASH.hash_parts(&[&[TEST_LEAF_DOMAIN], &path, &value.to_be_bytes()]),
    }
}

// Independent sorted-leaf oracle: groups paths by their first differing nibble,
// directly frames preimages, and never uses production node codecs or mutation.
fn bulk<const N: usize>(leaves: &[LeafRef<N>], depth: usize) -> Hash {
    let digit = |path: &[u8; N], i: usize| {
        if i.is_multiple_of(2) {
            path[i / 2] >> 4
        } else {
            path[i / 2] & 15
        }
    };
    if leaves.is_empty() {
        return HASH.hash(b"");
    }
    if leaves.len() == 1 {
        return leaves[0].hash;
    }
    let mut split = depth;
    while digit(&leaves[0].path, split) == digit(&leaves.last().unwrap().path, split) {
        split += 1;
    }
    let mut preimage = vec![TEST_BRANCH_DOMAIN, (split - depth) as u8];
    for i in (depth..split).step_by(2) {
        preimage.push(
            digit(&leaves[0].path, i) * 16
                + if i + 1 < split {
                    digit(&leaves[0].path, i + 1)
                } else {
                    0
                },
        );
    }
    let mut groups: BTreeMap<u8, Vec<LeafRef<N>>> = BTreeMap::new();
    for leaf in leaves {
        groups
            .entry(digit(&leaf.path, split))
            .or_default()
            .push(*leaf);
    }
    let state = groups.keys().fold(0u16, |mask, slot| mask | 1 << slot);
    let tree = groups
        .iter()
        .filter(|(_, group)| group.len() > 1)
        .fold(0u16, |mask, (slot, _)| mask | 1 << slot);
    preimage.extend(state.to_be_bytes());
    preimage.extend(tree.to_be_bytes());
    for group in groups.values() {
        preimage.extend(bulk(group, split + 1));
    }
    HASH.hash(&preimage)
}

fn key<const N: usize>(id: u8) -> [u8; N] {
    let mut path = [0xaa; N];
    // Deep prefixes, odd splits, different root slots and repeated keys.
    if id & 8 != 0 {
        path[0] = id >> 4;
    }
    path[N - 1] = id & 0x37;
    path
}

fn exercise<const N: usize>(ops: &[(u8, u32, u8)]) {
    let db = MemoryStore::new();
    let trie = Trie::<_, N>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let mut expected = BTreeMap::new();
    let mut root = RootRef::Empty;
    let mut history = Vec::new();
    for &(id, value, action) in ops {
        let path = key(id);
        if action == 0 {
            root = trie.remove(&mut tx, root, &path).unwrap();
            expected.remove(&path);
        } else {
            let leaf = leaf(path, value);
            root = trie.insert(&mut tx, root, leaf).unwrap();
            expected.insert(path, leaf);
        }
        let sorted: Vec<_> = expected.values().copied().collect();
        assert_eq!(root.hash(&HASH), bulk(&sorted, 0));
        assert_eq!(
            trie.walk(&tx, root).collect::<Result<Vec<_>, _>>().unwrap(),
            sorted
        );
        for id in 0..64 {
            assert_eq!(
                trie.get(&tx, root, &key(id)).unwrap(),
                expected.get(&key(id)).copied()
            );
        }
        history.push((root, sorted));
    }
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    for (root, leaves) in history {
        assert_eq!(
            trie.walk(&read, root)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            leaves
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]
    #[test]
    fn random_edits_match_bulk_and_preserve_history(ops in prop::collection::vec((0u8..64, any::<u32>(), 0u8..3), 0..90)) {
        exercise::<6>(&ops);
        exercise::<32>(&ops);
    }
}

/// Branch rows reachable from `root`.
fn branches<const N: usize>(tx: &impl ReadTransaction, root: RootRef<N>) -> BTreeSet<Vec<u8>> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(root) = stack.pop() {
        if let RootRef::Branch(hash) = root {
            let node =
                BranchNodeCompact::<N>::decode(&tx.get(TABLE, &hash).unwrap().unwrap()).unwrap();
            stack.extend((0..16).filter_map(|slot| node.child(slot)));
            seen.insert(hash.to_vec());
        }
    }
    seen
}

/// Every row of the test branch table.
fn rows(tx: &impl ReadTransaction) -> BTreeSet<Vec<u8>> {
    scan_prefix(tx, TABLE, vec![])
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// `batch` over the paths `key` gives each id.
fn batch_ids<const N: usize>(base: &[(u8, u32)], edits: &[(u8, u32, bool)]) {
    let base: Vec<_> = base.iter().map(|&(id, value)| (key(id), value)).collect();
    let edits: Vec<_> = (edits.iter())
        .map(|&(id, value, present)| (key(id), value, present))
        .collect();
    batch::<N>(&base, &edits);
}

/// Commit `base` one leaf at a time, then apply `edits` as one batch. The batch
/// must match the oracle and the same edits made one at a time, keep the old
/// root readable, and write only branches the new root reaches.
fn batch<const N: usize>(base: &[([u8; N], u32)], edits: &[([u8; N], u32, bool)]) {
    let db = MemoryStore::new();
    let trie = Trie::<_, N>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let mut expected = BTreeMap::new();
    let mut old = RootRef::Empty;
    for &(path, value) in base {
        old = trie.insert(&mut tx, old, leaf(path, value)).unwrap();
        expected.insert(path, leaf(path, value));
    }
    tx.commit().unwrap();
    let old_leaves: Vec<_> = expected.values().copied().collect();
    let before = rows(&db.begin_read().unwrap());
    let mut changes = Vec::new();
    for &(path, value, present) in edits {
        let new = leaf(path, value);
        changes.push((new.path, present.then_some(new.hash)));
        if present {
            expected.insert(new.path, new);
        } else {
            expected.remove(&new.path);
        }
    }
    let mut tx = db.begin_write().unwrap();
    let root = trie.apply(&mut tx, old, changes).unwrap();
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    let sorted: Vec<_> = expected.values().copied().collect();
    assert_eq!(root.hash(&HASH), bulk(&sorted, 0));
    assert_eq!(
        trie.walk(&read, root)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        sorted
    );
    // History is kept, and the batch wrote no branch the new root misses.
    assert_eq!(
        trie.walk(&read, old)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        old_leaves
    );
    let written: BTreeSet<_> = rows(&read).difference(&before).cloned().collect();
    assert!(written.is_subset(&branches(&read, root)));
    let mut tx = db.begin_write().unwrap();
    let mut one_at_a_time = old;
    for &(path, value, present) in edits {
        one_at_a_time = if present {
            trie.insert(&mut tx, one_at_a_time, leaf(path, value))
        } else {
            trie.remove(&mut tx, one_at_a_time, &path)
        }
        .unwrap();
    }
    tx.abort();
    assert_eq!(root, one_at_a_time);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]
    #[test]
    fn batched_edits_match_bulk_and_write_only_reachable_branches(
        base in prop::collection::vec((0u8..64, any::<u32>()), 0..40),
        edits in prop::collection::vec((0u8..64, any::<u32>(), any::<bool>()), 0..60),
    ) {
        batch_ids::<6>(&base, &edits);
        batch_ids::<32>(&base, &edits);
    }
}

/// Path bytes whose nibbles are mostly 0 and 1, so random paths share long
/// prefixes and part at every depth, including inside a branch's prefix.
const DENSE_BYTES: [u8; 5] = [0x00, 0x01, 0x10, 0x11, 0xa0];
/// Trailing path bytes drawn from `DENSE_BYTES`; wider paths are padded like `key`.
const DENSE_TAIL_BYTES: usize = 3;

/// A random path tail of `DENSE_TAIL_BYTES` bytes from `DENSE_BYTES`.
fn dense_tail() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(&DENSE_BYTES[..]), DENSE_TAIL_BYTES)
}

/// `batch` over paths ending in each tail, or in its last `N` bytes if shorter.
fn dense_batch<const N: usize>(base: &[(Vec<u8>, u32)], edits: &[(Vec<u8>, u32, bool)]) {
    let path = |tail: &[u8]| {
        let mut path = [0xaa; N];
        let width = tail.len().min(N);
        path[N - width..].copy_from_slice(&tail[tail.len() - width..]);
        path
    };
    let base: Vec<_> = base
        .iter()
        .map(|(tail, value)| (path(tail), *value))
        .collect();
    let edits: Vec<_> = (edits.iter())
        .map(|(tail, value, present)| (path(tail), *value, *present))
        .collect();
    batch::<N>(&base, &edits);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]
    // Few values, so some insertions repeat the stored leaf.
    #[test]
    fn batched_edits_on_dense_paths_match_edits_made_one_at_a_time(
        base in prop::collection::vec((dense_tail(), 0u32..4), 0..24),
        edits in prop::collection::vec((dense_tail(), 0u32..4, any::<bool>()), 0..24),
    ) {
        dense_batch::<1>(&base, &edits);
        dense_batch::<2>(&base, &edits);
        dense_batch::<6>(&base, &edits);
        dense_batch::<32>(&base, &edits);
    }
}

#[test]
fn batch_removing_every_leaf_empties_the_trie() {
    let all: Vec<_> = (0..64).map(|id| (id, 0, false)).collect();
    batch_ids::<6>(&(0..64).map(|id| (id, 7)).collect::<Vec<_>>(), &all);
    batch_ids::<32>(&[(3, 1), (40, 2)], &all);
}

/// Under `key`, ids 0, 1, 2 and 16 differ only in the last two nibbles, so
/// {0, 1, 2} form a branch beside leaf 16. Ids 8 and 9 sit in another root slot.
fn lone_children<const N: usize>() {
    // At the root, a stored branch takes over its emptied parent's prefix.
    batch_ids::<N>(&[(0, 1), (1, 2), (16, 3)], &[(16, 0, false)]);
    // So does a branch the same batch rewrites.
    batch_ids::<N>(
        &[(0, 1), (1, 2), (2, 3), (16, 4)],
        &[(2, 0, false), (16, 0, false)],
    );
    // Below the root, beside the untouched branch {8, 9}.
    let below = [(0, 1), (1, 2), (16, 3), (8, 4), (9, 5)];
    batch_ids::<N>(&below, &[(16, 0, false)]);
    batch_ids::<N>(&below, &[(16, 0, false), (1, 7, true)]);
}

#[test]
fn lone_child_absorbs_the_prefix_of_its_emptied_parent() {
    lone_children::<6>();
    lone_children::<32>();
}

#[test]
fn empty_singleton_branch_and_replacement() {
    exercise::<6>(&[
        (1, 10, 0),
        (1, 10, 1),
        (1, 10, 1),
        (2, 20, 1),
        (1, 30, 1),
        (3, 0, 0),
        (2, 0, 0),
        (1, 0, 0),
    ]);
    exercise::<32>(&[
        (1, 10, 0),
        (1, 10, 1),
        (1, 10, 1),
        (2, 20, 1),
        (1, 30, 1),
        (3, 0, 0),
        (2, 0, 0),
        (1, 0, 0),
    ]);
}

fn deepest<const N: usize>() {
    let db = MemoryStore::new();
    let trie = Trie::<_, N>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let mut root = RootRef::Empty;
    let mut leaves = vec![leaf([0; N], 1)];
    for nibble in 0..N * 2 {
        let mut path = [0; N];
        path[nibble / 2] = if nibble.is_multiple_of(2) { 0x10 } else { 0x01 };
        leaves.push(leaf(path, 2));
    }
    for leaf in &leaves {
        root = trie.insert(&mut tx, root, *leaf).unwrap();
    }
    let original = root;
    for leaf in &leaves {
        root = trie.remove(&mut tx, root, &leaf.path).unwrap();
        let remaining = trie.walk(&tx, root).collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(root.hash(&HASH), bulk(&remaining, 0));
    }
    assert_eq!(root, RootRef::Empty);
    leaves.sort_by_key(|leaf| leaf.path);
    assert_eq!(
        trie.walk(&tx, original)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        leaves
    );
}

#[test]
fn every_depth_splits_and_collapses() {
    deepest::<6>();
    deepest::<32>();
}

#[test]
fn insertion_order_does_not_change_root() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let leaves = [1, 2, 8, 32].map(|id| leaf(key(id), id as u32));
    let mut sorted = leaves.to_vec();
    sorted.sort_by_key(|leaf| leaf.path);
    let expected = bulk(&sorted, 0);
    let mut tx = db.begin_write().unwrap();
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    if [a, b, c, d]
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        != 4
                    {
                        continue;
                    }
                    let mut root = RootRef::Empty;
                    for i in [a, b, c, d] {
                        root = trie.insert(&mut tx, root, leaves[i]).unwrap();
                    }
                    assert_eq!(root.hash(&HASH), expected);
                }
            }
        }
    }
}

#[test]
fn snapshots_commit_abort_and_missing_branches() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let old_read = db.begin_read().unwrap();
    let a = leaf(key(1), 1);
    let b = leaf(key(2), 2);
    let mut tx = db.begin_write().unwrap();
    let root = trie.insert(&mut tx, RootRef::Leaf(a), b).unwrap();
    assert!(matches!(
        trie.get(&old_read, root, &a.path),
        Err(MerkleError::MissingBranch(_))
    ));
    tx.commit().unwrap();
    assert!(matches!(
        trie.get(&old_read, root, &a.path),
        Err(MerkleError::MissingBranch(_))
    ));
    let read = db.begin_read().unwrap();
    assert_eq!(trie.get(&read, root, &a.path).unwrap(), Some(a));
    let mut tx = db.begin_write().unwrap();
    let aborted = trie.insert(&mut tx, root, leaf(key(3), 3)).unwrap();
    tx.abort();
    let latest = db.begin_read().unwrap();
    assert!(matches!(
        trie.get(&latest, aborted, &a.path),
        Err(MerkleError::MissingBranch(_))
    ));
    assert_eq!(trie.walk(&latest, root).count(), 2);
    let mut walk = trie.walk(&latest, aborted);
    assert!(matches!(
        walk.next(),
        Some(Err(MerkleError::MissingBranch(_)))
    ));
    assert!(walk.next().is_none());
    assert!(walk.next().is_none());
}

struct Counted<T> {
    tx: T,
    reads: Cell<usize>,
    writes: usize,
    fail_reads: bool,
    fail_write: Option<usize>,
}
impl<T: ReadTransaction> ReadTransaction for Counted<T> {
    type Cursor<'a>
        = T::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        self.reads.set(self.reads.get() + 1);
        if self.fail_reads {
            return Err(injected_error());
        }
        self.tx.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.tx.cursor(table, key)
    }
}
impl<T: WriteTransaction> WriteTransaction for Counted<T> {
    fn insert(&mut self, t: Table, k: &[u8], v: &[u8]) -> golemdb_storage::Result<()> {
        self.writes += 1;
        if self.fail_write == Some(self.writes) {
            return Err(injected_error());
        }
        self.tx.insert(t, k, v)
    }
    fn put(&mut self, t: Table, k: &[u8], v: &[u8]) -> golemdb_storage::Result<()> {
        panic!("unexpected upsert: {t:?} {k:?} {v:?}")
    }
    fn delete(&mut self, _: Table, _: &[u8]) -> golemdb_storage::Result<bool> {
        panic!("unexpected delete")
    }
    fn commit(self) -> golemdb_storage::Result<()> {
        self.tx.commit()
    }
    fn abort(self) {
        self.tx.abort()
    }
}

#[test]
fn updates_touch_only_affected_branches_and_walk_is_lazy() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = Counted {
        tx: db.begin_write().unwrap(),
        reads: Cell::new(0),
        writes: 0,
        fail_reads: false,
        fail_write: None,
    };
    let mut root = RootRef::Empty;
    for i in 0u16..256 {
        let mut path = [0; 6];
        path[0] = i as u8;
        root = trie.insert(&mut tx, root, leaf(path, 0)).unwrap();
    }
    tx.writes = 0;
    tx.reads.set(0);
    let unchanged = trie.insert(&mut tx, root, leaf([0; 6], 0)).unwrap();
    assert_eq!(unchanged, root);
    assert_eq!(tx.writes, 0);
    let mut absent = [0; 6];
    absent[5] = 1;
    assert_eq!(trie.remove(&mut tx, root, &absent).unwrap(), root);
    assert_eq!(tx.writes, 0);
    tx.reads.set(0);
    root = trie.insert(&mut tx, root, leaf([0; 6], 1)).unwrap();
    assert_eq!(tx.writes, 2);
    assert_eq!(tx.reads.get(), 2);
    tx.reads.set(0);
    let mut walk = trie.walk(&tx, root);
    assert_eq!(tx.reads.get(), 0);
    assert_eq!(walk.next().unwrap().unwrap(), leaf([0; 6], 1));
    assert_eq!(tx.reads.get(), 2);
}

/// Leaves below the long prefix in `edits_off_a_prefix_cost_the_same_at_any_subtrie_size`.
const PREFIXED_LEAVES: u16 = 1024;

#[test]
fn edits_off_a_prefix_cost_the_same_at_any_subtrie_size() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 32>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    // The leaves differ only in their last two bytes, so the root branch's
    // prefix covers everything before them.
    let leaves = (0..PREFIXED_LEAVES).map(|i| {
        let mut path = [0xaa; 32];
        let tail = i.to_be_bytes();
        path[32 - tail.len()..].copy_from_slice(&tail);
        (path, Some(leaf(path, 0).hash))
    });
    let mut tx = db.begin_write().unwrap();
    let root = trie.apply(&mut tx, RootRef::Empty, leaves).unwrap();
    tx.commit().unwrap();
    let mut tx = Counted {
        tx: db.begin_write().unwrap(),
        reads: Cell::new(0),
        writes: 0,
        fail_reads: false,
        fail_write: None,
    };
    let off = leaf([0; 32], 1);
    // A removal off the prefix has nothing to remove.
    let unchanged = trie.apply(&mut tx, root, [(off.path, None)]).unwrap();
    assert_eq!((unchanged, tx.reads.get(), tx.writes), (root, 1, 0));
    // An insertion there reads only the root, then writes the root moved one
    // level down with a shorter prefix, and its new parent.
    tx.reads.set(0);
    let split = trie
        .apply(&mut tx, root, [(off.path, Some(off.hash))])
        .unwrap();
    assert_eq!((tx.reads.get(), tx.writes), (1, 2));
    assert_eq!(split, trie.insert(&mut tx, root, off).unwrap());
}

fn injected_error() -> golemdb_storage::StorageError {
    golemdb_storage::StorageError::Backend(std::io::Error::other("injected failure").into())
}

#[test]
fn storage_errors_propagate_and_abort_discards_partial_branches() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let a = leaf([0; 6], 1);
    let b = leaf([0, 0, 0, 0, 0, 1], 2);
    let root = trie.insert(&mut tx, RootRef::Leaf(a), b).unwrap();
    tx.commit().unwrap();
    let mut tx = Counted {
        tx: db.begin_write().unwrap(),
        reads: Cell::new(0),
        writes: 0,
        fail_reads: true,
        fail_write: Some(2),
    };
    assert!(matches!(
        trie.get(&tx, root, &a.path),
        Err(MerkleError::Storage(_))
    ));
    let mut walk = trie.walk(&tx, root);
    assert!(matches!(walk.next(), Some(Err(MerkleError::Storage(_)))));
    assert!(walk.next().is_none());
    tx.fail_reads = false;
    // Splits the existing long prefix: the trimmed child is written first,
    // then writing the new parent fails. The enclosing transaction must abort.
    assert!(matches!(
        trie.insert(&mut tx, root, leaf([0x10; 6], 3)),
        Err(MerkleError::Storage(_))
    ));
    assert_eq!(tx.writes, 2);
    let rows = golemdb_storage::scan_prefix(&tx, TABLE, vec![])
        .unwrap()
        .collect::<golemdb_storage::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    tx.abort();
    let read = db.begin_read().unwrap();
    assert_eq!(
        golemdb_storage::scan_prefix(&read, TABLE, vec![])
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        trie.walk(&read, root)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![a, b]
    );
}
