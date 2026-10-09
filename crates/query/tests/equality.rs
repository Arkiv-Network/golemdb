mod support;

use golemdb_index::TermError;
use golemdb_query::{Predicate, QueryError, Value};
use proptest::prelude::*;
use support::{Store, add, any_value, remove, small_value, written_term};

fn eq(field: &str, value: Value) -> Predicate {
    Predicate::eq(field, value)
}

/// Boundary values of every type. All distinct, so each has its own term.
fn samples() -> Vec<Value> {
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
        Value::Bool(false),
        Value::Bool(true),
        Value::Str(String::new()),
        Value::Str("a".into()),
        Value::Str("é𝄞\u{10FFFF}".into()),
        Value::Str("long".repeat(256)),
        Value::Bytes4([0; 4]),
        Value::Bytes4([0xff; 4]),
        Value::Bytes8([0xff; 8]),
        Value::Bytes16([0xff; 16]),
        Value::Bytes20([0; 20]),
        Value::Bytes20([0xff; 20]),
        Value::Bytes32([0; 32]),
        Value::U32(0),
        Value::U32(u32::MAX),
        Value::U64(0),
        Value::U64(u64::MAX),
        Value::U128(u128::MAX),
        Value::U256([0; 32]),
        Value::U256([0xff; 32]),
        Value::I32(i32::MIN),
        Value::I32(-1),
        Value::I32(0),
        Value::I32(i32::MAX),
        Value::I64(i64::MIN),
        Value::I64(i64::MAX),
        Value::I128(i128::MIN),
        Value::I128(i128::MAX),
        Value::I256(i256_min),
        Value::I256([0xff; 32]),
        Value::I256(i256_max),
        Value::Dec32(i32::MIN),
        Value::Dec64(-1),
        Value::Dec128(i128::MAX),
        Value::Dec256(i256_min),
        Value::F32(f32::NEG_INFINITY),
        Value::F32(-f32::from_bits(1)),
        Value::F32(0.0),
        Value::F32(f32::MAX),
        Value::F32(f32::INFINITY),
        Value::F64(f64::NEG_INFINITY),
        Value::F64(f64::MIN),
        Value::F64(0.0),
        Value::F64(f64::from_bits(1)),
        Value::F64(f64::INFINITY),
        Value::Date32(i32::MIN),
        Value::Date32(0),
        Value::Date32(i32::MAX),
        Value::Timestamp64(i64::MIN),
        Value::Timestamp64(i64::MAX),
    ]
}

/// Record IDs spread over many 48-bit regions, so containers differ.
fn id(i: usize) -> u64 {
    i as u64 * 70_001
}

#[test]
fn every_type_round_trips_through_the_index() {
    let samples = samples();
    let mut store = Store::default();
    store.apply(
        samples
            .iter()
            .enumerate()
            .map(|(i, value)| add(id(i), "f", value)),
    );
    for (i, value) in samples.iter().enumerate() {
        assert_eq!(
            store.query(eq("f", value.clone())).unwrap(),
            [id(i)],
            "{value:?}"
        );
    }
}

#[test]
fn values_never_stored_match_nothing() {
    let mut store = Store::default();
    store.apply(samples().iter().map(|value| add(1, "f", value)));
    for value in [
        Value::Str("b".into()),
        Value::U32(1),
        Value::I32(1),
        Value::Dec32(-1),
        Value::F64(1.0),
        Value::Timestamp64(0),
    ] {
        assert!(
            store.query(eq("f", value.clone())).unwrap().is_empty(),
            "{value:?}"
        );
    }
    assert!(store.query(eq("g", Value::Bool(true))).unwrap().is_empty());
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
    let blue = Value::from("blue");
    let red = Value::from("red");
    let mut store = Store::default();
    store.apply(ids.iter().map(|&id| add(id, "color", &blue)));
    store.apply(ids.iter().map(|&id| add(id ^ 1, "color", &red)));
    assert_eq!(store.query(eq("color", blue)).unwrap(), ids);
    let mut reds: Vec<u64> = ids.iter().map(|id| id ^ 1).collect();
    reds.sort_unstable();
    assert_eq!(store.query(eq("color", red)).unwrap(), reds);
}

#[test]
fn removed_postings_stop_matching() {
    let blue = Value::from("blue");
    let mut store = Store::default();
    store.apply([1, 2, 3, 70_000].map(|id| add(id, "color", &blue)));
    store.apply([remove(2, "color", &blue)]);
    assert_eq!(
        store.query(eq("color", blue.clone())).unwrap(),
        [1, 3, 70_000]
    );

    // The last posting going removes the term itself.
    store.apply([1, 3, 70_000].map(|id| remove(id, "color", &blue)));
    assert!(store.query(eq("color", blue.clone())).unwrap().is_empty());

    store.apply([add(9, "color", &blue)]);
    assert_eq!(store.query(eq("color", blue)).unwrap(), [9]);
}

#[test]
fn one_record_under_many_terms() {
    let mut store = Store::default();
    store.apply([
        add(9, "a", &Value::I32(1)),
        add(9, "ab", &Value::I32(1)),
        add(9, "b", &Value::U32(1)),
        add(10, "a", &Value::I32(2)),
    ]);
    assert_eq!(store.query(eq("a", Value::I32(1))).unwrap(), [9]);
    assert_eq!(store.query(eq("ab", Value::I32(1))).unwrap(), [9]);
    assert_eq!(store.query(eq("b", Value::U32(1))).unwrap(), [9]);
    assert!(store.query(eq("b", Value::I32(1))).unwrap().is_empty());
    assert_eq!(store.query(eq("a", Value::I32(2))).unwrap(), [10]);
}

#[test]
fn invalid_predicates_are_errors_not_empty_results() {
    let store = Store::default();
    assert!(matches!(
        store.query(eq("not valid", Value::I32(1))),
        Err(QueryError::InvalidPredicate {
            source: TermError::InvalidName,
            ..
        })
    ));
    assert!(matches!(
        store.query(eq("f", Value::F32(f32::NAN))),
        Err(QueryError::InvalidPredicate {
            source: TermError::NaN,
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
    fn query_terms_equal_written_terms(field in "[a-zA-Z][a-zA-Z0-9_.:-]{0,15}", value in any_value()) {
        prop_assert_eq!(value.term(&field).unwrap(), written_term(&field, &value));
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
        let mut store = Store::default();
        store.apply(records.iter().flat_map(|(&id, cells)| {
            cells.iter().map(move |(field, value)| add(id, field, value))
        }));
        let stored: Vec<(&str, &Value)> = records
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
            // A record matches iff its cell under `field` has the query's type
            // and value; `Value`'s equality already compares both (and -0 == 0).
            let expected: Vec<u64> = records
                .iter()
                .filter(|(_, cells)| cells.get(field) == Some(&value))
                .map(|(&id, _)| id)
                .collect();
            prop_assert_eq!(store.query(eq(field, value)).unwrap(), expected);
        }
    }
}
