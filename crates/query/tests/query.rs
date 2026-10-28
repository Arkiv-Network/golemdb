//! The `query` and `count` calls: what they return, and what they reject.

mod support;

use std::ops::Bound;

use golemdb_api::{Api, CommitId, ReadTarget, RecordKey, RecordOp};
use golemdb_api::{ApiError, Budget, CountOp, QueryOp, QueryPage};
use golemdb_api::{CellLimits, Database, Genesis, HashAlgorithm, OpenConfig};
use golemdb_branch::{BranchError, Branches};
use golemdb_cells::system::FIRST_USER_RECORD_ID;
use golemdb_cells::{CellKey, CellValue, reserved};
use golemdb_merkle::Keccak256Hasher;
use golemdb_query::{FetchAndSort, Queries, QueryError};
use golemdb_record::RecordError;
use golemdb_storage::MemoryStore;
use support::{TestDb, eq, key, range, record};

fn blue() -> CellValue {
    CellValue::from_str("blue")
}

/// The configuration of a database opened over a caller's store.
fn config() -> OpenConfig {
    let limits = CellLimits {
        max_cell_name_len: 32,
        max_str_len: 64,
        max_bytes_len: 64,
    };
    OpenConfig::new(Genesis::new(
        HashAlgorithm::Keccak256,
        limits,
        golemdb_api::RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
    ))
}

/// A page's record keys, in page order, and its `total_matched`.
fn keys_and_total(page: QueryPage) -> (Vec<RecordKey>, Option<u64>) {
    let keys = page.records.into_iter().map(|record| record.key).collect();
    (keys, page.total_matched)
}

/// Two blue records with extra cells, one of them not indexed, and a red one.
fn colors() -> TestDb {
    let test = TestDb::default();
    test.create([
        record(
            key(1),
            &[("color", blue()), ("size", CellValue::from_u32(3))],
        )
        .field("note", CellValue::from_str("first")),
        record(key(2), &[("color", CellValue::from_str("red"))]),
        record(key(3), &[("color", blue())]),
    ]);
    test
}

#[test]
fn records_are_returned_as_get_returns_them() {
    let test = colors();
    let page = test
        .db
        .query(
            None,
            QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result()
        .unwrap();
    let get = |n| {
        test.db
            .get(
                ReadTarget::Head,
                RecordOp::get(key(n), golemdb_api::Budget::Limited(u64::MAX)),
            )
            .into_result()
            .unwrap()
    };
    assert_eq!(page.records, [get(1), get(3)]);
}

#[test]
fn records_come_in_creation_order() {
    let test = TestDb::default();
    test.create([9, 1, 5].map(|n| record(key(n), &[("color", blue())])));
    assert_eq!(
        test.keys(eq("color", blue())).unwrap(),
        [key(9), key(1), key(5)]
    );
}

#[test]
fn projection_selects_cells_as_get_does() {
    let test = colors();
    let op = QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX))
        .only(["size", "note", "missing"]);
    let page = test.db.query(None, op).into_result().unwrap();
    let get = |n| {
        let op = RecordOp::get(key(n), golemdb_api::Budget::Limited(u64::MAX))
            .only(["size", "note", "missing"]);
        test.db.get(ReadTarget::Head, op).into_result().unwrap()
    };
    assert_eq!(page.records, [get(1), get(3)]);

    let empty = QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX))
        .only(Vec::<&str>::new());
    let page = test.db.query(None, empty).into_result().unwrap();
    assert!(page.records.iter().all(|record| record.cells.is_empty()));
    assert_eq!(page.records.len(), 2);
}

#[test]
fn total_matched_only_when_asked() {
    let test = colors();
    let query = |op| test.db.query(None, op).into_result().unwrap();
    assert_eq!(
        query(QueryOp::from_filter(
            eq("color", blue()),
            golemdb_api::Budget::Limited(u64::MAX)
        ))
        .total_matched,
        None
    );
    let op = QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX))
        .with_total();
    assert_eq!(query(op).total_matched, Some(2));
}

#[test]
fn pages_report_the_commit_they_read() {
    let test = colors();
    test.create([record(key(4), &[("color", blue())])]);
    let head = test.db.head().unwrap();
    let page = test
        .db
        .query(
            None,
            QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result()
        .unwrap();
    assert_eq!(page.evaluated_at, head);
    assert_eq!(page.records.len(), 3);
}

#[test]
fn the_head_commit_can_be_named() {
    let test = colors();
    let head = test.db.head().unwrap();
    let op = QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX));
    let page = test.db.query(Some(head), op).into_result().unwrap();
    assert_eq!(page.evaluated_at, head);
    let count = test
        .db
        .count(
            Some(head),
            CountOp::filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result()
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn other_commits_are_unavailable() {
    let test = colors();
    let head = test.db.head().unwrap();
    let older = CommitId::new(head.get() - 1);
    let result = test
        .db
        .query(
            Some(older),
            QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result();
    assert!(matches!(
        result,
        Err(ApiError::CommitUnavailable { requested, head: got }) if requested == older && got == head
    ));
}

#[test]
fn pending_branch_work_is_invisible() {
    let test = colors();
    let branch = test.db.begin().unwrap();
    test.db
        .create(branch, record(key(7), &[("color", blue())]))
        .into_result()
        .unwrap();
    assert_eq!(test.keys(eq("color", blue())).unwrap(), [key(1), key(3)]);
    test.db.commit(branch).unwrap();
    assert_eq!(
        test.keys(eq("color", blue())).unwrap(),
        [key(1), key(3), key(7)]
    );
}

#[test]
fn the_budget_is_accepted_but_not_enforced_yet() {
    let test = colors();
    let op = QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX))
        .budget(Budget::Limited(0));
    let page = test.db.query(None, op).into_result().unwrap();
    assert_eq!(page.records.len(), 2);
    let op = CountOp::filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX))
        .budget(Budget::Limited(0));
    assert_eq!(test.db.count(None, op).into_result().unwrap(), 2);
}

#[test]
fn count_is_the_size_of_the_match_set() {
    let test = colors();
    assert_eq!(test.count(eq("color", blue())).unwrap(), 2);
    assert_eq!(
        test.count(eq("color", CellValue::from_str("red"))).unwrap(),
        1
    );
    assert_eq!(
        test.count(eq("color", CellValue::from_str("green")))
            .unwrap(),
        0
    );
    assert_eq!(test.count(eq("size", CellValue::from_u32(3))).unwrap(), 1);
}

#[test]
fn snapshot_selection_precedes_admission() {
    let test = colors();
    let head = test.db.head().unwrap();
    let older = Some(CommitId::new(head.get() - 1));
    let invalid = || eq("not valid", blue());
    let query = test
        .db
        .query(
            older,
            QueryOp::from_filter(invalid(), golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result();
    assert!(matches!(query, Err(ApiError::CommitUnavailable { .. })));
    let count = test
        .db
        .count(
            older,
            CountOp::filter(invalid(), golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result();
    assert!(matches!(count, Err(ApiError::CommitUnavailable { .. })));
}

/// `Database`'s query limit: its page size, hardcoded there until it is read
/// from genesis.
const MAX_QUERY_LIMIT: u32 = 100;

#[test]
fn a_page_holds_the_first_matches_up_to_the_page_size() {
    // `n` from 0 to one past the page size, created in reverse, so record ID
    // order, the query order, is descending `n`.
    let last = MAX_QUERY_LIMIT + 1;
    let test = TestDb::default();
    test.create(
        (0..=last)
            .rev()
            .map(|n| record(key(n.into()), &[("n", CellValue::from_u32(n))])),
    );
    let at_most = |max: u32| {
        range(
            "n",
            Bound::Unbounded,
            Bound::Included(CellValue::from_u32(max)),
        )
    };
    let page = |max: u32| {
        let op = QueryOp::from_filter(at_most(max), Budget::Limited(u64::MAX)).with_total();
        keys_and_total(test.db.query(None, op).into_result().unwrap())
    };
    // The keys of `n` from `high` down to `low`.
    let keys = |high: u32, low: u32| -> Vec<RecordKey> {
        (low..=high).rev().map(|n| key(n.into())).collect()
    };

    // Fewer matches than the page size, or as many: all of them.
    assert_eq!(page(9), (keys(9, 0), Some(10)));
    let full = MAX_QUERY_LIMIT - 1;
    assert_eq!(
        page(full),
        (keys(full, 0), Some(u64::from(MAX_QUERY_LIMIT)))
    );
    // More: the first ones in query order. The others are cut, yet counted.
    assert_eq!(page(last), (keys(last, 2), Some(u64::from(last) + 1)));
    assert_eq!(test.count(at_most(last)).unwrap(), u64::from(last) + 1);
}

#[test]
fn a_limit_returns_the_first_matches_up_to_it() {
    // Blue records 1, 3 and 4, in that query order.
    let test = colors();
    test.create([record(key(4), &[("color", blue())])]);
    let page = |limit| {
        let op = QueryOp::from_filter(eq("color", blue()), Budget::Limited(u64::MAX))
            .limit(limit)
            .with_total();
        keys_and_total(test.db.query(None, op).into_result().unwrap())
    };
    assert_eq!(page(1), (vec![key(1)], Some(3)));
    assert_eq!(page(2), (vec![key(1), key(3)], Some(3)));
    // A limit past the matches, up to the page size, takes them all.
    let all = vec![key(1), key(3), key(4)];
    assert_eq!(page(3), (all.clone(), Some(3)));
    assert_eq!(page(MAX_QUERY_LIMIT.into()), (all, Some(3)));
}

#[test]
fn a_limit_above_the_page_size_fails_stating_the_page_size() {
    let test = colors();
    let op = QueryOp::from_filter(eq("color", blue()), Budget::Limited(u64::MAX))
        .limit(u64::from(MAX_QUERY_LIMIT) + 1);
    let error = test
        .db
        .query(None, op)
        .into_result()
        .expect_err("a limit above the page size");
    assert!(
        matches!(
            &error,
            ApiError::LimitExceeded {
                limit: "query limit",
                max: MAX_QUERY_LIMIT
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("limit exceeded: query limit is capped at {MAX_QUERY_LIMIT}")
    );
}

#[test]
fn a_limit_of_zero_is_an_invalid_query() {
    let test = colors();
    let op = QueryOp::from_filter(eq("color", blue()), Budget::Limited(u64::MAX)).limit(0);
    let error = test
        .db
        .query(None, op)
        .into_result()
        .expect_err("a limit of 0");
    assert!(matches!(&error, ApiError::InvalidQuery { .. }), "{error:?}");
    assert_eq!(
        error.to_string(),
        "invalid query: `limit` must be at least 1"
    );
}

#[test]
fn the_limit_is_checked_before_the_filter() {
    let test = colors();
    let query = |op| test.db.query(None, op).into_result();
    let over = u64::from(MAX_QUERY_LIMIT) + 1;
    // A bad limit is reported before a bad filter.
    let bad_filter = || QueryOp::from_filter(eq("not valid", blue()), Budget::Limited(u64::MAX));
    assert!(matches!(
        query(bad_filter().limit(over)),
        Err(ApiError::LimitExceeded { .. })
    ));
    assert!(matches!(
        query(bad_filter().limit(0)),
        Err(ApiError::InvalidQuery { .. })
    ));
}

#[test]
fn an_offset_skips_the_first_matches() {
    // Blue records 1, 3 and 4, in that query order.
    let test = colors();
    test.create([record(key(4), &[("color", blue())])]);
    let page = |offset| {
        let op = QueryOp::from_filter(eq("color", blue()), Budget::Limited(u64::MAX))
            .offset(offset)
            .with_total();
        keys_and_total(test.db.query(None, op).into_result().unwrap())
    };
    assert_eq!(page(0), (vec![key(1), key(3), key(4)], Some(3)));
    assert_eq!(page(1), (vec![key(3), key(4)], Some(3)));
    assert_eq!(page(2), (vec![key(4)], Some(3)));
    // Past the last match: an empty page, not an error.
    for offset in [3, 4, u64::MAX] {
        assert_eq!(page(offset), (vec![], Some(3)), "{offset}");
    }
}

#[test]
fn offsets_page_through_every_match() {
    // More matches than two pages hold, created in reverse, so record ID
    // order, the query order, is descending `n`.
    let total = 2 * u64::from(MAX_QUERY_LIMIT) + 50;
    let test = TestDb::default();
    test.create(
        (0..total)
            .rev()
            .map(|n| record(key(n), &[("color", blue())])),
    );
    let in_query_order: Vec<_> = (0..total).rev().map(key).collect();
    let page = |offset, limit: Option<u64>| {
        let op =
            QueryOp::from_filter(eq("color", blue()), Budget::Limited(u64::MAX)).offset(offset);
        let op = QueryOp { limit, ..op };
        keys_and_total(test.db.query(None, op).into_result().unwrap()).0
    };
    for (limit, size) in [
        (None, MAX_QUERY_LIMIT as usize),
        (Some(7), 7),
        (Some(MAX_QUERY_LIMIT.into()), MAX_QUERY_LIMIT as usize),
    ] {
        // Each page's offset is the previous one plus the records returned,
        // until a page comes back empty. One query per expected page, and one
        // for the empty page past them: bounded, so a query that ignores its
        // offset fails the test rather than hangs it.
        let mut pages = Vec::new();
        let mut offset = 0;
        for _ in 0..=in_query_order.len().div_ceil(size) {
            let keys = page(offset, limit);
            if keys.is_empty() {
                break;
            }
            offset += keys.len() as u64;
            pages.push(keys);
        }
        let expected: Vec<_> = in_query_order.chunks(size).map(<[_]>::to_vec).collect();
        assert_eq!(pages, expected, "{limit:?}");
    }
}

/// A store holding five blue records, and their keys in query order: the
/// order they were created in.
fn five_blue_records() -> (MemoryStore, Vec<RecordKey>) {
    let store = MemoryStore::new();
    let db = Database::from_store(store.clone(), &config()).unwrap();
    let keys = [5, 1, 4, 2, 3].map(key).to_vec();
    let branch = db.begin().unwrap();
    for key in &keys {
        db.create(branch, record(*key, &[("color", blue())]))
            .into_result()
            .unwrap();
    }
    db.commit(branch).unwrap();
    (store, keys)
}

/// The page of blue records `paging` asks for, from queries over `store`
/// built with `max_query_limit`.
fn blue_page(
    store: &MemoryStore,
    max_query_limit: Option<u32>,
    paging: impl FnOnce(QueryOp) -> QueryOp,
) -> golemdb_query::Result<(Vec<RecordKey>, Option<u64>)> {
    let queries = Queries::new(
        store.clone(),
        Keccak256Hasher,
        None,
        max_query_limit,
        None,
        FetchAndSort,
    );
    let op = QueryOp::from_filter(eq("color", blue()), Budget::Limited(u64::MAX)).with_total();
    queries.query(None, paging(op)).map(keys_and_total)
}

#[test]
fn the_page_size_is_set_when_queries_are_built() {
    let (store, keys) = five_blue_records();
    let page = |max_query_limit| blue_page(&store, max_query_limit, |op| op).unwrap();
    // With no cap, every match.
    assert_eq!(page(None), (keys.clone(), Some(5)));
    assert_eq!(page(Some(5)), (keys.clone(), Some(5)));
    assert_eq!(page(Some(3)), (keys[..3].to_vec(), Some(5)));
    assert_eq!(page(Some(0)), (vec![], Some(5)));

    // A count returns no records: the page size plays no part.
    let queries = Queries::new(store, Keccak256Hasher, None, Some(1), None, FetchAndSort);
    let count = CountOp::filter(eq("color", blue()), Budget::Limited(u64::MAX));
    assert_eq!(queries.count(None, count).unwrap(), 5);
}

#[test]
fn a_limit_holds_up_to_the_page_size_of_its_queries() {
    let (store, keys) = five_blue_records();
    let page = |max_query_limit, limit| blue_page(&store, max_query_limit, |op| op.limit(limit));
    assert_eq!(page(Some(3), 2).unwrap(), (keys[..2].to_vec(), Some(5)));
    assert_eq!(page(Some(3), 3).unwrap(), (keys[..3].to_vec(), Some(5)));
    assert_eq!(page(Some(10), 8).unwrap(), (keys.clone(), Some(5)));
    // With no page size, any limit holds.
    assert_eq!(page(None, 4).unwrap(), (keys[..4].to_vec(), Some(5)));
    assert_eq!(page(None, u64::MAX).unwrap(), (keys.clone(), Some(5)));
    // Past the page size, a limit fails, whatever the matches.
    for limit in [4, 10] {
        assert!(
            matches!(
                page(Some(3), limit),
                Err(QueryError::LimitExceeded {
                    limit: "query limit",
                    max: 3
                })
            ),
            "{limit}"
        );
    }
    // A limit of 0 never holds.
    for max_query_limit in [None, Some(3)] {
        assert!(
            matches!(
                page(max_query_limit, 0),
                Err(QueryError::InvalidQuery { .. })
            ),
            "{max_query_limit:?}"
        );
    }
}

#[test]
fn an_offset_starts_the_page_of_its_queries() {
    let (store, keys) = five_blue_records();
    let page = |max_query_limit, paging: fn(QueryOp) -> QueryOp| {
        blue_page(&store, max_query_limit, paging)
    };
    // With no page size, the page holds every match past the offset.
    assert_eq!(
        page(None, |op| op.offset(2)).unwrap(),
        (keys[2..].to_vec(), Some(5))
    );
    // With one, it holds up to the page size, or up to the limit.
    assert_eq!(
        page(Some(3), |op| op.offset(1)).unwrap(),
        (keys[1..4].to_vec(), Some(5))
    );
    assert_eq!(
        page(Some(3), |op| op.offset(4)).unwrap(),
        (keys[4..].to_vec(), Some(5))
    );
    assert_eq!(
        page(Some(3), |op| op.offset(1).limit(2)).unwrap(),
        (keys[1..3].to_vec(), Some(5))
    );
    assert_eq!(page(Some(3), |op| op.offset(5)).unwrap(), (vec![], Some(5)));
}

#[test]
fn materialization_requires_identity_but_does_not_validate_metadata() {
    for missing in [reserved::KEY, reserved::META] {
        let store = MemoryStore::new();
        let db = Database::from_store(store.clone(), &config()).unwrap();
        let branch = db.begin().unwrap();
        db.create(branch, record(key(1), &[("color", blue())]))
            .into_result()
            .unwrap();
        db.commit(branch).unwrap();

        // Remove an engine-owned cell below the facade while keeping the
        // indexed attribute. Materialization needs #key but does not use #meta.
        let branches = Branches::new(store, Keccak256Hasher, None, None).unwrap();
        let corrupt = branches.begin().unwrap();
        branches
            .write(corrupt, |cell_writer| {
                cell_writer.delete(CellKey::new(FIRST_USER_RECORD_ID, missing));
                Ok::<_, BranchError>(())
            })
            .unwrap();
        branches.commit(corrupt).unwrap();

        let result = db
            .query(
                None,
                QueryOp::from_filter(eq("color", blue()), golemdb_api::Budget::Limited(u64::MAX)),
            )
            .into_result();
        if missing == reserved::KEY {
            let Err(ApiError::Internal { source }) = result else {
                panic!("expected Internal, got {result:?}");
            };
            assert!(matches!(
                source.downcast_ref::<RecordError>(),
                Some(RecordError::CorruptState(_))
            ));
        } else {
            let page = result.unwrap();
            assert_eq!(page.records.len(), 1);
            let record = &page.records[0];
            assert_eq!(record.key, key(1));
            assert!(!record.cells.contains_key(reserved::META.as_bytes()));
            assert_eq!(record.meta(), None);
        }
    }
}
