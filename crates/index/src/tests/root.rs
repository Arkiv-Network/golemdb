use std::ops::RangeBounds;

use golemdb_cells::CellType;
use golemdb_merkle::HASH_BYTES;
use proptest::prelude::*;

use crate::index_trie::IndexTriePath;
use crate::term::IndexKey;
use crate::{Hi48, INDEX_TRIE_PATH_BYTES, IndexTerm, TermRange};

#[test]
fn container_trie_paths_are_the_term_path_then_its_hi48() {
    let hi48 = Hi48::new(0x0102_0304).unwrap();
    let trie = IndexTriePath::new(&[7; HASH_BYTES], hi48);
    let trie = trie.as_ref();
    assert_eq!(trie.len(), INDEX_TRIE_PATH_BYTES);
    assert_eq!(trie[..HASH_BYTES], [7; HASH_BYTES]);
    assert_eq!(trie[HASH_BYTES..], hi48.to_bytes());
    let key = IndexKey::new(b"term", hi48);
    assert_eq!(key.as_ref()[4..], trie[HASH_BYTES..]);
    assert_eq!(Hi48::from_slice(&key.as_ref()[4..]), Some(hi48));
    assert_eq!(Hi48::from_slice(&[0; 2]), None);
    // A real term's key, so a bad suffix is what fails the split.
    let term = str_term(b"x");
    let key = IndexKey::new(&term.key(), hi48);
    assert_eq!(IndexKey::split(key.as_ref()).unwrap(), (term, hi48));
    assert!(IndexKey::split(&[key.as_ref(), &[0]].concat()).is_err());
    assert!(IndexKey::split(&key.as_ref()[..key.as_ref().len() - 1]).is_err());
}

fn str_term(value: &[u8]) -> IndexTerm {
    IndexTerm::new("s", CellType::Str, value).unwrap()
}

#[test]
fn string_keys_end_with_a_terminator_and_reject_zeros() {
    let prefix = IndexTerm::prefix("s", CellType::Str).unwrap();
    let key = str_term(b"ab").key();
    assert_eq!(key, [&prefix[..], b"ab\0"].concat());
    assert!(IndexTerm::new("s", CellType::Str, b"a\0b").is_err());
    let fixed = IndexTerm::new("n", CellType::Uint(golemdb_cells::Width::W8), &[0; 8]);
    let fixed = fixed.unwrap();
    assert_eq!(fixed.key(), fixed.as_bytes());
    for tail in [&b""[..], b"a"] {
        let bad = [&prefix[..], tail].concat();
        assert!(IndexTerm::split_key(&bad).is_err(), "{bad:?}");
    }
    assert!(IndexTerm::split_key(b"s").is_err());
    assert!(IndexTerm::split_key(b"s\0").is_err());
}

proptest! {
    #[test]
    fn container_keys_keep_term_order_and_split_back(
        a in prop::collection::vec(prop::sample::select(&[1u8, b'a', b'b'][..]), 0..5),
        b in prop::collection::vec(prop::sample::select(&[1u8, b'a', b'b'][..]), 0..5),
        ha in 0u64..4, hb in 0u64..4,
    ) {
        let (ta, tb) = (str_term(&a), str_term(&b));
        let (ha, hb) = (Hi48::new(ha).unwrap(), Hi48::new(hb).unwrap());
        let ka = IndexKey::new(&ta.key(), ha).as_ref().to_vec();
        let kb = IndexKey::new(&tb.key(), hb).as_ref().to_vec();
        // Containers of a term are contiguous, ordered by term, then container.
        prop_assert_eq!(ka.cmp(&kb), (&ta, ha).cmp(&(&tb, hb)));
        let (term, hi48) = IndexKey::split(&ka).unwrap();
        prop_assert_eq!(term, ta.clone());
        prop_assert_eq!(hi48, ha);
        // Every key-space range holds exactly the containers of its terms.
        let prefix = String::from_utf8_lossy(&b).into_owned();
        if prefix.as_bytes() == b.as_slice() {
            let range = TermRange::with_prefix(golemdb_cells::CellNameRef::raw(b"s"), &prefix).unwrap();
            let (lower, upper) = range.into_key_bounds();
            prop_assert_eq!((lower, upper).contains(&ka), a.starts_with(&b));
        }
    }
}
