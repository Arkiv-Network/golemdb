//! Filter semantics against a real database: which records `And` and `Or`
//! trees match, over every kind of leaf, and which trees admission rejects.

mod support;

use std::{collections::BTreeMap, ops::Bound};

use golemdb_api::{ApiError, CellType, Filter, RecordKey};
use golemdb_cells::CellValue;
use golemdb_index::TermError;
use proptest::prelude::*;
use support::{
    TestDb, eq, is_between, is_in, key, prefix, range, range_of, record, small_bounds_of_one_type,
    small_value, small_values_of_one_type,
};

fn str(value: &str) -> CellValue {
    CellValue::from_str(value)
}

fn u32(n: u32) -> CellValue {
    CellValue::from_u32(n)
}

/// People with a name, an age and a color; one has no color.
fn people() -> TestDb {
    let test = TestDb::default();
    test.create([
        record(
            key(1),
            &[
                ("name", str("alice")),
                ("age", u32(30)),
                ("color", str("red")),
            ],
        ),
        record(
            key(2),
            &[
                ("name", str("bob")),
                ("age", u32(17)),
                ("color", str("blue")),
            ],
        ),
        record(
            key(3),
            &[
                ("name", str("anna")),
                ("age", u32(45)),
                ("color", str("blue")),
            ],
        ),
        record(key(4), &[("name", str("carl")), ("age", u32(30))]),
    ]);
    test
}

#[test]
fn and_matches_the_records_every_child_matches() {
    let test = people();
    let filter = Filter::And(vec![eq("color", str("blue")), eq("age", u32(45))]);
    assert_eq!(test.keys(filter.clone()).unwrap(), [key(3)]);
    assert_eq!(test.count(filter).unwrap(), 1);
    let filter = Filter::And(vec![eq("age", u32(30))]);
    assert_eq!(test.keys(filter).unwrap(), [key(1), key(4)]);
    let filter = Filter::And(vec![eq("color", str("red")), eq("color", str("blue"))]);
    assert!(test.keys(filter).unwrap().is_empty());
}

#[test]
fn or_matches_the_records_some_child_matches() {
    let test = people();
    let filter = Filter::Or(vec![eq("color", str("red")), eq("age", u32(17))]);
    assert_eq!(test.keys(filter.clone()).unwrap(), [key(1), key(2)]);
    assert_eq!(test.count(filter).unwrap(), 2);
    let filter = Filter::Or(vec![eq("color", str("green")), eq("age", u32(99))]);
    assert!(test.keys(filter).unwrap().is_empty());
}

#[test]
fn every_kind_of_leaf_combines() {
    let test = people();
    // Names starting with "a", adults, and either blue or red.
    let filter = Filter::And(vec![
        prefix("name", "a"),
        range("age", Bound::Included(u32(18)), Bound::Unbounded),
        Filter::Or(vec![
            eq("color", str("blue")),
            is_in("color", [str("red"), str("green")]),
        ]),
    ]);
    assert_eq!(test.keys(filter).unwrap(), [key(1), key(3)]);
    // Aged 30 with a name starting with "c", or named "bob".
    let filter = Filter::Or(vec![
        Filter::And(vec![eq("age", u32(30)), prefix("name", "c")]),
        eq("name", str("bob")),
    ]);
    assert_eq!(test.keys(filter).unwrap(), [key(2), key(4)]);
}

#[test]
fn a_node_with_no_child_is_an_invalid_query() {
    let test = people();
    for filter in [
        Filter::And(vec![]),
        Filter::Or(vec![]),
        Filter::And(vec![eq("age", u32(30)), Filter::Or(vec![])]),
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
fn an_invalid_leaf_anywhere_is_an_error_not_an_empty_result() {
    let test = people();
    let cause = |result: Result<Vec<RecordKey>, ApiError>| match result {
        Err(ApiError::InvalidArgument {
            source: Some(source),
            ..
        }) => source.downcast_ref::<TermError>().copied(),
        _ => None,
    };
    // Even under an `And` whose first child matches nothing.
    let filter = Filter::And(vec![
        eq("age", u32(99)),
        Filter::Or(vec![eq("color", str("red")), prefix("not valid", "a")]),
    ]);
    assert_eq!(cause(test.keys(filter)), Some(TermError::InvalidName));
}

/// `Database`'s cap on `And` / `Or` nesting, hardcoded there until it is read
/// from genesis.
const MAX_QUERY_DEPTH: u32 = 64;

/// `levels` `And` / `Or` levels over "aged 30", in the shape that takes the
/// most stack per level: each `Or` nests its only child, each `And` its last.
fn aged_30_nested(levels: u32) -> Filter {
    (0..levels).fold(eq("age", u32(30)), |filter, level| {
        if level % 2 == 0 {
            Filter::Or(vec![filter])
        } else {
            Filter::And(vec![eq("age", u32(30)), filter])
        }
    })
}

#[test]
fn a_filter_nests_up_to_the_depth_cap() {
    let test = people();
    // Admitted and evaluated at the cap, on a test thread's stack.
    let at_the_cap = aged_30_nested(MAX_QUERY_DEPTH);
    assert_eq!(test.keys(at_the_cap.clone()).unwrap(), [key(1), key(4)]);
    assert_eq!(test.count(at_the_cap).unwrap(), 2);
    let over_the_cap = aged_30_nested(MAX_QUERY_DEPTH + 1);
    assert!(matches!(
        test.keys(over_the_cap.clone()),
        Err(ApiError::LimitExceeded {
            limit: "filter depth",
            max: MAX_QUERY_DEPTH
        })
    ));
    assert!(matches!(
        test.count(over_the_cap),
        Err(ApiError::LimitExceeded {
            limit: "filter depth",
            max: MAX_QUERY_DEPTH
        })
    ));
}

const FIELDS: [&str; 3] = ["a", "ab", "b"];

fn field() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&FIELDS[..])
}

/// A filter tree, with a model of which cells it matches.
#[derive(Debug, Clone)]
enum Tree {
    Eq(&'static str, CellValue),
    In(&'static str, Vec<CellValue>),
    Range(&'static str, CellType, Bound<CellValue>, Bound<CellValue>),
    Prefix(&'static str, &'static str),
    And(Vec<Tree>),
    Or(Vec<Tree>),
}

impl Tree {
    fn filter(&self) -> Filter {
        match self {
            Self::Eq(field, value) => eq(field, value.clone()),
            Self::In(field, values) => is_in(field, values.clone()),
            Self::Range(field, ty, lower, upper) => {
                range_of(field, *ty, lower.clone(), upper.clone())
            }
            Self::Prefix(field, start) => prefix(field, start),
            Self::And(children) => Filter::And(children.iter().map(Self::filter).collect()),
            Self::Or(children) => Filter::Or(children.iter().map(Self::filter).collect()),
        }
    }

    /// Whether a record with `cells` matches, by the leaves' definitions,
    /// with no index code.
    fn matches(&self, cells: &BTreeMap<&str, CellValue>) -> bool {
        match self {
            Self::Eq(field, value) => cells.get(field) == Some(value),
            Self::In(field, values) => cells.get(field).is_some_and(|cell| values.contains(cell)),
            Self::Range(field, ty, lower, upper) => cells
                .get(field)
                .is_some_and(|cell| cell.cell_type() == *ty && is_between(cell, lower, upper)),
            Self::Prefix(field, start) => cells
                .get(field)
                .and_then(CellValue::as_str)
                .is_some_and(|value| value.starts_with(start)),
            Self::And(children) => children.iter().all(|child| child.matches(cells)),
            Self::Or(children) => children.iter().any(|child| child.matches(cells)),
        }
    }
}

/// A leaf of any kind, over [`small_value`]'s domain, so leaves and records
/// collide often.
fn leaf() -> impl Strategy<Value = Tree> {
    prop_oneof![
        (field(), small_value()).prop_map(|(field, value)| Tree::Eq(field, value)),
        (field(), small_values_of_one_type(0..4))
            .prop_map(|(field, values)| Tree::In(field, values)),
        (field(), small_bounds_of_one_type())
            .prop_map(|(field, (ty, lower, upper))| Tree::Range(field, ty, lower, upper)),
        (field(), prop::sample::select(&["", "a", "ab", "é"][..]))
            .prop_map(|(field, start)| Tree::Prefix(field, start)),
    ]
}

/// Trees up to three levels of `And` / `Or` deep, each node with one to
/// three children.
fn tree() -> impl Strategy<Value = Tree> {
    leaf().prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..4).prop_map(Tree::And),
            prop::collection::vec(inner, 1..4).prop_map(Tree::Or),
        ]
    })
}

proptest! {
    // Each case builds and commits a whole database.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn trees_match_a_model(
        records in prop::collection::vec(
            prop::collection::btree_map(field(), small_value(), 0..4),
            0..24,
        ),
        trees in prop::collection::vec(tree(), 1..8),
    ) {
        let test = TestDb::default();
        test.create(records.iter().enumerate().map(|(n, cells)| {
            let cells: Vec<_> = cells.iter().map(|(field, value)| (*field, value.clone())).collect();
            record(key(n as u64), &cells)
        }));
        for tree in trees {
            // Records are created in order, so record IDs, and with them the
            // query order, follow `n`.
            let expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(_, cells)| tree.matches(cells))
                .map(|(n, _)| key(n as u64))
                .collect();
            let filter = tree.filter();
            prop_assert_eq!(test.count(filter.clone()).unwrap(), expected.len() as u64);
            prop_assert_eq!(test.keys(filter).unwrap(), expected, "{:?}", tree);
        }
    }
}
