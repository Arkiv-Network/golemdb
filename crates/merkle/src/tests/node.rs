use crate::{
    BranchNodeCompact, HashProvider, Keccak256Hasher, LeafRef, MerkleError, RootRef, Trie,
};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction};
use proptest::prelude::*;

// Test domains are fixture inputs, independent of application assignments.
const TEST_BRANCH_DOMAIN: u8 = 0x05;
const OTHER_BRANCH_DOMAIN: u8 = 0x03;

const HASH: Keccak256Hasher = Keccak256Hasher;
const TABLE: Table = Table("Branches");

fn node() -> BranchNodeCompact<6> {
    BranchNodeCompact::new(
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
        Some(RootRef::Leaf(LeafRef {
            path: [0xab, 0xcd, 0, 0, 0, 0],
            hash: [0x11; 32]
        }))
    );
    assert_eq!(
        node.child(14),
        Some(RootRef::Leaf(LeafRef {
            path: [0xab, 0xce, 0, 0, 0, 0],
            hash: [0x22; 32]
        }))
    );
    assert_eq!(node.child(12), None);
    assert_eq!(node.child(16), None);
    assert_eq!(node.child(255), None);
    assert_eq!(RootRef::<6>::Empty.hash(&HASH), HASH.hash(b""));
    assert_eq!(
        RootRef::Leaf(LeafRef {
            path: [0; 6],
            hash: [42; 32]
        })
        .hash(&HASH),
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
        assert!(BranchNodeCompact::<6>::new(vec![], 0, state, 0, vec![], vec![]).is_err());
    }
    assert!(
        BranchNodeCompact::<6>::new(vec![0; 6], 12, 3, 0, vec![[0; 32]; 2], vec![[0; 6]; 2])
            .is_err()
    );
    assert!(BranchNodeCompact::<6>::new(vec![], 0, 3, 0, vec![[0; 32]], vec![[0; 6]; 2]).is_err());
    assert!(BranchNodeCompact::<6>::new(vec![], 0, 3, 0, vec![[0; 32]; 2], vec![[0; 6]]).is_err());
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
    let root = RootRef::Branch(hash);
    let leaf = LeafRef {
        path: node.leaf_paths()[0],
        hash: node.child_hashes()[0],
    };
    let mut bytes = node.encode();
    bytes[7] ^= 1;
    tx.put(TABLE, &hash, &bytes).unwrap();
    assert!(matches!(
        trie.get(&tx, root, &leaf.path),
        Err(MerkleError::HashMismatch)
    ));
    assert!(matches!(
        trie.insert(&mut tx, root, leaf),
        Err(MerkleError::HashMismatch)
    ));
    assert!(matches!(
        trie.remove(&mut tx, root, &leaf.path),
        Err(MerkleError::HashMismatch)
    ));
    let mut bytes = node.encode();
    bytes[71] ^= 1; // Unhashed first leaf path, inconsistent with branch prefix.
    tx.put(TABLE, &hash, &bytes).unwrap();
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
fn collisions_check_complete_stored_bytes_and_never_overwrite() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let node = node();
    let hash = node.hash(TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let mut bytes = node.encode();
    *bytes.last_mut().unwrap() = 1;
    tx.insert(TABLE, &hash, &bytes).unwrap();
    let a = node.child(13).unwrap();
    let RootRef::Leaf(b) = node.child(14).unwrap() else {
        unreachable!()
    };
    assert!(matches!(
        trie.insert(&mut tx, a, b),
        Err(MerkleError::ConflictingBranch)
    ));
    assert_eq!(tx.get(TABLE, &hash).unwrap(), Some(bytes));
    tx.abort();
    let mut tx = db.begin_write().unwrap();
    tx.insert(TABLE, &hash, &node.encode()).unwrap();
    assert_eq!(trie.insert(&mut tx, a, b).unwrap(), RootRef::Branch(hash));
}

#[test]
fn branch_depth_is_checked_in_context() {
    let db = MemoryStore::new();
    let trie = Trie::<_, 6>::new(TABLE, TEST_BRANCH_DOMAIN, &HASH);
    let mut tx = db.begin_write().unwrap();
    let child =
        BranchNodeCompact::<6>::new(vec![0; 5], 10, 3, 3, vec![[0; 32], [1; 32]], vec![]).unwrap();
    let child_hash = child.hash(TEST_BRANCH_DOMAIN, &HASH);
    tx.put(TABLE, &child_hash, &child.encode()).unwrap();
    let parent =
        BranchNodeCompact::<6>::new(vec![0], 2, 3, 3, vec![child_hash, child_hash], vec![])
            .unwrap();
    let parent_hash = parent.hash(TEST_BRANCH_DOMAIN, &HASH);
    tx.put(TABLE, &parent_hash, &parent.encode()).unwrap();
    assert!(matches!(
        trie.get(&tx, RootRef::Branch(parent_hash), &[0; 6]),
        Err(MerkleError::InvalidNode(_))
    ));
    let terminal =
        BranchNodeCompact::<6>::new(vec![0; 6], 11, 3, 3, vec![[0; 32], [1; 32]], vec![]).unwrap();
    let hash = terminal.hash(TEST_BRANCH_DOMAIN, &HASH);
    tx.put(TABLE, &hash, &terminal.encode()).unwrap();
    assert!(matches!(
        trie.get(&tx, RootRef::Branch(hash), &[0; 6]),
        Err(MerkleError::InvalidNode(_))
    ));
}
