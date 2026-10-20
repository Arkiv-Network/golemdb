//! The `query` and `count` calls: what they return, and what they reject.

mod support;

use golemdb_api::{Api, CommitId, ReadTarget, RecordOp};
use golemdb_api::{ApiError, Budget, CountOp, QueryOp, SortKey};
use golemdb_api::{CellLimits, Database, Genesis, HashAlgorithm, OpenConfig};
use golemdb_branch::{BranchError, Branches};
use golemdb_cells::system::FIRST_USER_RECORD_ID;
use golemdb_cells::{CellKey, CellType, CellValue, Width, reserved};
use golemdb_merkle::Keccak256Hasher;
use golemdb_record::RecordError;
use golemdb_storage::MemoryStore;
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
        .db
        .query(None, QueryOp::from_filter(eq("color", blue())))
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
    let op = QueryOp::from_filter(eq("color", blue())).only(["size", "note", "missing"]);
    let page = test.db.query(None, op).unwrap();
    let get = |n| {
        let op = RecordOp::get(key(n)).only(["size", "note", "missing"]);
        test.db.get(ReadTarget::Head, op).unwrap()
    };
    assert_eq!(page.records, [get(1), get(3)]);

    let empty = QueryOp::from_filter(eq("color", blue())).only(Vec::<&str>::new());
    let page = test.db.query(None, empty).unwrap();
    assert!(page.records.iter().all(|record| record.cells.is_empty()));
    assert_eq!(page.records.len(), 2);
}

#[test]
fn total_matched_only_when_asked() {
    let test = colors();
    let query = |op| test.db.query(None, op).unwrap();
    assert_eq!(
        query(QueryOp::from_filter(eq("color", blue()))).total_matched,
        None
    );
    let op = QueryOp::from_filter(eq("color", blue())).with_total();
    assert_eq!(query(op).total_matched, Some(2));
}

#[test]
fn pages_report_the_commit_they_read() {
    let test = colors();
    test.create([record(key(4), &[("color", blue())])]);
    let head = test.db.head().unwrap();
    let page = test
        .db
        .query(None, QueryOp::from_filter(eq("color", blue())))
        .unwrap();
    assert_eq!(page.evaluated_at, head);
    assert_eq!(page.records.len(), 3);
}

#[test]
fn the_head_commit_can_be_named() {
    let test = colors();
    let head = test.db.head().unwrap();
    let op = QueryOp::from_filter(eq("color", blue()));
    let page = test.db.query(Some(head), op).unwrap();
    assert_eq!(page.evaluated_at, head);
    let count = test
        .db
        .count(Some(head), CountOp::filter(eq("color", blue())))
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
        .query(Some(older), QueryOp::from_filter(eq("color", blue())));
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
        .unwrap();
    assert_eq!(test.keys(eq("color", blue())).unwrap(), [key(1), key(3)]);
    test.db.commit(branch).unwrap();
    assert_eq!(
        test.keys(eq("color", blue())).unwrap(),
        [key(1), key(3), key(7)]
    );
}

#[test]
fn unbuilt_parts_are_not_implemented() {
    let test = colors();
    let filter = || QueryOp::from_filter(eq("color", blue()));
    for (op, part) in [
        (
            filter().sort([SortKey::asc("size", CellType::Uint(Width::W4))]),
            "query: sort",
        ),
        (filter().limit(1), "query: limit"),
        (filter().offset(1), "query: offset"),
    ] {
        let result = test.db.query(None, op);
        assert!(
            matches!(result, Err(ApiError::NotImplemented { operation }) if operation == part),
            "{part}"
        );
    }
}

#[test]
fn the_budget_is_accepted_but_not_enforced_yet() {
    let test = colors();
    let op = QueryOp::from_filter(eq("color", blue())).budget(Budget::Limited(0));
    let page = test.db.query(None, op).unwrap();
    assert_eq!(page.records.len(), 2);
    let op = CountOp::filter(eq("color", blue())).budget(Budget::Limited(0));
    assert_eq!(test.db.count(None, op).unwrap(), 2);
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
fn query_keys_are_the_matching_keys_ascending() {
    let test = TestDb::default();
    test.create([
        record(key(9), &[("color", blue())]),
        record(key(2), &[("color", CellValue::from_str("red"))]),
        record(key(1), &[("color", blue())]),
        record(key(5), &[("color", blue())]),
    ]);
    let keys = |filter| test.db.query_keys(None, CountOp::filter(filter)).unwrap();
    assert_eq!(keys(eq("color", blue())), [key(1), key(5), key(9)]);
    assert_eq!(keys(eq("color", CellValue::from_str("green"))), []);
}

/// Past the one-by-one limit the keys come off the bindings, which hold
/// reserved records' keys too: only the matches' are returned.
#[test]
fn query_keys_of_a_large_match_set_come_off_the_bindings() {
    let test = TestDb::default();
    let n = (1 << 16) + (1 << 13);
    // Keys created out of key order, every tenth one red.
    let color = |i: u64| if i.is_multiple_of(10) { "red" } else { "blue" };
    let scrambled = |i: u64| i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    test.create((0..n).map(|i| {
        record(
            key(scrambled(i)),
            &[("color", CellValue::from_str(color(i)))],
        )
    }));
    let mut blue_keys: Vec<_> = (0..n)
        .filter(|&i| color(i) == "blue")
        .map(|i| key(scrambled(i)))
        .collect();
    blue_keys.sort_unstable();
    let keys = test
        .db
        .query_keys(None, CountOp::filter(eq("color", blue())))
        .unwrap();
    assert!(keys.len() as u64 > 1 << 16);
    assert_eq!(keys, blue_keys);
}

#[test]
fn a_non_head_commit_is_checked_after_admission() {
    let test = colors();
    let head = test.db.head().unwrap();
    let older = Some(CommitId::new(head.get() - 1));
    let invalid = || eq("not valid", blue());
    let query = test.db.query(older, QueryOp::from_filter(invalid()));
    assert!(matches!(query, Err(ApiError::InvalidArgument { .. })));
    let count = test.db.count(older, CountOp::filter(invalid()));
    assert!(matches!(count, Err(ApiError::InvalidArgument { .. })));
}

#[test]
fn an_indexed_record_without_its_key_is_internal_corruption() {
    let store = MemoryStore::new();
    let limits = CellLimits {
        max_cell_name_len: 32,
        max_str_len: 64,
        max_bytes_len: 64,
    };
    let config = OpenConfig::new(Genesis::new(HashAlgorithm::Keccak256, limits, None));
    let db = Database::from_store(store.clone(), &config).unwrap();
    let branch = db.begin().unwrap();
    db.create(branch, record(key(1), &[("color", blue())]))
        .unwrap();
    db.commit(branch).unwrap();

    // Corrupt the committed state below the facade: the record keeps its
    // indexed attribute but loses its `#key` cell.
    let branches = Branches::new(store, Keccak256Hasher, None).unwrap();
    let corrupt = branches.begin().unwrap();
    branches
        .write(corrupt, |cell_writer| {
            cell_writer.delete(CellKey::new(FIRST_USER_RECORD_ID, reserved::KEY));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    branches.commit(corrupt).unwrap();

    let result = db.query(None, QueryOp::from_filter(eq("color", blue())));
    let Err(ApiError::Internal { source }) = result else {
        panic!("expected Internal, got {result:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RecordError>(),
        Some(RecordError::CorruptState(_))
    ));
}
