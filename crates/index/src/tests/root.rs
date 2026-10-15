use crate::index_trie::{decode_bitmap_root, decode_root};
use crate::{BITMAP_TRIE_PATH_BYTES, IndexError};
use golemdb_merkle::{BranchRef, HashProvider, Keccak256Hasher, LeafRef, RootRef};

const HASH: Keccak256Hasher = Keccak256Hasher;

#[test]
fn index_row_locates_branch_and_singleton_roots() {
    let branch = RootRef::<BITMAP_TRIE_PATH_BYTES>::Branch(BranchRef {
        hash: [1; 32],
        commit: 0x0102_0304_0506_0708,
    });
    let singleton = RootRef::Leaf(LeafRef {
        path: [9, 8, 7, 6, 5, 4],
        hash: [2; 32],
        commit: 42,
    });
    let bytes = branch.encode();
    assert_eq!(bytes.len(), 40);
    assert_eq!(bytes[32..], 0x0102_0304_0506_0708u64.to_be_bytes());
    assert_eq!(decode_bitmap_root(&bytes, &HASH).unwrap(), branch);
    let bytes = singleton.encode();
    assert_eq!(bytes.len(), 46);
    assert_eq!(bytes[40..], [9, 8, 7, 6, 5, 4]);
    assert_eq!(decode_bitmap_root(&bytes, &HASH).unwrap(), singleton);
    // The committed hash is the first 32 bytes, whatever the location says.
    assert_eq!(decode_root(&bytes, &HASH).unwrap(), [2; 32]);
}

#[test]
fn malformed_index_rows_are_corruption() {
    let good = RootRef::<BITMAP_TRIE_PATH_BYTES>::Leaf(LeafRef::new([0; 6], [3; 32])).encode();
    for len in [0, 32, 39, 41, 45, 47, 72] {
        let bytes: Vec<u8> = good.iter().copied().cycle().take(len).collect();
        assert!(matches!(
            decode_bitmap_root(&bytes, &HASH),
            Err(IndexError::Corruption(_))
        ));
    }
    let mut empty = good;
    empty[..32].copy_from_slice(&HASH.hash(&[]));
    assert!(matches!(
        decode_root(&empty, &HASH),
        Err(IndexError::Corruption(_))
    ));
}
