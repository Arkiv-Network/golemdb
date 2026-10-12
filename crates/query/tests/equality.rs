mod support;

use golemdb_cells::{CellNameRef, CellType, CellValue};
use golemdb_index::{IndexTerm, TermError};
use golemdb_query::{Filter, QueryError};
use proptest::prelude::*;
use support::{TestIndex, add, any_value, remove, small_value, written_term};

/// Raw, so tests can also build names the grammar rejects.
fn eq(field: &str, value: CellValue) -> Filter {
    Filter::Eq {
        field: CellNameRef::raw(field.as_bytes()).into(),
        value,
    }
}

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

/// Record IDs spread over many 48-bit regions, so containers differ.
fn id(i: usize) -> u64 {
    i as u64 * 70_001
}

#[test]
fn every_type_round_trips_through_the_index() {
    let samples = samples();
    let mut index = TestIndex::default();
    index.apply(
        samples
            .iter()
            .enumerate()
            .map(|(i, value)| add(id(i), "f", value)),
    );
    for (i, value) in samples.iter().enumerate() {
        assert_eq!(
            index.evaluate(&eq("f", value.clone())).unwrap(),
            [id(i)],
            "{value:?}"
        );
    }
}

#[test]
fn values_never_stored_match_nothing() {
    let mut index = TestIndex::default();
    index.apply(samples().iter().map(|value| add(1, "f", value)));
    for value in [
        CellValue::from_str("b"),
        CellValue::from_u32(1),
        CellValue::from_i32(1),
        CellValue::from_dec32_unscaled(-1),
        f64_value(1.0),
        CellValue::from_timestamp64(0),
    ] {
        assert!(
            index.evaluate(&eq("f", value.clone())).unwrap().is_empty(),
            "{value:?}"
        );
    }
    assert!(
        index
            .evaluate(&eq("g", CellValue::from_bool(true)))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn postings_span_container_boundaries() {
    let ids = [
        0,
        1,
        65_535,
        65_536,
        65_537,
        u64::from(u32::MAX),
        1 << 32,
        (1 << 48) - 1,
        1 << 48,
        u64::MAX - 1,
        u64::MAX,
    ];
    let blue = CellValue::from_str("blue");
    let red = CellValue::from_str("red");
    let mut index = TestIndex::default();
    index.apply(ids.iter().map(|&id| add(id, "color", &blue)));
    index.apply(ids.iter().map(|&id| add(id ^ 1, "color", &red)));
    assert_eq!(index.evaluate(&eq("color", blue)).unwrap(), ids);
    let mut reds: Vec<u64> = ids.iter().map(|id| id ^ 1).collect();
    reds.sort_unstable();
    assert_eq!(index.evaluate(&eq("color", red)).unwrap(), reds);
}

#[test]
fn removed_postings_stop_matching() {
    let blue = CellValue::from_str("blue");
    let mut index = TestIndex::default();
    index.apply([1, 2, 3, 70_000].map(|id| add(id, "color", &blue)));
    index.apply([remove(2, "color", &blue)]);
    assert_eq!(
        index.evaluate(&eq("color", blue.clone())).unwrap(),
        [1, 3, 70_000]
    );

    // The last posting going removes the term itself.
    index.apply([1, 3, 70_000].map(|id| remove(id, "color", &blue)));
    assert!(
        index
            .evaluate(&eq("color", blue.clone()))
            .unwrap()
            .is_empty()
    );

    index.apply([add(9, "color", &blue)]);
    assert_eq!(index.evaluate(&eq("color", blue)).unwrap(), [9]);
}

#[test]
fn one_record_under_many_terms() {
    let mut index = TestIndex::default();
    index.apply([
        add(9, "a", &CellValue::from_i32(1)),
        add(9, "ab", &CellValue::from_i32(1)),
        add(9, "b", &CellValue::from_u32(1)),
        add(10, "a", &CellValue::from_i32(2)),
    ]);
    assert_eq!(
        index.evaluate(&eq("a", CellValue::from_i32(1))).unwrap(),
        [9]
    );
    assert_eq!(
        index.evaluate(&eq("ab", CellValue::from_i32(1))).unwrap(),
        [9]
    );
    assert_eq!(
        index.evaluate(&eq("b", CellValue::from_u32(1))).unwrap(),
        [9]
    );
    assert!(
        index
            .evaluate(&eq("b", CellValue::from_i32(1)))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        index.evaluate(&eq("a", CellValue::from_i32(2))).unwrap(),
        [10]
    );
}

#[test]
fn invalid_arguments_are_errors_not_empty_results() {
    let index = TestIndex::default();
    assert!(matches!(
        index.evaluate(&eq("not valid", CellValue::from_i32(1))),
        Err(QueryError::InvalidArgument {
            source: TermError::InvalidName,
            ..
        })
    ));
    assert!(matches!(
        index.evaluate(&eq("f", CellValue::from_bytes(b"x"))),
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

/// IDs clustered around container boundaries and spread far apart.
fn record_id() -> impl Strategy<Value = u64> {
    prop_oneof![0u64..64, 65_520u64..65_552, any::<u64>()]
}

proptest! {
    #[test]
    fn lookup_terms_equal_written_terms(field in "[a-zA-Z][a-zA-Z0-9_.:-]{0,15}", value in any_value()) {
        // Filters hold fields; seal indexes attributes. The kind must not matter.
        let lookup = IndexTerm::for_value(&field, value.as_view()).unwrap();
        prop_assert_eq!(lookup, written_term(&field, &value));
    }

    #[test]
    fn equality_matches_a_model(
        records in prop::collection::btree_map(
            record_id(),
            prop::collection::btree_map(field(), small_value(), 0..4),
            0..40,
        ),
        queries in prop::collection::vec(
            (field(), small_value(), any::<Option<prop::sample::Index>>()),
            1..16,
        ),
    ) {
        let mut index = TestIndex::default();
        index.apply(records.iter().flat_map(|(&id, cells)| {
            cells.iter().map(move |(field, value)| add(id, field, value))
        }));
        let stored: Vec<(&str, &CellValue)> = records
            .values()
            .flat_map(|cells| cells.iter().map(|(field, value)| (*field, value)))
            .collect();
        for (field, value, pick) in queries {
            // Mostly ask for cells that exist, so matches are not rare.
            let (field, value) = match pick {
                Some(pick) if !stored.is_empty() => {
                    let (field, value) = stored[pick.index(stored.len())];
                    (field, value.clone())
                }
                Some(_) | None => (field, value),
            };
            // A record matches iff its cell under `field` has the leaf's type
            // and value. Every generated value is a field, so `CellValue`
            // equality compares exactly type and stored bytes (-0 is already 0).
            let expected: Vec<u64> = records
                .iter()
                .filter(|(_, cells)| cells.get(field) == Some(&value))
                .map(|(&id, _)| id)
                .collect();
            prop_assert_eq!(index.evaluate(&eq(field, value)).unwrap(), expected);
        }
    }
}
