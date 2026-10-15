use crate::{BitmapContainer, IndexTerm, TermError};
use golemdb_cells::{
    CellType as T, CellValueRef, FloatWidth as F, Width as W, encode_float, flip_sign,
};
use golemdb_merkle::{Hash, HashProvider, Keccak256Hasher};
use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction, scan, scan_prefix};
use proptest::prelude::*;
use std::{cell::RefCell, ops::Bound::Included};

#[test]
fn literal_term_vectors() {
    for (name, ty, value, expected) in [
        ("B", T::Bool, vec![1], vec![b'B', 0, 0x81, 1]),
        (
            "S",
            T::Str,
            b"abc".to_vec(),
            vec![b'S', 0, 0x82, b'a', b'b', b'c'],
        ),
        ("S", T::Str, vec![], vec![b'S', 0, 0x82]),
        (
            "I",
            T::Int(W::W4),
            (-1i32).to_be_bytes().to_vec(),
            vec![b'I', 0, 0x90, 0x7f, 0xff, 0xff, 0xff],
        ),
        (
            "I",
            T::Int(W::W4),
            i32::MIN.to_be_bytes().to_vec(),
            vec![b'I', 0, 0x90, 0, 0, 0, 0],
        ),
        (
            "U",
            T::Uint(W::W4),
            1u32.to_be_bytes().to_vec(),
            vec![b'U', 0, 0x8c, 0, 0, 0, 1],
        ),
        (
            "F",
            T::Float(F::F32),
            (-1f32).to_be_bytes().to_vec(),
            vec![b'F', 0, 0x98, 0x40, 0x7f, 0xff, 0xff],
        ),
        (
            "F",
            T::Float(F::F32),
            1f32.to_be_bytes().to_vec(),
            vec![b'F', 0, 0x98, 0xbf, 0x80, 0, 0],
        ),
        (
            "F",
            T::Float(F::F32),
            (-0f32).to_be_bytes().to_vec(),
            vec![b'F', 0, 0x98, 0x80, 0, 0, 0],
        ),
    ] {
        let term = IndexTerm::new(name, ty, &value).unwrap();
        assert_eq!(term.as_bytes(), expected);
        assert_eq!(IndexTerm::decode(&expected).unwrap(), term);
    }
}

#[test]
fn every_supported_family_and_width_has_canonical_order() {
    for width in [W::W4, W::W8, W::W16, W::W32] {
        let len = width.bytes();
        for ty in [T::Int(width), T::Decimal(width)] {
            let mut min = vec![0; len];
            min[0] = 0x80;
            let minus_one = vec![255; len];
            let zero = vec![0; len];
            let mut max = vec![255; len];
            max[0] = 0x7f;
            assert_order(ty, &[min, minus_one, zero, max]);
        }
        for ty in [T::Uint(width), T::FixedBytes(width)] {
            assert_order(ty, &[vec![0; len], vec![1; len], vec![255; len]]);
        }
    }
    assert_order(T::Bytes20, &[vec![0; 20], vec![255; 20]]);
    assert_order(T::Bool, &[vec![0], vec![1]]);
    assert_order(
        T::Date32,
        &[
            i32::MIN.to_be_bytes().to_vec(),
            0i32.to_be_bytes().to_vec(),
            i32::MAX.to_be_bytes().to_vec(),
        ],
    );
    assert_order(
        T::Timestamp64,
        &[
            i64::MIN.to_be_bytes().to_vec(),
            0i64.to_be_bytes().to_vec(),
            i64::MAX.to_be_bytes().to_vec(),
        ],
    );
    assert_order(
        T::Str,
        &[
            b"".to_vec(),
            b"a".to_vec(),
            b"a\0".to_vec(),
            b"aa".to_vec(),
            b"b".to_vec(),
            "é".as_bytes().to_vec(),
        ],
    );
}

fn assert_order(ty: T, values: &[Vec<u8>]) {
    let terms: Vec<_> = values
        .iter()
        .map(|v| IndexTerm::new("Value", ty, v).unwrap())
        .collect();
    for pair in terms.windows(2) {
        assert!(pair[0] < pair[1], "{ty:?}");
    }
    for term in terms {
        assert_eq!(IndexTerm::decode(term.as_bytes()).unwrap(), term);
    }
}

#[test]
fn floats_reject_nan_and_normalize_zero() {
    for bits in [0x7f80_0001u32, 0x7fc0_0000, 0xffff_ffff] {
        assert!(matches!(
            IndexTerm::new("F", T::Float(F::F32), &bits.to_be_bytes()),
            Err(TermError::NaN)
        ));
    }
    for bits in [
        0x7ff0_0000_0000_0001u64,
        0x7ff8_0000_0000_0000,
        0xffff_ffff_ffff_ffff,
    ] {
        assert!(matches!(
            IndexTerm::new("F", T::Float(F::F64), &bits.to_be_bytes()),
            Err(TermError::NaN)
        ));
    }
    assert_eq!(
        IndexTerm::new("F", T::Float(F::F64), &(-0f64).to_be_bytes()).unwrap(),
        IndexTerm::new("F", T::Float(F::F64), &0f64.to_be_bytes()).unwrap()
    );
    assert_order(
        T::Float(F::F32),
        &[
            f32::NEG_INFINITY,
            -f32::MAX,
            -1.,
            -f32::from_bits(1),
            0.,
            f32::from_bits(1),
            1.,
            f32::MAX,
            f32::INFINITY,
        ]
        .map(|v| v.to_be_bytes().to_vec()),
    );
    assert_order(
        T::Float(F::F64),
        &[
            f64::NEG_INFINITY,
            -f64::MAX,
            -1.,
            -f64::from_bits(1),
            0.,
            f64::from_bits(1),
            1.,
            f64::MAX,
            f64::INFINITY,
        ]
        .map(|v| v.to_be_bytes().to_vec()),
    );
    // Ordered -0 must be rejected in persisted input instead of normalized.
    assert!(IndexTerm::decode(&[b'F', 0, 0x98, 0x7f, 0xff, 0xff, 0xff]).is_err());
    assert!(
        IndexTerm::decode(&[
            b'F', 0, 0x99, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
        ])
        .is_err()
    );
}

#[test]
fn field_filtering_names_and_unsupported_types() {
    let value = 0u64.to_be_bytes();
    let field = CellValueRef::new(T::Uint(W::W8), &value, false).unwrap();
    assert_eq!(IndexTerm::from_cell("#nextRecordID", field).unwrap(), None);
    let cell = CellValueRef::new(T::Bool, &[1], true).unwrap();
    for name in [
        "Price",
        "$owner",
        "price.usd",
        "erc20:balance",
        "created-at",
        "snake_case",
    ] {
        assert!(IndexTerm::from_cell(name, cell).unwrap().is_some());
    }
    for name in [
        "", "$", "$$a", "a$b", "0name", "#key", "@admin", "a\0b", "a b", "é",
    ] {
        assert!(matches!(
            IndexTerm::from_cell(name, cell),
            Err(TermError::InvalidName)
        ));
    }
    assert!(matches!(
        IndexTerm::new("A", T::Bytes, b""),
        Err(TermError::UnsupportedType(_))
    ));
    assert!(IndexTerm::new("A", T::Bool, &[2]).is_err());
    assert!(IndexTerm::new("A", T::Int(W::W4), &[1]).is_err());
    assert!(IndexTerm::new("A", T::Str, &[255]).is_err());
}

#[test]
fn strict_term_decoding() {
    for invalid in [
        vec![],
        b"Name".to_vec(),
        b"Name\0".to_vec(),
        vec![b'A', 0, 1, 1],
        vec![b'A', 0, 0x80],
        vec![b'A', 0, 0x85],
        vec![b'A', 0, 0x83],
        vec![b'A', 0, 0x81, 1, 1],
        vec![b'A', 0, 0x90],
        vec![255, 0, 0x81, 1],
    ] {
        assert!(IndexTerm::decode(&invalid).is_err(), "{invalid:?}");
    }
}

#[derive(Default)]
struct Recorder(RefCell<Vec<Vec<u8>>>);
impl HashProvider for Recorder {
    fn hash_parts(&self, parts: &[&[u8]]) -> Hash {
        self.0.borrow_mut().push(parts.concat());
        [self.0.borrow().len() as u8; 32]
    }
}

#[test]
fn leaf_preimages_belong_to_index_and_use_the_supplied_provider() {
    let term = IndexTerm::new("A", T::Bool, &[1]).unwrap();
    let hasher = Recorder::default();
    assert_eq!(term.leaf_hash(&[7; 32], &hasher), [2; 32]);
    assert_eq!(
        *hasher.0.borrow(),
        vec![
            vec![b'A', 0, 0x81, 1],
            [&[2][..], &[1; 32], &[7; 32]].concat()
        ]
    );
    let chunk = BitmapContainer::from_values(1, [42]).unwrap();
    assert_eq!(chunk.leaf_hash(&hasher).unwrap(), [3; 32]);
    assert_eq!(
        hasher.0.borrow()[2],
        [&[4][..], chunk.canonical_bytes().unwrap().as_slice()].concat()
    );
    assert!(
        BitmapContainer::empty(0)
            .unwrap()
            .leaf_hash(&hasher)
            .is_err()
    );
    let keccak = Keccak256Hasher;
    assert_ne!(
        chunk.leaf_hash(&keccak).unwrap(),
        BitmapContainer::from_values(2, [42])
            .unwrap()
            .leaf_hash(&keccak)
            .unwrap()
    );
    assert_ne!(
        term.leaf_hash(&[7; 32], &keccak),
        IndexTerm::new("B", T::Bool, &[1])
            .unwrap()
            .leaf_hash(&[7; 32], &keccak)
    );
}

#[test]
fn ordered_terms_drive_storage_range_and_prefix_scans() {
    let db = MemoryStore::new();
    let table = Table("Index");
    let mut tx = db.begin_write().unwrap();
    for value in [-10i32, 0, 5, 20] {
        let term = IndexTerm::new("Price", T::Int(W::W4), &value.to_be_bytes()).unwrap();
        tx.put(table, term.as_bytes(), &value.to_be_bytes())
            .unwrap();
    }
    tx.put(
        table,
        IndexTerm::new("Price", T::Uint(W::W4), &5u32.to_be_bytes())
            .unwrap()
            .as_bytes(),
        b"different type",
    )
    .unwrap();
    for name in ["Alice", "Alicia", "Bob"] {
        tx.put(
            table,
            IndexTerm::new("Name", T::Str, name.as_bytes())
                .unwrap()
                .as_bytes(),
            b"root",
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    let low = IndexTerm::new("Price", T::Int(W::W4), &(-10i32).to_be_bytes())
        .unwrap()
        .into_bytes();
    let high = IndexTerm::new("Price", T::Int(W::W4), &5i32.to_be_bytes())
        .unwrap()
        .into_bytes();
    let values: Vec<_> = scan(&read, table, Included(low), Included(high))
        .unwrap()
        .map(|r| i32::from_be_bytes(r.unwrap().1.try_into().unwrap()))
        .collect();
    assert_eq!(values, [-10, 0, 5]);
    let mut prefix = IndexTerm::prefix("Name", T::Str).unwrap();
    prefix.extend_from_slice(b"Ali");
    assert_eq!(scan_prefix(&read, table, prefix).unwrap().count(), 2);
}

proptest! {
    #[test]
    fn f32_order_matches_numeric_comparison(a in any::<u32>(),b in any::<u32>()) {
        let (a,b)=(f32::from_bits(a),f32::from_bits(b));
        prop_assume!(!a.is_nan() && !b.is_nan());
        let a_term=IndexTerm::new("A",T::Float(F::F32),&a.to_be_bytes()).unwrap();
        let b_term=IndexTerm::new("A",T::Float(F::F32),&b.to_be_bytes()).unwrap();
        prop_assert_eq!(a.partial_cmp(&b).unwrap(),a_term.cmp(&b_term));
        prop_assert_eq!(IndexTerm::decode(a_term.as_bytes()).unwrap(),a_term);
    }
    #[test]
    fn strings_preserve_utf8_byte_order(a in ".{0,80}",b in ".{0,80}") {
        let a_term=IndexTerm::new("A",T::Str,a.as_bytes()).unwrap();
        let b_term=IndexTerm::new("A",T::Str,b.as_bytes()).unwrap();
        prop_assert_eq!(a.as_bytes().cmp(b.as_bytes()),a_term.cmp(&b_term));
        prop_assert_eq!(IndexTerm::decode(a_term.as_bytes()).unwrap(),a_term);
    }
    #[test]
    fn signed_integer_order(a in any::<i64>(), b in any::<i64>()) {
        let a_term=IndexTerm::new("A",T::Int(W::W8),&a.to_be_bytes()).unwrap();
        let b_term=IndexTerm::new("A",T::Int(W::W8),&b.to_be_bytes()).unwrap();
        prop_assert_eq!(a.cmp(&b),a_term.cmp(&b_term));
    }
    #[test]
    fn f64_order_matches_numeric_comparison(a in any::<u64>(),b in any::<u64>()) {
        let (a,b)=(f64::from_bits(a),f64::from_bits(b));
        prop_assume!(!a.is_nan() && !b.is_nan());
        let a_term=IndexTerm::new("A",T::Float(F::F64),&a.to_be_bytes()).unwrap();
        let b_term=IndexTerm::new("A",T::Float(F::F64),&b.to_be_bytes()).unwrap();
        prop_assert_eq!(a.partial_cmp(&b).unwrap(),a_term.cmp(&b_term));
        prop_assert_eq!(IndexTerm::decode(a_term.as_bytes()).unwrap(),a_term);
    }
    #[test]
    fn arbitrary_decode_is_total_and_canonical(bytes in prop::collection::vec(any::<u8>(),0..512)) {
        if let Ok(term)=IndexTerm::decode(&bytes) { prop_assert_eq!(term.into_bytes(),bytes); }
    }
}

/// Cell payloads already have the index's ordered representation. Constructing
/// from a cell must agree with native input, preserve bytes, and remain readable.
fn assert_cell_term(ty: T, native: &[u8], ordered: &[u8]) -> IndexTerm {
    let cell = CellValueRef::new(ty, ordered, true).unwrap();
    let term = IndexTerm::from_cell("Value", cell).unwrap().unwrap();
    let expected = [b"Value\0".as_slice(), &[cell.metadata()], ordered].concat();
    assert_eq!(term.as_bytes(), expected, "{ty:?}");
    assert_eq!(IndexTerm::decode(&expected).unwrap(), term, "{ty:?}");
    assert_eq!(IndexTerm::new("Value", ty, native).unwrap(), term, "{ty:?}");
    term
}

#[test]
fn cell_signed_decimal_and_time_values_are_not_encoded_twice() {
    for width in [W::W4, W::W8, W::W16, W::W32] {
        let size = width.bytes();
        let mut min = vec![0; size];
        min[0] = 0x80;
        let mut max = vec![0xff; size];
        max[0] = 0x7f;
        for ty in [T::Int(width), T::Decimal(width)] {
            let mut terms = Vec::new();
            for raw in [&min, &vec![0xff; size], &vec![0; size], &max] {
                let mut ordered = raw.clone();
                ordered[0] ^= 0x80;
                terms.push(assert_cell_term(ty, raw, &ordered));
            }
            assert!(terms.windows(2).all(|w| w[0] < w[1]));
        }
    }
    for native in [i32::MIN, -1, 0, i32::MAX] {
        assert_cell_term(
            T::Date32,
            &native.to_be_bytes(),
            &flip_sign(native.to_be_bytes()),
        );
    }
    for native in [i64::MIN, -1, 0, i64::MAX] {
        assert_cell_term(
            T::Timestamp64,
            &native.to_be_bytes(),
            &flip_sign(native.to_be_bytes()),
        );
    }
}

#[test]
fn cell_float_values_are_not_encoded_twice() {
    let mut terms = Vec::new();
    for value in [
        f32::NEG_INFINITY,
        -f32::MAX,
        -1.,
        -f32::from_bits(1),
        0.,
        f32::from_bits(1),
        1.,
        f32::MAX,
        f32::INFINITY,
    ] {
        terms.push(assert_cell_term(
            T::Float(F::F32),
            &value.to_be_bytes(),
            &encode_float(value.to_be_bytes()),
        ));
    }
    assert!(terms.windows(2).all(|w| w[0] < w[1]));
    let mut terms = Vec::new();
    for value in [
        f64::NEG_INFINITY,
        -f64::MAX,
        -1.,
        -f64::from_bits(1),
        0.,
        f64::from_bits(1),
        1.,
        f64::MAX,
        f64::INFINITY,
    ] {
        terms.push(assert_cell_term(
            T::Float(F::F64),
            &value.to_be_bytes(),
            &encode_float(value.to_be_bytes()),
        ));
    }
    assert!(terms.windows(2).all(|w| w[0] < w[1]));
    assert_cell_term(
        T::Float(F::F32),
        &(-0.0f32).to_be_bytes(),
        &encode_float((-0.0f32).to_be_bytes()),
    );
    assert_cell_term(
        T::Float(F::F64),
        &(-0.0f64).to_be_bytes(),
        &encode_float((-0.0f64).to_be_bytes()),
    );
}

#[test]
fn cell_plain_values_preserve_their_bytes_and_fields_are_omitted() {
    for (ty, bytes) in [(T::Bool, &b"\x01"[..]), (T::Str, b"a\0b"), (T::Str, b"")] {
        assert_cell_term(ty, bytes, bytes);
    }
    assert_cell_term(T::Bytes20, &[0xab; 20], &[0xab; 20]);
    for width in [W::W4, W::W8, W::W16, W::W32] {
        let bytes = vec![0x7f; width.bytes()];
        for ty in [T::Uint(width), T::FixedBytes(width)] {
            assert_cell_term(ty, &bytes, &bytes);
        }
    }
    for (ty, bytes) in [
        (T::Bytes, &b"\xff\0"[..]),
        (T::Int(W::W4), &b"\x7f\xff\xff\xff"[..]),
    ] {
        let field = CellValueRef::new(ty, bytes, false).unwrap();
        assert_eq!(IndexTerm::from_cell("#key", field).unwrap(), None);
    }
}
