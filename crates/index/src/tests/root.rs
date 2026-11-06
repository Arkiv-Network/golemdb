use std::ops::RangeBounds;

use golemdb_cells::CellType;
use golemdb_merkle::HASH_BYTES;
use proptest::prelude::*;

use crate::index_trie::IndexTriePath;
use crate::{ContainerId, INDEX_TRIE_PATH_BYTES, IndexKey, IndexTerm, TermRange};

#[test]
fn container_trie_paths_are_the_term_path_then_its_hi48() {
    let container_id = ContainerId::new(0x0102_0304).unwrap();
    let trie = IndexTriePath::new(&[7; HASH_BYTES], container_id);
    let trie = trie.as_ref();
    assert_eq!(trie.len(), INDEX_TRIE_PATH_BYTES);
    assert_eq!(trie[..HASH_BYTES], [7; HASH_BYTES]);
    assert_eq!(trie[HASH_BYTES..], container_id.to_bytes());
    let term = IndexTerm::new("n", CellType::Bool, &[1]).unwrap();
    let index_key = IndexKey {
        term: term.clone(),
        container_id,
    };
    let key = index_key.encode();
    assert_eq!(key[term.as_bytes().len()..], trie[HASH_BYTES..]);
    assert_eq!(IndexKey::decode(&key).unwrap(), index_key);
    assert!(IndexKey::decode(&key[..key.len() - 1]).is_err());
    assert!(IndexKey::decode(&[&key[..], &[0]].concat()).is_err());
}

fn str_term(value: &[u8]) -> IndexTerm {
    IndexTerm::new("s", CellType::Str, value).unwrap()
}

#[test]
fn string_terms_end_with_a_terminator_and_reject_zeros() {
    let prefix = IndexTerm::prefix("s", CellType::Str).unwrap();
    assert_eq!(
        str_term(b"ab").into_bytes(),
        [&prefix[..], b"ab\0"].concat()
    );
    assert!(IndexTerm::new("s", CellType::Str, b"a\0b").is_err());
    let ty = CellType::Uint(golemdb_cells::Width::W8);
    let fixed = IndexTerm::new("n", ty, &[0; 8]).unwrap();
    let fixed_prefix = IndexTerm::prefix("n", ty).unwrap();
    assert_eq!(fixed.into_bytes(), [&fixed_prefix[..], &[0; 8]].concat());
    // No zero byte in the ID, so none can stand in for the terminator.
    let id = ContainerId::new(0x0102_0304_0506).unwrap().to_bytes();
    for tail in [&b""[..], b"a"] {
        let bad = [&prefix[..], tail, &id].concat();
        assert!(IndexKey::decode(&bad).is_err(), "{bad:?}");
    }
    assert!(IndexKey::decode(b"s").is_err());
    assert!(IndexKey::decode(b"s\0").is_err());
}

proptest! {
    #[test]
    fn container_keys_keep_term_order_and_split_back(
        a in prop::collection::vec(prop::sample::select(&[1u8, b'a', b'b'][..]), 0..5),
        b in prop::collection::vec(prop::sample::select(&[1u8, b'a', b'b'][..]), 0..5),
        ha in 0u64..4, hb in 0u64..4,
    ) {
        let (ta, tb) = (str_term(&a), str_term(&b));
        let (ha, hb) = (ContainerId::new(ha).unwrap(), ContainerId::new(hb).unwrap());
        let ka = IndexKey { term: ta.clone(), container_id: ha }.encode();
        let kb = IndexKey { term: tb.clone(), container_id: hb }.encode();
        // Containers of a term are contiguous, ordered by term, then container.
        prop_assert_eq!(ka.cmp(&kb), (&ta, ha).cmp(&(&tb, hb)));
        let key = IndexKey::decode(&ka).unwrap();
        prop_assert_eq!(key, IndexKey { term: ta.clone(), container_id: ha });
        // Every key-space range holds exactly the containers of its terms.
        let prefix = String::from_utf8_lossy(&b).into_owned();
        if prefix.as_bytes() == b.as_slice() {
            let range = TermRange::with_prefix(golemdb_cells::CellNameRef::raw(b"s"), &prefix).unwrap();
            let (lower, upper) = crate::key::bounds(range.bounds());
            prop_assert_eq!((lower, upper).contains(&ka), a.starts_with(&b));
        }
    }
}
