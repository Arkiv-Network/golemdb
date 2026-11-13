//! Filter semantics against a real database: which records an `Exists` leaf
//! matches, of one type and of any, as records are created, patched and
//! deleted, and which `Exists` leaves admission rejects.

mod support;

use std::collections::BTreeMap;

use golemdb_api::{
    Api, ApiError, Budget, CellNameRef, ExistsType, Filter, RecordOp,
    marker::{Date32, Dec32, I32},
};
use golemdb_cells::{CellType, CellValue, Width};
use golemdb_index::TermError;
use proptest::prelude::*;
use support::{TestDb, exists, indexable_type, key, record, small_value, value_of_each_type};

const U32: CellType = CellType::Uint(Width::W4);

fn u32(n: u32) -> CellValue {
    CellValue::from_u32(n)
}

fn str(value: &str) -> CellValue {
    CellValue::from_str(value)
}

/// A record per indexable type, each holding `f` of that type, created in
/// reverse, so record ID order (the query order) is the reverse of
/// [`value_of_each_type`]. Then records that hold no `f`: one with no cell
/// at all, one with another name, one with names `f` is a prefix of (`-` is
/// the least byte a name continues with), one with `f` in another case.
fn one_record_per_type() -> TestDb {
    let test = TestDb::default();
    test.create(
        value_of_each_type()
            .into_iter()
            .enumerate()
            .rev()
            .map(|(n, value)| record(key(n as u64), &[("f", value)]))
            .chain([
                record(key(100), &[]),
                record(key(101), &[("g", u32(1))]),
                record(key(102), &[("f-", str("x")), ("fa", u32(1))]),
                record(key(103), &[("F", u32(1))]),
            ]),
    );
    test
}

#[test]
fn exists_of_one_type_matches_the_records_holding_the_field_in_that_type() {
    let test = one_record_per_type();
    for (n, value) in value_of_each_type().iter().enumerate() {
        let filter = exists("f", Some(value.cell_type()));
        assert_eq!(
            test.keys(filter.clone()).unwrap(),
            [key(n as u64)],
            "{value:?}"
        );
        assert_eq!(test.count(filter).unwrap(), 1, "{value:?}");
    }
}

#[test]
fn exists_of_no_type_matches_the_records_holding_the_field_in_any_type() {
    let test = one_record_per_type();
    let types = value_of_each_type().len();
    let expected: Vec<_> = (0..types).rev().map(|n| key(n as u64)).collect();
    assert_eq!(test.keys(exists("f", None)).unwrap(), expected);
    assert_eq!(test.count(exists("f", None)).unwrap(), types as u64);
}

#[test]
fn only_attributes_match_an_exists() {
    // A field is not indexed, so it never matches. Variable-length `bytes`
    // can only be a field.
    let test = TestDb::default();
    let budget = Budget::Limited(u64::MAX);
    test.create([
        record(key(1), &[("f", u32(1))]),
        RecordOp::create(key(2), budget).field("f", u32(1)),
        RecordOp::create(key(3), budget).field("f", CellValue::from_bytes(b"x")),
    ]);
    for ty in [None, Some(U32)] {
        assert_eq!(test.keys(exists("f", ty)).unwrap(), [key(1)], "{ty:?}");
        assert_eq!(test.count(exists("f", ty)).unwrap(), 1, "{ty:?}");
    }
}

#[test]
fn exists_follows_patches_and_deletes() {
    let test = TestDb::default();
    test.create(
        (1..=5)
            .map(|n| record(key(n), &[("f", u32(n as u32))]))
            .chain([record(key(6), &[("g", u32(6))])]),
    );

    let budget = Budget::Limited(u64::MAX);
    let branch = test.db.begin().unwrap();
    for patch in [
        // `f` becomes a `str`.
        RecordOp::patch(key(2), budget).attribute("f", str("x")),
        // `f` becomes a field, which is not indexed.
        RecordOp::patch(key(3), budget).field("f", u32(3)),
        // `f` is removed.
        RecordOp::patch(key(4), budget).remove("f"),
        // `f` is added.
        RecordOp::patch(key(6), budget).attribute("f", CellValue::from_bool(true)),
    ] {
        test.db.patch(branch, patch).into_result().unwrap();
    }
    test.db
        .delete(branch, RecordOp::delete(key(5), budget))
        .into_result()
        .unwrap();
    test.db.commit(branch).unwrap();

    assert_eq!(test.keys(exists("f", Some(U32))).unwrap(), [key(1)]);
    assert_eq!(
        test.keys(exists("f", Some(CellType::Str))).unwrap(),
        [key(2)]
    );
    assert_eq!(
        test.keys(exists("f", Some(CellType::Bool))).unwrap(),
        [key(6)]
    );
    assert_eq!(
        test.keys(exists("f", None)).unwrap(),
        [key(1), key(2), key(6)]
    );
}

#[test]
fn typed_types_match_as_their_runtime_form_does() {
    let test = TestDb::default();
    test.create([
        record(key(1), &[("f", CellValue::from_dec32_unscaled(5))]),
        record(key(2), &[("f", CellValue::from_i32(5))]),
        record(key(3), &[("f", CellValue::from_date32(5))]),
    ]);
    let typed = |ty| Filter::Exists {
        field: CellNameRef::parse_user(b"f", 32)
            .expect("valid name")
            .into(),
        ty: Some(ty),
    };
    // Three types whose cells share a Rust value: the marker decides which
    // matches.
    assert_eq!(test.keys(typed(ExistsType::of(Dec32))).unwrap(), [key(1)]);
    assert_eq!(test.keys(typed(ExistsType::of(I32))).unwrap(), [key(2)]);
    assert_eq!(test.keys(typed(ExistsType::of(Date32))).unwrap(), [key(3)]);
    let runtime = exists("f", Some(CellType::Decimal(Width::W4)));
    assert_eq!(runtime, typed(ExistsType::of(Dec32)));
}

#[test]
fn an_invalid_field_is_an_error_not_an_empty_result() {
    let test = TestDb::default();
    for ty in [None, Some(CellType::Str)] {
        let result = test.keys(exists("not valid", ty));
        let Err(ApiError::InvalidArgument {
            source: Some(source),
            ..
        }) = &result
        else {
            panic!("expected InvalidArgument, got {result:?}");
        };
        assert_eq!(
            source.downcast_ref::<TermError>(),
            Some(&TermError::InvalidName),
            "{ty:?}"
        );
    }
}

const FIELDS: [&str; 3] = ["a", "ab", "b"];

fn field() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&FIELDS[..])
}

/// A record's cells, all attributes, by name.
type Cells = BTreeMap<&'static str, CellValue>;

/// Records, and `Exists` filters over them: of no type, or of one type,
/// usually a type the records hold, so that typed filters often match.
fn records_and_filters()
-> impl Strategy<Value = (Vec<Cells>, Vec<(&'static str, Option<CellType>)>)> {
    prop::collection::vec(
        prop::collection::btree_map(field(), small_value(), 0..4),
        0..24,
    )
    .prop_flat_map(|records| {
        let held: Vec<CellType> = records
            .iter()
            .flat_map(|cells| cells.values().map(CellValue::cell_type))
            .collect();
        let ty = if held.is_empty() {
            indexable_type().boxed()
        } else {
            prop_oneof![3 => prop::sample::select(held), 1 => indexable_type()].boxed()
        };
        let filters = prop::collection::vec((field(), prop::option::of(ty)), 1..12);
        (Just(records), filters)
    })
}

proptest! {
    // Each case builds and commits a whole database.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn exists_matches_a_model((records, filters) in records_and_filters()) {
        let test = TestDb::default();
        test.create(records.iter().enumerate().map(|(n, cells)| {
            let cells: Vec<_> = cells.iter().map(|(field, value)| (*field, value.clone())).collect();
            record(key(n as u64), &cells)
        }));
        for (field, ty) in filters {
            // A record matches iff it holds `field`, of type `ty` when one is
            // given. Records are created in order, so record IDs, and with
            // them the query order, follow `n`.
            let expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(_, cells)| {
                    cells
                        .get(field)
                        .is_some_and(|cell| ty.is_none_or(|ty| cell.cell_type() == ty))
                })
                .map(|(n, _)| key(n as u64))
                .collect();
            let filter = exists(field, ty);
            prop_assert_eq!(test.count(filter.clone()).unwrap(), expected.len() as u64);
            prop_assert_eq!(test.keys(filter).unwrap(), expected);
        }
    }
}
