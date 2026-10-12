//! The `query` and `count` calls: what they return, and what they reject.

mod support;

use golemdb_api::{Api, BranchId, CommitId, ReadTarget, RecordOp};
use golemdb_cells::{CellType, CellValue, Width};
use golemdb_query::{Budget, CountOp, QueryError, QueryOp, SortKey};
use support::{TestDb, eq, key, record};

fn blue() -> CellValue {
    CellValue::from_str("blue")
}

/// Two blue records with extra cells, one of them not indexed, and a red one.
fn colors() -> TestDb {
    let test = TestDb::default();
    test.create([
        record(
            key(1),
            &[("color", blue()), ("size", CellValue::from_u32(3))],
        )
        .field("note", CellValue::from_str("first"))
        .expect("valid field"),
        record(key(2), &[("color", CellValue::from_str("red"))]),
        record(key(3), &[("color", blue())]),
    ]);
    test
}

#[test]
fn records_are_returned_as_get_returns_them() {
    let test = colors();
    let page = test
        .queries
        .query(ReadTarget::Head, QueryOp::filter(eq("color", blue())))
        .result
        .unwrap();
    let get = |n| {
        test.db
            .get(ReadTarget::Head, RecordOp::get(key(n)))
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
    let op = QueryOp::filter(eq("color", blue())).only(["size", "note", "missing"]);
    let page = test.queries.query(ReadTarget::Head, op).result.unwrap();
    let get = |n| {
        let op = RecordOp::get(key(n)).only(["size", "note", "missing"]);
        test.db.get(ReadTarget::Head, op).unwrap()
    };
    assert_eq!(page.records, [get(1), get(3)]);

    let empty = QueryOp::filter(eq("color", blue())).only(Vec::<&str>::new());
    let page = test.queries.query(ReadTarget::Head, empty).result.unwrap();
    assert!(page.records.iter().all(|record| record.cells.is_empty()));
    assert_eq!(page.records.len(), 2);
}

#[test]
fn total_matched_only_when_asked() {
    let test = colors();
    let query = |op| test.queries.query(ReadTarget::Head, op).result.unwrap();
    assert_eq!(
        query(QueryOp::filter(eq("color", blue()))).total_matched,
        None
    );
    let op = QueryOp::filter(eq("color", blue())).with_total();
    assert_eq!(query(op).total_matched, Some(2));
}

#[test]
fn pages_report_the_commit_they_read() {
    let test = colors();
    test.create([record(key(4), &[("color", blue())])]);
    let head = test.db.head().unwrap();
    let page = test
        .queries
        .query(ReadTarget::Head, QueryOp::filter(eq("color", blue())))
        .result
        .unwrap();
    assert_eq!(page.evaluated_at, head);
    assert_eq!(page.records.len(), 3);
}

#[test]
fn the_head_commit_can_be_named() {
    let test = colors();
    let head = test.db.head().unwrap();
    let op = QueryOp::filter(eq("color", blue()));
    let page = test
        .queries
        .query(ReadTarget::Commit(head), op)
        .result
        .unwrap();
    assert_eq!(page.evaluated_at, head);
    let count = test
        .queries
        .count(
            ReadTarget::Commit(head),
            CountOp::filter(eq("color", blue())),
        )
        .result
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn other_commits_are_unavailable() {
    let test = colors();
    let head = test.db.head().unwrap();
    let older = CommitId::new(head.get() - 1);
    let result = test
        .queries
        .query(
            ReadTarget::Commit(older),
            QueryOp::filter(eq("color", blue())),
        )
        .result;
    assert!(matches!(
        result,
        Err(QueryError::CommitUnavailable { requested, head: got }) if requested == older && got == head
    ));
}

#[test]
fn branches_are_not_queried() {
    let test = colors();
    let branch = test.db.begin().unwrap();
    for target in [
        ReadTarget::Branch(branch),
        ReadTarget::Branch(BranchId::new(u64::MAX)),
    ] {
        let query = test
            .queries
            .query(target, QueryOp::filter(eq("color", blue())))
            .result;
        assert!(matches!(query, Err(QueryError::InvalidQuery { .. })));
        let count = test
            .queries
            .count(target, CountOp::filter(eq("color", blue())))
            .result;
        assert!(matches!(count, Err(QueryError::InvalidQuery { .. })));
    }
}

#[test]
fn pending_branch_work_is_invisible() {
    let test = colors();
    let branch = test.db.begin().unwrap();
    test.db
        .create(branch, record(key(7), &[("color", blue())]))
        .unwrap();
    assert_eq!(test.keys(eq("color", blue())).unwrap(), [key(1), key(3)]);
    test.db.commit(branch).unwrap();
    assert_eq!(
        test.keys(eq("color", blue())).unwrap(),
        [key(1), key(3), key(7)]
    );
}

#[test]
fn a_target_error_comes_before_admission() {
    let test = colors();
    let branch = test.db.begin().unwrap();
    let invalid = eq("not valid", blue());
    let result = test
        .queries
        .query(ReadTarget::Branch(branch), QueryOp::filter(invalid))
        .result;
    assert!(matches!(result, Err(QueryError::InvalidQuery { .. })));
}

#[test]
fn unbuilt_parts_are_not_implemented() {
    let test = colors();
    let filter = || QueryOp::filter(eq("color", blue()));
    for (op, part) in [
        (
            filter().sort([SortKey::asc("size", CellType::Uint(Width::W4))]),
            "query: sort",
        ),
        (filter().limit(1), "query: limit"),
        (filter().offset(1), "query: offset"),
    ] {
        let result = test.queries.query(ReadTarget::Head, op).result;
        assert!(
            matches!(result, Err(QueryError::NotImplemented { operation }) if operation == part),
            "{part}"
        );
    }
}

#[test]
fn the_budget_is_accepted_but_not_enforced_yet() {
    let test = colors();
    let op = QueryOp::filter(eq("color", blue())).budget(Budget::Limited(0));
    let page = test.queries.query(ReadTarget::Head, op).result.unwrap();
    assert_eq!(page.records.len(), 2);
    let op = CountOp::filter(eq("color", blue())).budget(Budget::Limited(0));
    assert_eq!(test.queries.count(ReadTarget::Head, op).result.unwrap(), 2);
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
