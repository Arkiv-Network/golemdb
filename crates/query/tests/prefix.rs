//! Filter semantics against a real database: which records a `Prefix` leaf
//! matches, and which `Prefix` leaves admission rejects.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use golemdb_api::{ApiError, RecordKey, RecordOp};
use golemdb_cells::CellValue;
use golemdb_index::TermError;
use proptest::prelude::*;
use support::{TestDb, key, prefix, record, small_value};

fn str(value: &str) -> CellValue {
    CellValue::from_str(value)
}

/// Strings sharing prefixes, with multibyte chars up to the greatest one.
const STRINGS: [&str; 14] = [
    "",
    "a",
    "aB",
    "ab",
    "abc",
    "ab\u{10FFFF}",
    "ac",
    "b",
    "e",
    "é",
    "éa",
    "𝄞",
    "\u{10FFFF}",
    "\u{10FFFF}a",
];

#[test]
fn prefixes_match_the_strings_that_start_with_them() {
    // Created in reverse, so record ID order (the query order) is the
    // reverse of the list. The neighbours never match: another type under
    // `f`, with the same leading bytes, and names `f` is a prefix of or that
    // differ in case.
    let test = TestDb::default();
    test.create(
        STRINGS
            .iter()
            .enumerate()
            .rev()
            .map(|(n, value)| record(key(n as u64), &[("f", str(value))]))
            .chain([
                record(key(100), &[("f", CellValue::from_bytes4(*b"abcd"))]),
                record(key(101), &[("f", CellValue::from_u32(0x6162_6364))]),
                record(key(102), &[("fa", str("abc"))]),
                record(key(103), &[("F", str("abc"))]),
            ]),
    );
    // Every char-boundary prefix of every string, and a few that start none.
    let prefixes: BTreeSet<&str> = STRINGS
        .iter()
        .flat_map(|value| {
            value
                .char_indices()
                .map(|(end, _)| &value[..end])
                .chain([*value])
        })
        .chain(["abd", "c", "é\u{0}", "\u{10FFFF}b"])
        .collect();
    for start in prefixes {
        let expected: Vec<RecordKey> = STRINGS
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, value)| value.starts_with(start))
            .map(|(n, _)| key(n as u64))
            .collect();
        let filter = prefix("f", start);
        assert_eq!(test.keys(filter.clone()).unwrap(), expected, "{start:?}");
        assert_eq!(
            test.count(filter).unwrap(),
            expected.len() as u64,
            "{start:?}"
        );
    }
}

#[test]
fn the_empty_prefix_matches_every_str_cell_of_its_field() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("f", str(""))]),
        record(key(2), &[("f", CellValue::from_bool(true))]),
        record(key(3), &[("f", str("x"))]),
        record(key(4), &[("g", str("x"))]),
    ]);
    assert_eq!(test.keys(prefix("f", "")).unwrap(), [key(1), key(3)]);
    assert_eq!(test.count(prefix("f", "")).unwrap(), 2);
}

#[test]
fn only_attributes_match_a_prefix() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("f", str("abc"))]),
        RecordOp::create(key(2), golemdb_api::Budget::Limited(u64::MAX)).field("f", str("abc")),
    ]);
    for start in ["", "a", "abc"] {
        assert_eq!(
            test.keys(prefix("f", start)).unwrap(),
            [key(1)],
            "{start:?}"
        );
        assert_eq!(test.count(prefix("f", start)).unwrap(), 1, "{start:?}");
    }
}

#[test]
fn an_invalid_field_is_an_error_not_an_empty_result() {
    let test = TestDb::default();
    let result = test.keys(prefix("not valid", "a"));
    let Err(ApiError::InvalidArgument {
        source: Some(source),
        ..
    }) = &result
    else {
        panic!("expected InvalidArgument, got {result:?}");
    };
    assert_eq!(
        source.downcast_ref::<TermError>(),
        Some(&TermError::InvalidName)
    );
}

const FIELDS: [&str; 3] = ["a", "ab", "b"];

fn field() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&FIELDS[..])
}

/// A short string from a tiny alphabet, so prefixes are common. Two of its
/// chars are multibyte, one of them the greatest char.
fn small_str() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(&["a", "b", "é", "\u{10FFFF}"][..]),
        0..4,
    )
    .prop_map(|chars| chars.concat())
}

proptest! {
    // Each case builds and commits a whole database.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prefixes_match_a_model(
        records in prop::collection::vec(
            prop::collection::btree_map(
                field(),
                prop_oneof![3 => small_str().prop_map(|value| str(&value)), 1 => small_value()],
                0..4,
            ),
            0..24,
        ),
        filters in prop::collection::vec((field(), small_str()), 1..12),
    ) {
        let test = TestDb::default();
        test.create(records.iter().enumerate().map(|(n, cells)| {
            let cells: Vec<_> = cells.iter().map(|(field, value)| (*field, value.clone())).collect();
            record(key(n as u64), &cells)
        }));
        for (field, start) in filters {
            // A record matches iff its cell under `field` is a `str` that
            // starts with `start`. Records are created in order, so record
            // IDs, and with them the query order, follow `n`.
            let expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(_, cells): &(usize, &BTreeMap<&str, CellValue>)| {
                    cells
                        .get(field)
                        .and_then(CellValue::as_str)
                        .is_some_and(|value| value.starts_with(&start))
                })
                .map(|(n, _)| key(n as u64))
                .collect();
            let filter = prefix(field, &start);
            prop_assert_eq!(test.count(filter.clone()).unwrap(), expected.len() as u64);
            prop_assert_eq!(test.keys(filter).unwrap(), expected);
        }
    }
}
