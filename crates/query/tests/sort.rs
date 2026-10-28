//! Sorted queries against a real database: the order sort keys give, page
//! by page, and the sort keys admission rejects.

mod support;

use std::error::Error;

use golemdb_api::{
    Api, ApiError, Budget, Direction, Filter, QueryOp, QueryPage, RecordKey, RecordOp, SortKey,
    marker::{F64, I32, Str, U32},
    op,
};
use golemdb_cells::{CellType, CellValue};
use golemdb_index::TermError;
use proptest::prelude::*;
use support::{TestDb, eq, key, native_cmp, small_value, small_values_of_one_type};

const BUDGET: Budget = Budget::Limited(u64::MAX);

/// A record [`everything`] matches.
fn tagged(key: RecordKey) -> RecordOp<op::Create> {
    RecordOp::create(key, BUDGET).attribute("tag", CellValue::from_bool(true))
}

fn everything() -> Filter {
    eq("tag", CellValue::from_bool(true))
}

fn u32(n: u32) -> CellValue {
    CellValue::from_u32(n)
}

fn keys(page: QueryPage) -> Vec<RecordKey> {
    page.records.into_iter().map(|record| record.key).collect()
}

/// The keys of the records `filter` matches, in the order `sort` gives.
fn try_sorted(
    test: &TestDb,
    filter: Filter,
    sort: impl IntoIterator<Item = SortKey>,
) -> Result<Vec<RecordKey>, ApiError> {
    let op = QueryOp::from_filter(filter, BUDGET).sort(sort);
    test.db.query(None, op).into_result().map(keys)
}

/// The keys of every record, in the order `sort` gives.
fn sorted(test: &TestDb, sort: impl IntoIterator<Item = SortKey>) -> Vec<RecordKey> {
    try_sorted(test, everything(), sort).expect("a valid query")
}

/// The page of every record `sort` gives past `offset`, at most `limit`,
/// with the match count.
fn page(
    test: &TestDb,
    sort: impl IntoIterator<Item = SortKey>,
    offset: u64,
    limit: u64,
) -> QueryPage {
    let op = QueryOp::from_filter(everything(), BUDGET)
        .sort(sort)
        .offset(offset)
        .limit(limit)
        .with_total();
    test.db
        .query(None, op)
        .into_result()
        .expect("a valid query")
}

#[test]
fn absent_values_come_first_ascending_and_last_descending() {
    let test = TestDb::default();
    // 2 has no `n`, and 4's is a `str`: both are absent to a `u32` key.
    test.create([
        tagged(key(1)).attribute("n", u32(2)),
        tagged(key(2)),
        tagged(key(3)).attribute("n", u32(1)),
        tagged(key(4)).attribute("n", CellValue::from_str("x")),
        tagged(key(5)).attribute("n", u32(3)),
    ]);
    assert_eq!(
        sorted(&test, [SortKey::asc("n", U32)]),
        [2, 4, 3, 1, 5].map(key)
    );
    assert_eq!(
        sorted(&test, [SortKey::desc("n", U32)]),
        [5, 1, 3, 2, 4].map(key)
    );
}

#[test]
fn ties_come_in_creation_order_in_either_direction() {
    let test = TestDb::default();
    // Created in this order, which is record ID order, not key order.
    test.create(
        [(5, 1), (1, 2), (4, 1), (2, 2), (3, 1)]
            .map(|(n, size)| tagged(key(n)).attribute("size", u32(size))),
    );
    assert_eq!(
        sorted(&test, [SortKey::asc("size", U32)]),
        [5, 4, 3, 1, 2].map(key)
    );
    assert_eq!(
        sorted(&test, [SortKey::desc("size", U32)]),
        [1, 2, 5, 4, 3].map(key)
    );
}

#[test]
fn a_field_sorts_among_attributes_of_its_type() {
    let test = TestDb::default();
    test.create([
        tagged(key(1)).attribute("n", u32(2)),
        tagged(key(2)).field("n", u32(1)),
        tagged(key(3)).field("n", u32(3)),
        // A field of another type is absent, as an attribute would be.
        tagged(key(4)).field("n", CellValue::from_str("x")),
    ]);
    assert_eq!(
        sorted(&test, [SortKey::asc("n", U32)]),
        [4, 2, 1, 3].map(key)
    );
    assert_eq!(
        sorted(&test, [SortKey::desc("n", U32)]),
        [3, 1, 2, 4].map(key)
    );
}

#[test]
fn values_come_in_their_types_order() {
    let test = TestDb::default();
    let f64 = |value| CellValue::from_f64(value).expect("not NaN");
    test.create(
        [
            (1, 1, "b", 1.5),
            (2, i32::MIN, "ab", f64::NEG_INFINITY),
            (3, -1, "", f64::INFINITY),
            (4, 0, "a", -0.0),
            (5, i32::MAX, "é", -1.5),
        ]
        .map(|(n, i, s, f)| {
            tagged(key(n))
                .attribute("i", CellValue::from_i32(i))
                .attribute("s", CellValue::from_str(s))
                .attribute("f", f64(f))
        }),
    );
    // Numeric order: negative values first.
    assert_eq!(
        sorted(&test, [SortKey::asc("i", I32)]),
        [2, 3, 4, 1, 5].map(key)
    );
    // UTF-8 byte order: a prefix first, and "é" after every ASCII letter.
    assert_eq!(
        sorted(&test, [SortKey::asc("s", Str)]),
        [3, 4, 2, 1, 5].map(key)
    );
    // IEEE order, the infinities at the ends.
    assert_eq!(
        sorted(&test, [SortKey::asc("f", F64)]),
        [2, 5, 4, 1, 3].map(key)
    );
}

#[test]
fn pages_follow_one_another_through_the_sorted_matches() {
    let test = TestDb::default();
    // A tie on 3, and 3 has no size.
    let sizes = [Some(5), Some(3), None, Some(3), Some(9), Some(1), Some(7)];
    test.create((1..).zip(sizes).map(|(n, size)| match size {
        Some(size) => tagged(key(n)).attribute("size", u32(size)),
        None => tagged(key(n)),
    }));
    let by_size = || [SortKey::desc("size", U32)];
    let all = sorted(&test, by_size());
    assert_eq!(all, [5, 7, 1, 2, 4, 6, 3].map(key));
    let mut paged = Vec::new();
    for offset in [0, 3, 6] {
        let page = page(&test, by_size(), offset, 3);
        assert_eq!(page.total_matched, Some(7), "{offset}");
        paged.extend(keys(page));
    }
    assert_eq!(paged, all);
    let past_the_end = page(&test, by_size(), 7, 3);
    assert!(past_the_end.records.is_empty());
    assert_eq!(past_the_end.total_matched, Some(7));
}

/// The example of design §12: six records with a `status` and an `amount`.
fn statuses_and_amounts(test: &TestDb) {
    test.create(
        [
            (1, "active", 100),
            (2, "closed", 50),
            (3, "active", 300),
            (4, "active", 100),
            (5, "closed", 70),
            (6, "pending", 10),
        ]
        .map(|(n, status, amount)| {
            tagged(key(n))
                .attribute("status", CellValue::from_str(status))
                .attribute("amount", u32(amount))
        }),
    );
}

#[test]
fn each_key_orders_the_records_the_ones_before_it_tie() {
    let test = TestDb::default();
    statuses_and_amounts(&test);
    // 1 and 4 tie on both keys: creation order, in either direction.
    assert_eq!(
        sorted(
            &test,
            [SortKey::asc("status", Str), SortKey::desc("amount", U32)]
        ),
        [3, 1, 4, 5, 2, 6].map(key)
    );
    assert_eq!(
        sorted(
            &test,
            [SortKey::desc("status", Str), SortKey::asc("amount", U32)]
        ),
        [6, 2, 5, 1, 4, 3].map(key)
    );
}

#[test]
fn pages_of_a_multi_key_sort_follow_one_another() {
    let test = TestDb::default();
    statuses_and_amounts(&test);
    let by_status_then_amount = || [SortKey::asc("status", Str), SortKey::desc("amount", U32)];
    let all = sorted(&test, by_status_then_amount());
    // Pages of every size up to four, which start or end inside the ties.
    for limit in 1..=4 {
        let paged: Vec<_> = (0..6)
            .filter(|offset| offset % limit == 0)
            .flat_map(|offset| keys(page(&test, by_status_then_amount(), offset, limit)))
            .collect();
        assert_eq!(paged, all, "{limit}");
    }
}

#[test]
fn more_sort_keys_than_the_cap_exceed_it() {
    let test = TestDb::default();
    statuses_and_amounts(&test);
    // `Database` sorts by four keys at most.
    let by_status = || SortKey::asc("status", Str);
    assert_eq!(
        sorted(&test, vec![by_status(); 4]),
        [1, 3, 4, 2, 5, 6].map(key)
    );
    let error = try_sorted(&test, everything(), vec![by_status(); 5]).expect_err("five keys");
    assert!(
        matches!(
            error,
            ApiError::LimitExceeded {
                limit: "sort key count",
                max: 4
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_sort_key_with_a_bad_name_is_an_invalid_argument() {
    let test = TestDb::default();
    test.create([tagged(key(1))]);
    for name in ["1bad", "a b", "", "#key"] {
        let error = try_sorted(&test, everything(), [SortKey::asc(name, U32)])
            .expect_err("a bad sort key name");
        assert!(
            matches!(error, ApiError::InvalidArgument { .. }),
            "{name:?}: {error:?}"
        );
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&TermError::InvalidName),
            "{name:?}"
        );
    }
}

#[test]
fn a_bad_filter_is_reported_before_a_bad_sort() {
    let test = TestDb::default();
    let bad_filter = || eq("1filter", u32(1));
    for sort in [
        vec![SortKey::asc("1sort", U32)],
        vec![SortKey::asc("n", U32); 5],
    ] {
        let error = try_sorted(&test, bad_filter(), sort).expect_err("a bad filter");
        assert!(
            matches!(&error, ApiError::InvalidArgument { message, .. } if message.contains("1filter")),
            "{error:?}"
        );
    }
}

/// A record of the reference check: its cell `n`, if any, whether that cell
/// is a field, and whether the filter matches the record.
type Spec = (Option<CellValue>, bool, bool);

/// The sort key's type, and records for the reference check, created in
/// order. Half the cells are of the key's type, from a tiny domain so ties
/// are common; the others are absent or of a random type, rarely the key's.
fn records() -> impl Strategy<Value = (CellType, Vec<Spec>)> {
    small_values_of_one_type(1..12).prop_flat_map(|values| {
        let ty = values.first().expect("one value at least").cell_type();
        let specs = prop::collection::vec(
            (0u8..4, small_value(), any::<bool>(), any::<bool>()),
            values.len(),
        )
        .prop_map(move |choices| {
            values
                .iter()
                .zip(choices)
                .map(|(value, (choice, other, is_field, matched))| {
                    let cell = match choice {
                        0 => None,
                        1 => Some(other),
                        _ => Some(value.clone()),
                    };
                    (cell, is_field, matched)
                })
                .collect()
        });
        (Just(ty), specs)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The page is the reference's: the matched records sorted by their
    /// `n` of the key's type, compared as native values, absent below every
    /// value, reversed for descending, ties in creation order; then cut.
    #[test]
    fn a_sorted_page_is_the_reference_page(
        (ty, records) in records(),
        descending in any::<bool>(),
        offset in 0u64..14,
        limit in 1u64..14,
    ) {
        let test = TestDb::default();
        test.create((1..).zip(&records).map(|(n, (cell, is_field, matched))| {
            let record = RecordOp::create(key(n), BUDGET)
                .attribute("tag", CellValue::from_bool(*matched));
            match cell {
                None => record,
                Some(value) if *is_field => record.field("n", value.clone()),
                Some(value) => record.attribute("n", value.clone()),
            }
        }));
        let mut expected: Vec<(u64, Option<&CellValue>)> = (1..)
            .zip(&records)
            .filter(|(_, (_, _, matched))| *matched)
            .map(|(n, (cell, _, _))| (n, cell.as_ref().filter(|value| value.cell_type() == ty)))
            .collect();
        expected.sort_by(|(a_n, a), (b_n, b)| {
            let by_value = match (a, b) {
                (Some(a), Some(b)) => native_cmp(a, b).expect("values of one type"),
                (a, b) => a.is_some().cmp(&b.is_some()),
            };
            if descending { by_value.reverse() } else { by_value }.then(a_n.cmp(b_n))
        });
        let expected: Vec<RecordKey> = expected
            .into_iter()
            .skip(usize::try_from(offset).expect("small"))
            .take(usize::try_from(limit).expect("small"))
            .map(|(n, _)| key(n))
            .collect();
        let direction = if descending { Direction::Desc } else { Direction::Asc };
        let sort = SortKey::try_new("n", ty, direction).expect("an indexable type");
        prop_assert_eq!(keys(page(&test, [sort], offset, limit)), expected);
    }
}
