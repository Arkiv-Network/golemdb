use crate::{
    BranchNodeCompact, BranchRef, Hash, HashProvider, Keccak256Hasher, LeafRef, MerkleError,
    Result, RootRef, Trie, TrieTable,
};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction};
use proptest::prelude::*;

// Test domains are fixture inputs, independent of application assignments.
const TEST_BRANCH_DOMAIN: u8 = 0x05;
const OTHER_BRANCH_DOMAIN: u8 = 0x03;

const HASH: Keccak256Hasher = Keccak256Hasher;
const TABLE: TrieTable = TrieTable::new(Table("Branches"));

/// A branch whose leaves have no rows, with zero child commits.
fn compact(
    prefix: Vec<u8>,
    prefix_len: u8,
    state_mask: u16,
    tree_mask: u16,
    child_hashes: Vec<Hash>,
    leaf_paths: Vec<[u8; 6]>,
) -> Result<BranchNodeCompact<6>> {
    let commits = vec![0; tree_mask.count_ones() as usize];
    BranchNodeCompact::new(
        prefix,
        prefix_len,
        state_mask,
        tree_mask,
        child_hashes,
        leaf_paths,
        commits,
    )
}

fn node() -> BranchNodeCompact<6> {
    compact(
        vec![0xab, 0xc0],
        3,
        0x6000,
        0,
        vec![[0x11; 32], [0x22; 32]],
        vec![[0xab, 0xcd, 0, 0, 0, 0], [0xab, 0xce, 0, 0, 0, 0]],
    )
    .unwrap()
}

#[test]
fn canonical_bytes_masks_and_domain_preimages() {
    let node = node();
    let mut payload = vec![3, 0xab, 0xc0, 0x60, 0, 0, 0];
    payload.extend([0x11; 32]);
    payload.extend([0x22; 32]);
    assert_eq!(node.hash_payload(), payload);
    assert_eq!(
        node.hash(TEST_BRANCH_DOMAIN, &HASH),
        [
            0xc8, 0x16, 0x42, 0x41, 0x1b, 0x5e, 0x12, 0xef, 0x43, 0x5a, 0xbe, 0xdd, 0x4f, 0xb0,
            0x2d, 0xa8, 0x42, 0x6e, 0xc1, 0xaf, 0x8d, 0xb3, 0x1c, 0xb7, 0x12, 0x59, 0x15, 0xd1,
            0x06, 0x12, 0xa5, 0xb9,
        ]
    );
    let mut encoded = payload.clone();
    encoded.extend([0xab, 0xcd, 0, 0, 0, 0, 0xab, 0xce, 0, 0, 0, 0]);
    assert_eq!(node.encode(), encoded);
    assert_eq!(BranchNodeCompact::<6>::decode(&encoded).unwrap(), node);
    for domain in u8::MIN..=u8::MAX {
        assert_eq!(
            node.hash(domain, &HASH),
            HASH.hash_parts(&[&[domain], &payload])
        );
    }
    assert_ne!(
        node.hash(OTHER_BRANCH_DOMAIN, &HASH),
        node.hash(TEST_BRANCH_DOMAIN, &HASH)
    );
    assert_eq!(
        node.child(13),
        Some(RootRef::Leaf(LeafRef::new(
            [0xab, 0xcd, 0, 0, 0, 0],
            [0x11; 32]
        )))
    );
    assert_eq!(
        node.child(14),
        Some(RootRef::Leaf(LeafRef::new(
            [0xab, 0xce, 0, 0, 0, 0],
            [0x22; 32]
        )))
    );
    assert_eq!(node.child(12), None);
    assert_eq!(node.child(16), None);
    assert_eq!(node.child(255), None);
    assert_eq!(RootRef::<6>::Empty.hash(&HASH), HASH.hash(b""));
    assert_eq!(
        RootRef::Leaf(LeafRef::new([0; 6], [42; 32])).hash(&HASH),
        [42; 32]
    );
    // Changing a suffix changes physical metadata only. Authentication of the
    // complete path requires the owner's leaf preimage, unavailable to Merkle.
    let mut modified = encoded;
    *modified.last_mut().unwrap() = 1;
    let other = BranchNodeCompact::<6>::decode(&modified).unwrap();
    assert_eq!(
        other.hash(TEST_BRANCH_DOMAIN, &HASH),
        node.hash(TEST_BRANCH_DOMAIN, &HASH)
    );
    assert_ne!(other.encode(), node.encode());
}

#[test]
fn malformed_node_encodings_are_rejected() {
    let good = node().encode();
    for len in 0..good.len() {
        assert!(BranchNodeCompact::<6>::decode(&good[..len]).is_err());
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(BranchNodeCompact::<6>::decode(&trailing).is_err());
    let mut padding = good.clone();
    padding[2] |= 1;
    assert!(BranchNodeCompact::<6>::decode(&padding).is_err());
    // A tree-mask bit outside the state mask.
    let mut mask = good;
    mask[6] = 1;
    assert!(BranchNodeCompact::<6>::decode(&mask).is_err());
    for state in [0, 1, 0x8000] {
        assert!(compact(vec![], 0, state, 0, vec![], vec![]).is_err());
    }
    assert!(compact(vec![0; 6], 12, 3, 0, vec![[0; 32]; 2], vec![[0; 6]; 2]).is_err());
    assert!(compact(vec![], 0, 3, 0, vec![[0; 32]], vec![[0; 6]; 2]).is_err());
    assert!(compact(vec![], 0, 3, 0, vec![[0; 32]; 2], vec![[0; 6]]).is_err());
}

proptest! {
    #[test]
    fn arbitrary_decoding_is_panic_free_and_canonical(bytes in prop::collection::vec(any::<u8>(), 0..1200)) {
        if let Ok(node) = BranchNodeCompact::<6>::decode(&bytes) { prop_assert_eq!(&node.encode(), &bytes); }
        if let Ok(node) = BranchNodeCompact::<32>::decode(&bytes) { prop_assert_eq!(node.encode(), bytes); }
    }
}

#[test]
fn corrupt_hash_and_leaf_routing_fail_on_reads_and_mutations() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let node = node();
    let hash = node.hash(TEST_BRANCH_DOMAIN, &HASH);
    let root = RootRef::Branch(BranchRef { hash, commit: 0 });
    let leaf = LeafRef::new(node.leaf_paths()[0], node.child_hashes()[0]);
    let key = trie.node_key(0, &[]);
    let mut bytes = node.encode();
    bytes[7] ^= 1;
    tx.put(TABLE.table(), &key, &bytes).unwrap();
    assert!(matches!(
        trie.get(&tx, root, &leaf.path),
        Err(MerkleError::HashMismatch)
    ));
    assert!(matches!(
        trie.insert(&mut tx, 0, root, &leaf.path, leaf.hash),
        Err(MerkleError::HashMismatch)
    ));
    assert!(matches!(
        trie.remove(&mut tx, 0, root, &leaf.path),
        Err(MerkleError::HashMismatch)
    ));
    let mut bytes = node.encode();
    bytes[71] ^= 1; // Unhashed first leaf path, inconsistent with branch prefix.
    tx.put(TABLE.table(), &key, &bytes).unwrap();
    assert!(matches!(
        trie.get(&tx, root, &leaf.path),
        Err(MerkleError::InvalidNode(_))
    ));
    assert!(matches!(
        trie.walk(&tx, root).next(),
        Some(Err(MerkleError::InvalidNode(_)))
    ));
}

#[test]
fn later_commits_never_overwrite_earlier_rows() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let node = node();
    let a = node.child(13).unwrap();
    let RootRef::Leaf(b) = node.child(14).unwrap() else {
        unreachable!()
    };
    let mut tx = db.begin_write().unwrap();
    let first = trie.insert(&mut tx, 1, a, &b.path, b.hash).unwrap();
    let row = tx.get(TABLE.table(), &trie_key(1)).unwrap();
    assert_eq!(
        first,
        RootRef::Branch(BranchRef {
            hash: node.hash(TEST_BRANCH_DOMAIN, &HASH),
            commit: 1
        })
    );
    // The same branch written again at a later commit gets a row of its own.
    let second = trie.insert(&mut tx, 2, a, &b.path, b.hash).unwrap();
    assert_eq!(second.hash(&HASH), first.hash(&HASH));
    assert!(matches!(second, RootRef::Branch(b) if b.commit == 2));
    assert_eq!(tx.get(TABLE.table(), &trie_key(2)).unwrap(), row);
    assert_eq!(tx.get(TABLE.table(), &trie_key(1)).unwrap(), row);
}

fn trie_key(commit: u64) -> Vec<u8> {
    Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH).node_key(commit, &[])
}

#[test]
fn branch_depth_is_checked_in_context() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let child = compact(vec![0; 5], 10, 3, 3, vec![[0; 32], [1; 32]], vec![]).unwrap();
    let child_hash = child.hash(TEST_BRANCH_DOMAIN, &HASH);
    for slot in [0, 1] {
        tx.put(
            TABLE.table(),
            &trie.node_key(0, &[0, 0, slot]),
            &child.encode(),
        )
        .unwrap();
    }
    let parent = compact(vec![0], 2, 3, 3, vec![child_hash, child_hash], vec![]).unwrap();
    let parent_hash = parent.hash(TEST_BRANCH_DOMAIN, &HASH);
    tx.put(TABLE.table(), &trie.node_key(0, &[]), &parent.encode())
        .unwrap();
    let branch = |hash| RootRef::Branch(BranchRef { hash, commit: 0 });
    assert!(matches!(
        trie.get(&tx, branch(parent_hash), &[0; 6]),
        Err(MerkleError::InvalidNode(_))
    ));
    let terminal = compact(vec![0; 6], 11, 3, 3, vec![[0; 32], [1; 32]], vec![]).unwrap();
    let hash = terminal.hash(TEST_BRANCH_DOMAIN, &HASH);
    tx.put(TABLE.table(), &trie.node_key(0, &[]), &terminal.encode())
        .unwrap();
    assert!(matches!(
        trie.get(&tx, branch(hash), &[0; 6]),
        Err(MerkleError::InvalidNode(_))
    ));
}

#[test]
fn child_commits_round_trip_unhashed_and_counts_are_strict() {
    let leaf_path = [0x10, 0, 0, 0, 0, 0];
    let mixed = |commits: Vec<u64>| {
        BranchNodeCompact::<6>::new(
            vec![],
            0,
            0b11,
            0b01,
            vec![[1; 32], [2; 32]],
            vec![leaf_path],
            commits,
        )
    };
    let node = mixed(vec![7]).unwrap();
    assert_eq!(
        BranchNodeCompact::<6>::decode(&node.encode()).unwrap(),
        node
    );
    // Commits locate rows; they are stored but never hashed.
    assert_eq!(node.hash_payload(), mixed(vec![8]).unwrap().hash_payload());
    assert_eq!(
        node.child(0),
        Some(RootRef::Branch(BranchRef {
            hash: [1; 32],
            commit: 7
        }))
    );
    assert_eq!(
        node.child(1),
        Some(RootRef::Leaf(LeafRef::new(leaf_path, [2; 32])))
    );
    // One commit per branch child, exactly.
    for count in [0, 2] {
        assert!(mixed(vec![0; count]).is_err());
    }
    let good = node.encode();
    for extra in [1, 4, 8] {
        let mut bytes = good.clone();
        bytes.extend(vec![0; extra]);
        assert!(BranchNodeCompact::<6>::decode(&bytes).is_err());
    }
    for missing in [1, 4] {
        assert!(BranchNodeCompact::<6>::decode(&good[..good.len() - missing]).is_err());
    }
}
