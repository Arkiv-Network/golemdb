//! Filter semantics against a real database: which records an `In` leaf
//! matches, and which `In` leaves admission rejects.

mod support;

use std::collections::BTreeMap;

use golemdb_api::{
    ApiError, CellNameRef, Filter, InValues, RecordKey,
    marker::{Date32, Dec32, I32},
};
use golemdb_cells::CellValue;
use golemdb_index::TermError;
use proptest::prelude::*;
use support::{TestDb, eq, is_in, key, record, small_value, small_values_of_one_type};

fn str(value: &str) -> CellValue {
    CellValue::from_str(value)
}

#[test]
fn in_matches_records_holding_any_of_its_values() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("color", str("blue"))]),
        record(key(2), &[("color", str("red"))]),
        record(key(3), &[("color", str("green"))]),
        record(key(4), &[("color", str("blue"))]),
        record(key(5), &[("shape", str("blue"))]),
    ]);
    // Matches come in record ID order, whatever the order of the values.
    let filter = is_in("color", [str("red"), str("pink"), str("blue")]);
    assert_eq!(test.keys(filter.clone()).unwrap(), [key(1), key(2), key(4)]);
    assert_eq!(test.count(filter).unwrap(), 3);
    assert!(test.keys(is_in("color", [str("pink")])).unwrap().is_empty());
}

#[test]
fn repeated_values_match_once() {
    let test = TestDb::default();
    test.create([record(key(1), &[("n", CellValue::from_u64(7))])]);
    let filter = is_in("n", [7, 7, 7].map(CellValue::from_u64));
    assert_eq!(test.keys(filter.clone()).unwrap(), [key(1)]);
    assert_eq!(test.count(filter).unwrap(), 1);
}

#[test]
fn other_types_under_the_same_name_are_absent() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("amount", CellValue::from_i32(1))]),
        record(key(2), &[("amount", CellValue::from_u32(1))]),
        record(key(3), &[("amount", CellValue::from_i32(-1))]),
        record(key(4), &[("amount", str("1"))]),
    ]);
    let i32s = is_in("amount", [1, -1].map(CellValue::from_i32));
    assert_eq!(test.keys(i32s).unwrap(), [key(1), key(3)]);
    let u32s = is_in("amount", [1, 2].map(CellValue::from_u32));
    assert_eq!(test.keys(u32s).unwrap(), [key(2)]);
}

#[test]
fn typed_values_match_as_their_runtime_form_does() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("f", CellValue::from_dec32_unscaled(5))]),
        record(key(2), &[("f", CellValue::from_i32(5))]),
        record(key(3), &[("f", CellValue::from_date32(5))]),
    ]);
    let typed = |values| Filter::In {
        field: CellNameRef::parse_user(b"f", 32)
            .expect("valid name")
            .into(),
        values,
    };
    // One Rust value, three cell types: the marker decides which matches.
    assert_eq!(
        test.keys(typed(InValues::of(Dec32, [5, 7]))).unwrap(),
        [key(1)]
    );
    assert_eq!(test.keys(typed(InValues::of(I32, [5]))).unwrap(), [key(2)]);
    assert_eq!(
        test.keys(typed(InValues::of(Date32, [5]))).unwrap(),
        [key(3)]
    );
    let runtime = is_in(
        "f",
        [
            CellValue::from_dec32_unscaled(7),
            CellValue::from_dec32_unscaled(5),
        ],
    );
    assert_eq!(runtime, typed(InValues::of(Dec32, [5, 7])));
}

#[test]
fn an_in_with_no_value_matches_nothing() {
    let test = TestDb::default();
    test.create([record(key(1), &[("f", CellValue::from_i32(1))])]);
    assert!(test.keys(is_in("f", [])).unwrap().is_empty());
    assert_eq!(test.count(is_in("f", [])).unwrap(), 0);
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
    assert_eq!(
        cause(test.keys(is_in("not valid", [CellValue::from_i32(1)]))),
        Some(TermError::InvalidName)
    );
    assert_eq!(
        cause(test.keys(is_in("not valid", []))),
        Some(TermError::InvalidName)
    );
}

const FIELDS: [&str; 3] = ["a", "ab", "b"];

fn field() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&FIELDS[..])
}

proptest! {
    // Each case builds and commits a whole database.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn membership_matches_a_model(
        records in prop::collection::vec(
            prop::collection::btree_map(field(), small_value(), 0..4),
            0..24,
        ),
        filters in prop::collection::vec((field(), small_values_of_one_type(1..6)), 1..12),
    ) {
        let test = TestDb::default();
        test.create(records.iter().enumerate().map(|(n, cells)| {
            let cells: Vec<_> = cells.iter().map(|(field, value)| (*field, value.clone())).collect();
            record(key(n as u64), &cells)
        }));
        for (field, values) in filters {
            // A record matches iff its cell under `field` equals one of the
            // values, type included. Records are created in order, so record
            // IDs, and with them the query order, follow `n`.
            let expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(_, cells): &(usize, &BTreeMap<&str, CellValue>)| {
                    cells.get(field).is_some_and(|cell| values.contains(cell))
                })
                .map(|(n, _)| key(n as u64))
                .collect();
            let filter = is_in(field, values.clone());
            prop_assert_eq!(test.count(filter.clone()).unwrap(), expected.len() as u64);
            prop_assert_eq!(test.keys(filter).unwrap(), expected.clone());
            if let [value] = values.as_slice() {
                prop_assert_eq!(test.keys(eq(field, value.clone())).unwrap(), expected);
            }
        }
    }
}
