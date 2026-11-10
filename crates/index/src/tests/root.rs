use std::ops::RangeBounds;

use golemdb_cells::CellType;
use golemdb_merkle::HASH_BYTES;
use proptest::prelude::*;

use crate::index_trie::IndexTriePath;
use crate::key::term_key;
use crate::{HI48_BYTES, INDEX_TRIE_PATH_BYTES, IndexKey, IndexTerm, TermRange};

#[test]
fn container_trie_paths_are_the_term_path_then_the_container_path() {
    let trie = IndexTriePath::new(&[7; HASH_BYTES], 0x0102_0304).unwrap();
    let trie = trie.as_ref();
    assert_eq!(trie.len(), INDEX_TRIE_PATH_BYTES);
    assert_eq!(trie[..HASH_BYTES], [7; HASH_BYTES]);
    assert_eq!(trie[HASH_BYTES..].len(), HI48_BYTES);
    assert!(IndexTriePath::new(&[0; HASH_BYTES], u64::MAX).is_err());
    let term = IndexTerm::new("n", CellType::Bool, &[1]).unwrap();
    let index_key = IndexKey {
        term: term.clone(),
        container_id: 0x0102_0304,
    };
    let key = index_key.encode().unwrap();
    assert_eq!(key[term.as_bytes().len()..], trie[HASH_BYTES..]);
    assert_eq!(IndexKey::decode(&key).unwrap(), index_key);
    assert!(IndexKey::decode(&key[..key.len() - 1]).is_err());
    assert!(IndexKey::decode(&[&key[..], &[0]].concat()).is_err());
}

fn str_term(value: &[u8]) -> IndexTerm {
    IndexTerm::new("s", CellType::Str, value).unwrap()
}

#[test]
fn string_keys_end_with_a_terminator_and_reject_zeros() {
    let prefix = IndexTerm::prefix("s", CellType::Str).unwrap();
    let key = term_key(str_term(b"ab").as_bytes());
    assert_eq!(key, [&prefix[..], b"ab\0"].concat());
    assert!(IndexTerm::new("s", CellType::Str, b"a\0b").is_err());
    let fixed = IndexTerm::new("n", CellType::Uint(golemdb_cells::Width::W8), &[0; 8]);
    let fixed = fixed.unwrap();
    assert_eq!(term_key(fixed.as_bytes()), fixed.as_bytes());
    // No zero byte in the ID, so none can stand in for the terminator.
    let id = crate::path::path_bytes(0x0102_0304_0506).unwrap();
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
        let ka = IndexKey { term: ta.clone(), container_id: ha }.encode().unwrap();
        let kb = IndexKey { term: tb.clone(), container_id: hb }.encode().unwrap();
        // Containers of a term are contiguous, ordered by term, then path.
        prop_assert_eq!(ka.cmp(&kb), (&ta, ha).cmp(&(&tb, hb)));
        let key = IndexKey::decode(&ka).unwrap();
        prop_assert_eq!(key, IndexKey { term: ta.clone(), container_id: ha });
        // Every key-space range holds exactly the containers of its terms.
        let prefix = String::from_utf8_lossy(&b).into_owned();
        if prefix.as_bytes() == b.as_slice() {
            let range = TermRange::with_prefix(golemdb_cells::CellNameRef::raw(b"s"), &prefix).unwrap();
            let (lower, upper) = range.into_key_bounds();
            prop_assert_eq!((lower, upper).contains(&ka), a.starts_with(&b));
        }
    }
}
