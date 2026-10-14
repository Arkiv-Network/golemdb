//! Filter semantics against a real database: which records a `Range` leaf
//! matches, for every indexable type, and which `Range` leaves admission
//! rejects.

mod support;

use std::{cmp::Ordering, collections::BTreeMap, ops::Bound};

use golemdb_api::{ApiError, RecordKey};
use golemdb_cells::{CellType, CellValue};
use golemdb_index::TermError;
use proptest::prelude::*;
use support::{
    TestDb, is_between, key, native_cmp, range, record, small_bounds_of_one_type, small_value,
};

fn f32_value(value: f32) -> CellValue {
    CellValue::from_f32(value).expect("not NaN")
}

fn f64_value(value: f64) -> CellValue {
    CellValue::from_f64(value).expect("not NaN")
}

/// A 256-bit big-endian word: `head` then `fill`.
fn word(head: u8, fill: u8) -> [u8; 32] {
    let mut word = [fill; 32];
    word[0] = head;
    word
}

/// Values of every indexable type, one list per type, in ascending order,
/// from its least value to its greatest.
fn ascending_per_type() -> Vec<Vec<CellValue>> {
    let signed = |min: i128, max: i128| [min, -1, 0, 1, max];
    vec![
        vec![CellValue::from_bool(false), CellValue::from_bool(true)],
        ["", "a", "ab", "b", "é", "𝄞", "\u{10FFFF}"]
            .map(CellValue::from_str)
            .into(),
        [
            [0; 4],
            [0, 0, 0, 1],
            [0x7f, 0xff, 0xff, 0xff],
            [0x80, 0, 0, 0],
            [0xff; 4],
        ]
        .map(CellValue::from_bytes4)
        .into(),
        [[0; 8], [1; 8], [0xff; 8]]
            .map(CellValue::from_bytes8)
            .into(),
        [[0; 16], [1; 16], [0xff; 16]]
            .map(CellValue::from_bytes16)
            .into(),
        [[0; 20], [1; 20], [0xff; 20]]
            .map(CellValue::from_bytes20)
            .into(),
        [[0; 32], [1; 32], [0xff; 32]]
            .map(CellValue::from_bytes32)
            .into(),
        [0, 1, 0x8000_0000, u32::MAX]
            .map(CellValue::from_u32)
            .into(),
        [0, 1, 1 << 63, u64::MAX].map(CellValue::from_u64).into(),
        [0, 1, 1 << 127, u128::MAX].map(CellValue::from_u128).into(),
        [word(0, 0), word(0, 1), word(0x80, 0), word(0xff, 0xff)]
            .map(CellValue::from_u256_be)
            .into(),
        signed(i32::MIN.into(), i32::MAX.into())
            .map(|n| CellValue::from_i32(n.try_into().expect("i32")))
            .into(),
        signed(i64::MIN.into(), i64::MAX.into())
            .map(|n| CellValue::from_i64(n.try_into().expect("i64")))
            .into(),
        signed(i128::MIN, i128::MAX)
            .map(CellValue::from_i128)
            .into(),
        [
            word(0x80, 0),
            word(0xff, 0xff),
            word(0, 0),
            word(0, 1),
            word(0x7f, 0xff),
        ]
        .map(CellValue::from_i256_be)
        .into(),
        signed(i32::MIN.into(), i32::MAX.into())
            .map(|n| CellValue::from_dec32_unscaled(n.try_into().expect("i32")))
            .into(),
        signed(i64::MIN.into(), i64::MAX.into())
            .map(|n| CellValue::from_dec64_unscaled(n.try_into().expect("i64")))
            .into(),
        signed(i128::MIN, i128::MAX)
            .map(CellValue::from_dec128_unscaled)
            .into(),
        [
            word(0x80, 0),
            word(0xff, 0xff),
            word(0, 0),
            word(0x7f, 0xff),
        ]
        .map(CellValue::from_dec256_unscaled_be)
        .into(),
        [
            f32::NEG_INFINITY,
            f32::MIN,
            -1.5,
            -f32::from_bits(1),
            0.0,
            f32::from_bits(1),
            f32::MIN_POSITIVE,
            f32::MAX,
            f32::INFINITY,
        ]
        .map(f32_value)
        .into(),
        [
            f64::NEG_INFINITY,
            f64::MIN,
            -1.5,
            -f64::from_bits(1),
            0.0,
            f64::from_bits(1),
            f64::MAX,
            f64::INFINITY,
        ]
        .map(f64_value)
        .into(),
        [i32::MIN, -1, 0, i32::MAX]
            .map(CellValue::from_date32)
            .into(),
        [i64::MIN, -1, 0, i64::MAX]
            .map(CellValue::from_timestamp64)
            .into(),
    ]
}

#[test]
fn the_fixture_is_ascending_and_covers_every_indexable_type() {
    let lists = ascending_per_type();
    for list in &lists {
        for pair in list.windows(2) {
            assert_eq!(
                native_cmp(&pair[0], &pair[1]),
                Some(Ordering::Less),
                "{pair:?}"
            );
        }
    }
    // 23 indexable types: all but variable-length bytes.
    assert_eq!(lists.len(), 23);
}

#[test]
fn ranges_follow_each_types_order_and_stay_in_its_run() {
    let lists = ascending_per_type();
    // All values under one name, so every type's run has neighbours. Within
    // a type, records are created in descending order, so record ID order
    // (the query order) is the reverse of value order.
    let mut keys: Vec<Vec<RecordKey>> = Vec::new();
    let mut next = 0;
    for list in &lists {
        let ids: Vec<_> = (next..next + list.len() as u64).map(key).collect();
        next += list.len() as u64;
        keys.push(ids);
    }
    let test = TestDb::default();
    test.create(lists.iter().zip(&keys).flat_map(|(list, keys)| {
        list.iter()
            .zip(keys)
            .rev()
            .map(|(value, key)| record(*key, &[("f", value.clone())]))
    }));
    for (list, keys) in lists.iter().zip(&keys) {
        // Matches as keys in query order: ascending record ID.
        let expect = |values: std::ops::Range<usize>| -> Vec<RecordKey> {
            let mut matched: Vec<_> = keys[values].to_vec();
            matched.reverse();
            matched
        };
        let [first, .., last] = list.as_slice() else {
            panic!("every type lists two values or more");
        };
        let n = list.len();
        for (lower, upper, expected) in [
            (Bound::Included(first), Bound::Included(last), expect(0..n)),
            (
                Bound::Excluded(first),
                Bound::Excluded(last),
                expect(1..n - 1),
            ),
            (Bound::Included(first), Bound::Unbounded, expect(0..n)),
            (Bound::Unbounded, Bound::Included(last), expect(0..n)),
            (Bound::Excluded(first), Bound::Unbounded, expect(1..n)),
            (Bound::Unbounded, Bound::Excluded(last), expect(0..n - 1)),
        ] {
            let filter = range("f", lower.cloned(), upper.cloned());
            assert_eq!(test.keys(filter.clone()).unwrap(), expected, "{filter:?}");
            assert_eq!(test.count(filter).unwrap(), expected.len() as u64);
        }
        for (i, value) in list.iter().enumerate() {
            let point = range(
                "f",
                Bound::Included(value.clone()),
                Bound::Included(value.clone()),
            );
            assert_eq!(test.keys(point).unwrap(), expect(i..i + 1), "{value:?}");
            let below = range("f", Bound::Unbounded, Bound::Excluded(value.clone()));
            assert_eq!(test.keys(below).unwrap(), expect(0..i), "{value:?}");
            let from = range("f", Bound::Included(value.clone()), Bound::Unbounded);
            assert_eq!(test.keys(from).unwrap(), expect(i..n), "{value:?}");
        }
    }
}

#[test]
fn negative_zero_is_zero() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("x", f64_value(-0.0))]),
        record(key(2), &[("x", f64_value(-1.0))]),
    ]);
    let zero = |x: f64| {
        range(
            "x",
            Bound::Included(f64_value(x)),
            Bound::Included(f64_value(x)),
        )
    };
    assert_eq!(test.keys(zero(0.0)).unwrap(), [key(1)]);
    assert_eq!(test.keys(zero(-0.0)).unwrap(), [key(1)]);
    let below = range("x", Bound::Unbounded, Bound::Excluded(f64_value(0.0)));
    assert_eq!(test.keys(below).unwrap(), [key(2)]);
}

#[test]
fn names_sharing_a_prefix_are_distinct() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("a", CellValue::from_u32(1))]),
        record(key(2), &[("ab", CellValue::from_u32(1))]),
        record(key(3), &[("a0", CellValue::from_u32(1))]),
        record(key(4), &[("b", CellValue::from_u32(1))]),
    ]);
    let filter = range(
        "a",
        Bound::Included(CellValue::from_u32(0)),
        Bound::Unbounded,
    );
    assert_eq!(test.keys(filter).unwrap(), [key(1)]);
    let filter = range(
        "a",
        Bound::Unbounded,
        Bound::Included(CellValue::from_u32(u32::MAX)),
    );
    assert_eq!(test.keys(filter).unwrap(), [key(1)]);
}

#[test]
fn malformed_range_is_an_invalid_query() {
    let test = TestDb::default();
    test.create([record(key(1), &[("f", CellValue::from_i32(1))])]);
    let i = CellValue::from_i32;
    for filter in [
        range("f", Bound::Unbounded, Bound::Unbounded),
        range(
            "f",
            Bound::Included(i(1)),
            Bound::Excluded(CellValue::from_i64(2)),
        ),
        range("f", Bound::Included(i(1)), Bound::Included(i(-1))),
        range("f", Bound::Excluded(i(1)), Bound::Included(i(1))),
        range("f", Bound::Included(i(1)), Bound::Excluded(i(1))),
    ] {
        assert!(
            matches!(
                test.keys(filter.clone()),
                Err(ApiError::InvalidQuery { .. })
            ),
            "{filter:?}"
        );
        assert!(
            matches!(
                test.count(filter.clone()),
                Err(ApiError::InvalidQuery { .. })
            ),
            "{filter:?}"
        );
    }
}

#[test]
fn invalid_arguments_are_errors_not_empty_results() {
    let test = TestDb::default();
    let cause = |result: Result<Vec<RecordKey>, ApiError>| match result {
        Err(ApiError::InvalidArgument {
            source: Some(source),
            ..
        }) => source.downcast_ref::<TermError>().copied(),
        _ => None,
    };
    let one = || Bound::Included(CellValue::from_i32(1));
    assert_eq!(
        cause(test.keys(range("not valid", one(), Bound::Unbounded))),
        Some(TermError::InvalidName)
    );
    let bytes = Bound::Included(CellValue::from_bytes(b"x"));
    assert_eq!(
        cause(test.keys(range("f", Bound::Unbounded, bytes))),
        Some(TermError::UnsupportedType(CellType::Bytes))
    );
}

const FIELDS: [&str; 3] = ["a", "ab", "b"];

fn field() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&FIELDS[..])
}

/// Whether admission accepts the bounds: at least one, and not inverted in
/// the native order. Bounds of two types are left to the unit tests.
fn well_formed(lower: &Bound<CellValue>, upper: &Bound<CellValue>) -> bool {
    match (lower, upper) {
        (Bound::Unbounded, Bound::Unbounded) => false,
        (Bound::Included(low), Bound::Included(high)) => {
            native_cmp(low, high).is_some_and(Ordering::is_le)
        }
        (
            Bound::Included(low) | Bound::Excluded(low),
            Bound::Included(high) | Bound::Excluded(high),
        ) => native_cmp(low, high).is_some_and(Ordering::is_lt),
        (Bound::Unbounded, _) | (_, Bound::Unbounded) => true,
    }
}

proptest! {
    // Each case builds and commits a whole database.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn ranges_match_a_model(
        records in prop::collection::vec(
            prop::collection::btree_map(field(), small_value(), 0..4),
            0..24,
        ),
        filters in prop::collection::vec((field(), small_bounds_of_one_type()), 1..12),
    ) {
        let test = TestDb::default();
        test.create(records.iter().enumerate().map(|(n, cells)| {
            let cells: Vec<_> = cells.iter().map(|(field, value)| (*field, value.clone())).collect();
            record(key(n as u64), &cells)
        }));
        for (field, (lower, upper)) in filters {
            let filter = range(field, lower.clone(), upper.clone());
            if !well_formed(&lower, &upper) {
                let result = test.keys(filter);
                prop_assert!(matches!(result, Err(ApiError::InvalidQuery { .. })), "{:?}", result);
                continue;
            }
            // A record matches iff its cell under `field` has the bounds' type
            // and lies between them in that type's native order. Records are
            // created in order, so record IDs, and with them the query order,
            // follow `n`.
            let expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(_, cells): &(usize, &BTreeMap<&str, CellValue>)| {
                    cells.get(field).is_some_and(|cell| is_between(cell, &lower, &upper))
                })
                .map(|(n, _)| key(n as u64))
                .collect();
            prop_assert_eq!(test.count(filter.clone()).unwrap(), expected.len() as u64);
            prop_assert_eq!(test.keys(filter).unwrap(), expected);
        }
    }
}
