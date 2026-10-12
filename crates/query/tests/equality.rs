//! Filter semantics against a real database: which records an `Eq` leaf
//! matches, for every indexable type, as records are created, patched and
//! deleted.

mod support;

use std::collections::BTreeMap;

use golemdb_api::{Api, QueryError, RecordOp};
use golemdb_cells::{CellType, CellValue};
use golemdb_index::TermError;
use proptest::prelude::*;
use support::{TestDb, eq, key, record, small_value};

fn f32_value(value: f32) -> CellValue {
    CellValue::from_f32(value).expect("not NaN")
}

fn f64_value(value: f64) -> CellValue {
    CellValue::from_f64(value).expect("not NaN")
}

/// Boundary values of every type. All distinct, so each has its own term.
fn samples() -> Vec<CellValue> {
    let i256_min = {
        let mut v = [0; 32];
        v[0] = 0x80;
        v
    };
    let i256_max = {
        let mut v = [0xff; 32];
        v[0] = 0x7f;
        v
    };
    vec![
        CellValue::from_bool(false),
        CellValue::from_bool(true),
        CellValue::from_str(""),
        CellValue::from_str("a"),
        CellValue::from_str("é𝄞\u{10FFFF}"),
        CellValue::from_str(&"long".repeat(256)),
        CellValue::from_bytes4([0; 4]),
        CellValue::from_bytes4([0xff; 4]),
        CellValue::from_bytes8([0xff; 8]),
        CellValue::from_bytes16([0xff; 16]),
        CellValue::from_bytes20([0; 20]),
        CellValue::from_bytes20([0xff; 20]),
        CellValue::from_bytes32([0; 32]),
        CellValue::from_u32(0),
        CellValue::from_u32(u32::MAX),
        CellValue::from_u64(0),
        CellValue::from_u64(u64::MAX),
        CellValue::from_u128(u128::MAX),
        CellValue::from_u256_be([0; 32]),
        CellValue::from_u256_be([0xff; 32]),
        CellValue::from_i32(i32::MIN),
        CellValue::from_i32(-1),
        CellValue::from_i32(0),
        CellValue::from_i32(i32::MAX),
        CellValue::from_i64(i64::MIN),
        CellValue::from_i64(i64::MAX),
        CellValue::from_i128(i128::MIN),
        CellValue::from_i128(i128::MAX),
        CellValue::from_i256_be(i256_min),
        CellValue::from_i256_be([0xff; 32]),
        CellValue::from_i256_be(i256_max),
        CellValue::from_dec32_unscaled(i32::MIN),
        CellValue::from_dec64_unscaled(-1),
        CellValue::from_dec128_unscaled(i128::MAX),
        CellValue::from_dec256_unscaled_be(i256_min),
        f32_value(f32::NEG_INFINITY),
        f32_value(-f32::from_bits(1)),
        f32_value(0.0),
        f32_value(f32::MAX),
        f32_value(f32::INFINITY),
        f64_value(f64::NEG_INFINITY),
        f64_value(f64::MIN),
        f64_value(0.0),
        f64_value(f64::from_bits(1)),
        f64_value(f64::INFINITY),
        CellValue::from_date32(i32::MIN),
        CellValue::from_date32(0),
        CellValue::from_date32(i32::MAX),
        CellValue::from_timestamp64(i64::MIN),
        CellValue::from_timestamp64(i64::MAX),
    ]
}

#[test]
fn every_type_round_trips_through_the_index() {
    let samples = samples();
    let test = TestDb::default();
    test.create(
        samples
            .iter()
            .enumerate()
            .map(|(i, value)| record(key(i as u64), &[("f", value.clone())])),
    );
    for (i, value) in samples.iter().enumerate() {
        assert_eq!(
            test.keys(eq("f", value.clone())).unwrap(),
            [key(i as u64)],
            "{value:?}"
        );
        assert_eq!(test.count(eq("f", value.clone())).unwrap(), 1, "{value:?}");
    }
}

#[test]
fn values_never_stored_match_nothing() {
    let test = TestDb::default();
    test.create([record(key(1), &[("f", CellValue::from_i32(1))])]);
    for value in [
        CellValue::from_i32(2),
        CellValue::from_u32(1),
        CellValue::from_i64(1),
        CellValue::from_dec32_unscaled(1),
        CellValue::from_str("1"),
    ] {
        assert!(
            test.keys(eq("f", value.clone())).unwrap().is_empty(),
            "{value:?}"
        );
        assert_eq!(test.count(eq("f", value.clone())).unwrap(), 0, "{value:?}");
    }
    assert!(
        test.keys(eq("g", CellValue::from_i32(1)))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn only_attributes_are_indexed() {
    let test = TestDb::default();
    let blue = CellValue::from_str("blue");
    test.create([
        record(key(1), &[("color", blue.clone())]),
        RecordOp::create(key(2))
            .field("color", blue.clone())
            .expect("valid field"),
    ]);
    assert_eq!(test.keys(eq("color", blue)).unwrap(), [key(1)]);
}

#[test]
fn patched_and_deleted_records_stop_matching() {
    let test = TestDb::default();
    let blue = CellValue::from_str("blue");
    let red = CellValue::from_str("red");
    test.create((1..=4).map(|n| record(key(n), &[("color", blue.clone())])));

    let branch = test.db.begin().unwrap();
    test.db
        .patch(
            branch,
            RecordOp::patch(key(2))
                .attribute("color", red.clone())
                .unwrap(),
        )
        .unwrap();
    test.db
        .patch(branch, RecordOp::patch(key(3)).remove("color").unwrap())
        .unwrap();
    test.db.delete(branch, RecordOp::delete(key(4))).unwrap();
    test.db.commit(branch).unwrap();

    assert_eq!(test.keys(eq("color", blue)).unwrap(), [key(1)]);
    assert_eq!(test.keys(eq("color", red)).unwrap(), [key(2)]);
}

#[test]
fn one_record_under_many_terms() {
    let test = TestDb::default();
    test.create([
        record(
            key(9),
            &[
                ("a", CellValue::from_i32(1)),
                ("ab", CellValue::from_i32(1)),
                ("b", CellValue::from_u32(1)),
            ],
        ),
        record(key(10), &[("a", CellValue::from_i32(2))]),
    ]);
    assert_eq!(
        test.keys(eq("a", CellValue::from_i32(1))).unwrap(),
        [key(9)]
    );
    assert_eq!(
        test.keys(eq("ab", CellValue::from_i32(1))).unwrap(),
        [key(9)]
    );
    assert_eq!(
        test.keys(eq("b", CellValue::from_u32(1))).unwrap(),
        [key(9)]
    );
    assert!(
        test.keys(eq("b", CellValue::from_i32(1)))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        test.keys(eq("a", CellValue::from_i32(2))).unwrap(),
        [key(10)]
    );
}

#[test]
fn invalid_arguments_are_errors_not_empty_results() {
    let test = TestDb::default();
    assert!(matches!(
        test.keys(eq("not valid", CellValue::from_i32(1))),
        Err(QueryError::InvalidArgument {
            source: TermError::InvalidName,
            ..
        })
    ));
    assert!(matches!(
        test.count(eq("f", CellValue::from_bytes(b"x"))),
        Err(QueryError::InvalidArgument {
            source: TermError::UnsupportedType(CellType::Bytes),
            ..
        })
    ));
}

const FIELDS: [&str; 3] = ["a", "ab", "b"];

fn field() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&FIELDS[..])
}

proptest! {
    // Each case builds and commits a whole database.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn equality_matches_a_model(
        records in prop::collection::vec(
            prop::collection::btree_map(field(), small_value(), 0..4),
            0..24,
        ),
        filters in prop::collection::vec(
            (field(), small_value(), any::<Option<prop::sample::Index>>()),
            1..12,
        ),
    ) {
        let test = TestDb::default();
        test.create(records.iter().enumerate().map(|(n, cells)| {
            let cells: Vec<_> = cells.iter().map(|(field, value)| (*field, value.clone())).collect();
            record(key(n as u64), &cells)
        }));
        let stored: Vec<(&str, &CellValue)> = records
            .iter()
            .flat_map(|cells| cells.iter().map(|(field, value)| (*field, value)))
            .collect();
        for (field, value, pick) in filters {
            // Mostly ask for cells that exist, so matches are not rare.
            let (field, value) = match pick {
                Some(pick) if !stored.is_empty() => {
                    let (field, value) = stored[pick.index(stored.len())];
                    (field, value.clone())
                }
                Some(_) | None => (field, value),
            };
            // A record matches iff its cell under `field` has the leaf's type
            // and value. Records are created in order, so record IDs, and
            // with them the query order, follow `n`.
            let expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(_, cells): &(usize, &BTreeMap<&str, CellValue>)| {
                    cells.get(field) == Some(&value)
                })
                .map(|(n, _)| key(n as u64))
                .collect();
            prop_assert_eq!(test.count(eq(field, value.clone())).unwrap(), expected.len() as u64);
            prop_assert_eq!(test.keys(eq(field, value)).unwrap(), expected);
        }
    }
}
