use std::ops::RangeBounds;

use golemdb_cells::CellType;
use proptest::prelude::*;

use crate::index_trie::{row_container, row_key, trie_path};
use crate::{HI48_BYTES, INDEX_TRIE_PATH_BYTES, IndexTerm, TermRange};

#[test]
fn container_trie_paths_are_the_term_path_then_the_container_path() {
    let path = trie_path(&[7; 32], 0x0102_0304).unwrap();
    assert_eq!(path.len(), INDEX_TRIE_PATH_BYTES);
    assert_eq!(path[..32], [7; 32]);
    assert_eq!(path[32..].len(), HI48_BYTES);
    assert!(trie_path(&[0; 32], u64::MAX).is_err());
    let key = row_key(b"term", 0x0102_0304).unwrap();
    assert_eq!(key[4..], path[32..]);
    assert_eq!(row_container(&key[4..]).unwrap(), 0x0102_0304);
    assert!(row_container(&[0; 2]).is_err());
}

fn str_term(value: &[u8]) -> IndexTerm {
    IndexTerm::new("s", CellType::Str, value).unwrap()
}

#[test]
fn string_keys_escape_zeros_and_end_with_a_terminator() {
    let prefix = IndexTerm::prefix("s", CellType::Str).unwrap();
    let key = str_term(b"a\0b").key();
    assert_eq!(key, [&prefix[..], b"a\0\xffb\0\x01"].concat());
    let fixed = IndexTerm::new("n", CellType::Uint(golemdb_cells::Width::W8), &[0; 8]);
    let fixed = fixed.unwrap();
    assert_eq!(fixed.key(), fixed.as_bytes());
    for tail in [&b""[..], b"a", b"a\0", b"a\0\x02"] {
        let bad = [&prefix[..], tail].concat();
        assert!(IndexTerm::split_key(&bad).is_err(), "{bad:?}");
    }
    assert!(IndexTerm::split_key(b"s").is_err());
    assert!(IndexTerm::split_key(b"s\0").is_err());
}

proptest! {
    #[test]
    fn container_keys_keep_term_order_and_split_back(
        a in prop::collection::vec(prop::sample::select(&[0u8, 1, b'a', b'b'][..]), 0..5),
        b in prop::collection::vec(prop::sample::select(&[0u8, 1, b'a', b'b'][..]), 0..5),
        ha in 0u64..4, hb in 0u64..4,
    ) {
        let (ta, tb) = (str_term(&a), str_term(&b));
        let (ka, kb) = (row_key(&ta.key(), ha).unwrap(), row_key(&tb.key(), hb).unwrap());
        // Containers of a term are contiguous, ordered by term, then path.
        prop_assert_eq!(ka.cmp(&kb), (&ta, ha).cmp(&(&tb, hb)));
        let (term, rest) = IndexTerm::split_key(&ka).unwrap();
        prop_assert_eq!(term, ta.clone());
        prop_assert_eq!(row_container(rest).unwrap(), ha);
        // Every key-space range holds exactly the containers of its terms.
        let prefix = String::from_utf8_lossy(&b).into_owned();
        if prefix.as_bytes() == b.as_slice() {
            let range = TermRange::with_prefix(golemdb_cells::CellNameRef::raw(b"s"), &prefix).unwrap();
            let (lower, upper) = range.into_key_bounds();
            prop_assert_eq!((lower, upper).contains(&ka), a.starts_with(&b));
        }
    }
}
