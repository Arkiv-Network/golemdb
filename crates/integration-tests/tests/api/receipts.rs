use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use golemdb_api::*;
use golemdb_cells::{CellLimitError, CellNameError, CellValueParseError, system};
use golemdb_storage::{MemoryStore, StorageError, Store};
use std::error::Error;

use super::facade::each_backend;

const KEY: RecordKey = RecordKey([0x42; 32]);
// A zero budget is explicitly accepted but unenforced in this increment.
const BUDGET: Budget = Budget::Limited(0);

fn create() -> RecordOp<op::Create> {
    RecordOp::create(KEY, BUDGET)
        .field("age", CellValue::from_i32(30))
        .attribute("color", CellValue::from_str("red"))
}

fn filter(name: &str) -> Filter {
    Filter::Eq {
        field: CellNameRef::raw(name.as_bytes()).into(),
        value: EqValue::of(marker::Str, "red"),
    }
}

fn unchanged<T>(call: &Metered<T>, head: Option<CommitId>) {
    assert_eq!(call.receipt.cost, 0);
    assert_eq!(call.receipt.priced_at, head);
    assert_eq!(call.receipt.details, Details::default());
}

#[test]
fn effects_follow_actual_user_changes_and_failed_patches_are_atomic() {
    each_backend(|db| {
        let head = db.head().unwrap();
        let branch = db.begin().unwrap();
        let created = db.create(branch, create());
        assert_eq!(created.result.unwrap(), KEY);
        let details = created.receipt.details;
        assert_eq!(created.receipt.priced_at, Some(head));
        assert_eq!(created.receipt.cost, 0);
        assert_eq!(details.cells_created, 2);
        assert_eq!(details.cell_bytes_written, 33);
        assert_eq!(details.index_joins, 1);
        assert_eq!(details.index_bytes_written, 10);
        assert_eq!(
            details.cells_updated + details.cells_deleted + details.index_leaves,
            0
        );
        assert_eq!(details.cell_bytes_deleted + details.index_bytes_deleted, 0);
        let version = db.branch_info(branch).unwrap().version;
        for patch in [
            RecordOp::patch(KEY, BUDGET),
            RecordOp::patch(KEY, BUDGET)
                .set("age", CellValue::from_i32(30))
                .set("color", CellValue::from_str("red"))
                .remove("missing"),
        ] {
            let call = db.patch(branch, patch);
            unchanged(&call, Some(head));
            call.result.unwrap();
            assert_eq!(db.branch_info(branch).unwrap().version, version);
        }
        let changed = db.patch(
            branch,
            RecordOp::patch(KEY, BUDGET).set("color", CellValue::from_str("blue")),
        );
        changed.result.unwrap();
        let d = changed.receipt.details;
        assert_eq!(
            (d.cells_updated, d.cell_bytes_written, d.cell_bytes_deleted),
            (1, 18, 17)
        );
        assert_eq!(
            (
                d.index_joins,
                d.index_leaves,
                d.index_bytes_written,
                d.index_bytes_deleted
            ),
            (1, 1, 11, 10)
        );
        let before = db.get(ReadTarget::Branch(branch), RecordOp::get(KEY, BUDGET));
        unchanged(&before, Some(head));
        let before = before.result.unwrap();
        assert_eq!(
            before.cells[b"color".as_slice()].kind(),
            CellKind::Attribute
        );
        let version = db.branch_info(branch).unwrap().version;
        let failed = db.patch(
            branch,
            RecordOp::patch(KEY, BUDGET)
                .set("age", CellValue::from_i32(31))
                .set("zzz", CellValue::from_bool(true)),
        );
        unchanged(&failed, Some(head));
        assert!(
            matches!(failed.result, Err(ApiError::CellNotFound { name }) if name.as_bytes() == b"zzz")
        );
        assert_eq!(db.branch_info(branch).unwrap().version, version);
        assert_eq!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(KEY, BUDGET))
                .result
                .unwrap(),
            before
        );
        let invalid_kind = db.patch(
            branch,
            RecordOp::patch(KEY, BUDGET).set("color", CellValue::from_bytes(b"blue")),
        );
        unchanged(&invalid_kind, Some(head));
        assert!(matches!(
            invalid_kind.result,
            Err(ApiError::InvalidArgument { .. })
        ));
        let kind_change = db.patch(
            branch,
            RecordOp::patch(KEY, BUDGET).field("color", CellValue::from_str("blue")),
        );
        kind_change.result.unwrap();
        let d = kind_change.receipt.details;
        assert_eq!((d.cells_updated, d.index_leaves, d.index_joins), (1, 1, 0));
        let type_change = db.patch(
            branch,
            RecordOp::patch(KEY, BUDGET).set("age", CellValue::from_u32(30)),
        );
        type_change.result.unwrap();
        assert_eq!(type_change.receipt.details.cells_updated, 1);
        let deleted = db.delete(branch, RecordOp::delete(KEY, BUDGET));
        deleted.result.unwrap();
        let d = deleted.receipt.details;
        assert_eq!(
            (d.cells_deleted, d.cell_bytes_deleted, d.index_leaves),
            (2, 34, 0)
        );
        assert_eq!(d.cells_created + d.cells_updated + d.cell_bytes_written, 0);
        let missing = db.delete(branch, RecordOp::delete(KEY, BUDGET));
        unchanged(&missing, Some(head));
        assert!(matches!(missing.result, Err(ApiError::NotFound)));
    });
}

#[test]
fn handle_then_admission_then_record_state_and_failures_always_have_receipts() {
    each_backend(|db| {
        let head = db.head().unwrap();
        let branch = db.begin().unwrap();
        let invalid = || RecordOp::create(KEY, BUDGET).field("", CellValue::from_bool(true));
        let rejected = db.create(branch, invalid());
        unchanged(&rejected, Some(head));
        assert!(matches!(
            rejected.result,
            Err(ApiError::InvalidArgument { .. })
        ));
        assert_eq!(db.branch_info(branch).unwrap().version, 0);
        let bad_patch = db.patch(
            branch,
            RecordOp::patch(KEY, BUDGET).field(&"a".repeat(33), CellValue::from_bool(true)),
        );
        unchanged(&bad_patch, Some(head));
        assert!(matches!(
            bad_patch.result,
            Err(ApiError::InvalidArgument { .. })
        ));
        db.seal(branch).unwrap();
        let sealed = db.create(branch, invalid());
        unchanged(&sealed, None);
        assert!(matches!(sealed.result, Err(ApiError::Sealed)));
        let read = db.get(
            ReadTarget::Branch(branch),
            RecordOp::get(RecordKey(system::ALLOC.key), BUDGET),
        );
        unchanged(&read, Some(head));
        read.result.unwrap();
        db.discard(branch).unwrap();
        let dead = db.create(branch, invalid());
        unchanged(&dead, None);
        assert!(matches!(dead.result, Err(ApiError::HandleInvalid)));
        let dead_reserved = db.delete(
            branch,
            RecordOp::delete(RecordKey(system::ALLOC.key), BUDGET),
        );
        unchanged(&dead_reserved, None);
        assert!(matches!(dead_reserved.result, Err(ApiError::HandleInvalid)));
        let stale = db.begin().unwrap();
        let winner = db.begin().unwrap();
        db.commit(winner).unwrap();
        let rejected = db.create(stale, invalid());
        unchanged(&rejected, None);
        assert!(matches!(rejected.result, Err(ApiError::HandleInvalid)));
    });
}

#[test]
fn raw_admission_errors_preserve_input_order_causes_and_record_state() {
    each_backend(|db| {
        let branch = db.begin().unwrap();
        let head = db.head().unwrap();
        let allocator = || {
            db.get(
                ReadTarget::Branch(branch),
                RecordOp::get(RecordKey(system::ALLOC.key), BUDGET),
            )
            .result
            .unwrap()
        };
        let before_alloc = allocator();
        let before_info = db.branch_info(branch).unwrap();

        // An earlier deployment-limit error wins over a later syntax error.
        // This used to be impossible: the builder saved the later syntax error.
        let bad = RecordOp::create(KEY, BUDGET)
            .field("z", CellValue::from_str(&"x".repeat(65)))
            .field("", CellValue::from_bool(true));
        assert_eq!(bad.entries().len(), 2);
        let call = db.create(branch, bad);
        unchanged(&call, Some(head));
        let error = call.result.unwrap_err();
        assert!(error.to_string().contains("field(\"z\")"));
        assert!(error.source().unwrap().is::<CellLimitError>());

        let call = db.create(
            branch,
            RecordOp::create(KEY, BUDGET)
                .field("", CellValue::from_bool(true))
                .field("z", CellValue::from_str(&"x".repeat(65))),
        );
        unchanged(&call, Some(head));
        assert_eq!(
            call.result
                .unwrap_err()
                .source()
                .unwrap()
                .downcast_ref::<CellNameError>(),
            Some(&CellNameError::Empty)
        );

        let call = db.create(
            branch,
            RecordOp::create(KEY, BUDGET).attribute("payload", CellValue::from_bytes(b"x")),
        );
        unchanged(&call, Some(head));
        assert_eq!(
            call.result
                .unwrap_err()
                .source()
                .unwrap()
                .downcast_ref::<CellValueParseError>(),
            Some(&CellValueParseError::NotIndexable)
        );

        let call = db.create(
            branch,
            RecordOp::create(KEY, BUDGET)
                .field("age", CellValue::from_i32(1))
                .attribute("age", CellValue::from_i32(2)),
        );
        unchanged(&call, Some(head));
        assert!(
            call.result
                .unwrap_err()
                .to_string()
                .contains("already writes or removes")
        );
        assert_eq!(allocator(), before_alloc);
        assert_eq!(db.branch_info(branch).unwrap(), before_info);

        db.create(branch, create()).result.unwrap();
        let before = db
            .get(ReadTarget::Branch(branch), RecordOp::get(KEY, BUDGET))
            .result
            .unwrap();
        let version = db.branch_info(branch).unwrap().version;
        for patch in [
            RecordOp::patch(KEY, BUDGET)
                .field("age", CellValue::from_i32(99))
                .remove("age"),
            RecordOp::patch(KEY, BUDGET)
                .remove("age")
                .set("age", CellValue::from_i32(99)),
            RecordOp::patch(KEY, BUDGET)
                .field("age", CellValue::from_i32(99))
                .remove(""),
            RecordOp::patch(KEY, BUDGET)
                .field("age", CellValue::from_i32(99))
                .attribute("payload", CellValue::from_bytes(b"x")),
        ] {
            let call = db.patch(branch, patch);
            unchanged(&call, Some(head));
            assert!(matches!(call.result, Err(ApiError::InvalidArgument { .. })));
            assert_eq!(db.branch_info(branch).unwrap().version, version);
            assert_eq!(
                db.get(ReadTarget::Branch(branch), RecordOp::get(KEY, BUDGET))
                    .result
                    .unwrap(),
                before
            );
        }

        // Fixed target checks precede entry validation.
        let call = db.create(
            branch,
            RecordOp::create(RecordKey(system::ALLOC.key), BUDGET)
                .field("", CellValue::from_bool(true)),
        );
        unchanged(&call, Some(head));
        assert!(matches!(call.result, Err(ApiError::Reserved)));

        // A length failure's diagnostic is bounded even for a very large name.
        let call = db.patch(
            branch,
            RecordOp::patch(KEY, BUDGET).remove(&"a".repeat(100_000)),
        );
        unchanged(&call, Some(head));
        let error = call.result.unwrap_err();
        assert!(error.to_string().len() < 200);
        assert!(matches!(
            error.source().unwrap().downcast_ref::<CellNameError>(),
            Some(CellNameError::TooLong {
                actual: 100_000,
                ..
            })
        ));
    });
}

#[test]
fn get_normalizes_raw_projections_during_branch_and_committed_execution() {
    each_backend(|db| {
        let branch = db.begin().unwrap();
        db.create(branch, create()).result.unwrap();
        let request = RecordOp::get(KEY, BUDGET).only([
            b"color".as_slice(),
            b"#key",
            b"age",
            b"color",
            &[0, 255],
            b"age",
        ]);
        assert_eq!(request.names().unwrap().len(), 6);
        let pending = db
            .get(ReadTarget::Branch(branch), request.clone())
            .result
            .unwrap();
        assert_eq!(pending.cells.len(), 3);
        db.commit(branch).unwrap();
        assert_eq!(db.get(ReadTarget::Head, request).result.unwrap(), pending);
    });
}

#[test]
fn query_count_and_unavailable_history_report_the_selected_head() {
    each_backend(|db| {
        let branch = db.begin().unwrap();
        db.create(branch, create()).result.unwrap();
        let head = db.commit(branch).unwrap();
        let query = db.query(None, QueryOp::from_filter(filter("color"), BUDGET));
        unchanged(&query, Some(head));
        assert_eq!(query.result.unwrap().records.len(), 1);
        let count = db.count(None, CountOp::filter(filter("color"), BUDGET));
        unchanged(&count, Some(head));
        assert_eq!(count.result.unwrap(), 1);
        let invalid = db.query(None, QueryOp::from_filter(filter("bad name"), BUDGET));
        unchanged(&invalid, Some(head));
        assert!(matches!(
            invalid.result,
            Err(ApiError::InvalidArgument { .. })
        ));
        let invalid = db.count(None, CountOp::filter(filter("bad name"), BUDGET));
        unchanged(&invalid, Some(head));
        assert!(matches!(
            invalid.result,
            Err(ApiError::InvalidArgument { .. })
        ));
        let by_age = SortKey::asc("age", CellType::Int(Width::W4));
        let unsupported = db.query(
            None,
            QueryOp::from_filter(filter("color"), BUDGET).sort([by_age]),
        );
        unchanged(&unsupported, Some(head));
        assert!(matches!(
            unsupported.result,
            Err(ApiError::NotImplemented { .. })
        ));
        for requested in [CommitId::GENESIS, CommitId::new(head.get() + 1)] {
            let get = db.get(ReadTarget::Commit(requested), RecordOp::get(KEY, BUDGET));
            unchanged(&get, Some(head));
            assert!(matches!(
                get.result,
                Err(ApiError::CommitUnavailable { .. })
            ));
            let query = db.query(
                Some(requested),
                QueryOp::from_filter(filter("color"), BUDGET),
            );
            unchanged(&query, Some(head));
            assert!(matches!(
                query.result,
                Err(ApiError::CommitUnavailable { .. })
            ));
            let count = db.count(Some(requested), CountOp::filter(filter("color"), BUDGET));
            unchanged(&count, Some(head));
            assert!(matches!(
                count.result,
                Err(ApiError::CommitUnavailable { .. })
            ));
        }
    });
}

#[test]
fn boolean_admission_failures_retain_the_selected_head() {
    each_backend(|db| {
        let head = db.commit(db.begin().unwrap()).unwrap();
        // Database currently caps And/Or nesting at 64 levels.
        let too_deep = (0..65).fold(filter("color"), |child, _| Filter::And(vec![child]));
        for filter in [Filter::And(vec![]), Filter::Or(vec![]), too_deep] {
            let query = db.query(None, QueryOp::from_filter(filter.clone(), BUDGET));
            let count = db.count(None, CountOp::filter(filter, BUDGET));
            unchanged(&query, Some(head));
            unchanged(&count, Some(head));
            for error in [query.result.unwrap_err(), count.result.unwrap_err()] {
                assert!(matches!(
                    error,
                    ApiError::InvalidQuery { .. }
                        | ApiError::LimitExceeded {
                            limit: "filter depth",
                            max: 64
                        }
                ));
            }
        }
    });
}

#[test]
fn limit_failures_retain_the_selected_head() {
    each_backend(|db| {
        let head = db.commit(db.begin().unwrap()).unwrap();
        let query = |limit| {
            db.query(
                None,
                QueryOp::from_filter(filter("color"), BUDGET).limit(limit),
            )
        };
        let zero = query(0);
        unchanged(&zero, Some(head));
        assert!(matches!(zero.result, Err(ApiError::InvalidQuery { .. })));
        // Database currently caps a query's limit at its page size, 100.
        let over = query(101);
        unchanged(&over, Some(head));
        assert!(matches!(
            over.result,
            Err(ApiError::LimitExceeded {
                limit: "query limit",
                max: 100
            })
        ));
    });
}

#[derive(Clone, Default)]
struct PausingStore {
    inner: MemoryStore,
    pause: Arc<Mutex<Option<ReadPause>>>,
    fail: Arc<AtomicBool>,
}

struct ReadPause {
    captured: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

const READ_TIMEOUT: Duration = Duration::from_secs(10);

impl Store for PausingStore {
    type Read<'a> = <MemoryStore as Store>::Read<'a>;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn max_key_size(&self) -> usize {
        self.inner.max_key_size()
    }
    fn max_value_size(&self) -> usize {
        self.inner.max_value_size()
    }
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(StorageError::Backend("injected head read failure".into()));
        }
        let snapshot = self.inner.begin_read()?;
        let pause = self.pause.lock().unwrap().take();
        if let Some(pause) = pause {
            pause.captured.send(()).map_err(|error| {
                StorageError::Backend(format!("snapshot receiver disconnected: {error}").into())
            })?;
            pause.resume.recv_timeout(READ_TIMEOUT).map_err(|error| {
                StorageError::Backend(format!("snapshot was not resumed: {error}").into())
            })?;
        }
        Ok(snapshot)
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        self.inner.begin_write()
    }
}

fn paused_database() -> (Database, PausingStore) {
    let store = PausingStore::default();
    let config = OpenConfig::new(Genesis::new(
        HashAlgorithm::Blake3,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        golemdb_api::RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
    ));
    let db = Database::from_store(store.clone(), &config).unwrap();
    (db, store)
}

#[test]
fn advancing_head_cannot_change_the_context_of_an_inflight_read() {
    for operation in ["get", "branch get", "query", "count"] {
        let (db, store) = paused_database();
        let branch = db.begin().unwrap();
        db.create(branch, create()).result.unwrap();
        let old_head = db.commit(branch).unwrap();
        let reader_branch = db.begin().unwrap();
        let deleting = db.begin().unwrap();
        db.delete(deleting, RecordOp::delete(KEY, BUDGET))
            .result
            .unwrap();
        let (captured_tx, captured_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        *store.pause.lock().unwrap() = Some(ReadPause {
            captured: captured_tx,
            resume: resume_rx,
        });
        let reader = db.clone();
        let task = std::thread::spawn(move || {
            let outcome = match operation {
                "get" => reader
                    .get(ReadTarget::Head, RecordOp::get(KEY, BUDGET))
                    .map(|_| 1),
                "branch get" => reader
                    .get(
                        ReadTarget::Branch(reader_branch),
                        RecordOp::get(KEY, BUDGET),
                    )
                    .map(|_| 1),
                "query" => reader
                    .query(None, QueryOp::from_filter(filter("color"), BUDGET))
                    .map(|p| p.records.len() as u64),
                _ => reader.count(None, CountOp::filter(filter("color"), BUDGET)),
            };
            completed_tx.send(outcome).expect("receive read outcome");
        });
        captured_rx
            .recv_timeout(READ_TIMEOUT)
            .expect("reader captures its snapshot before the commit");
        let new_head = db.commit(deleting).unwrap();
        resume_tx
            .send(())
            .expect("resume the reader after the commit");
        let outcome = completed_rx
            .recv_timeout(READ_TIMEOUT)
            .expect("reader completes after resuming");
        task.join().unwrap();
        unchanged(&outcome, Some(old_head));
        assert_eq!(outcome.result.unwrap(), 1);
        assert!(new_head > old_head);
    }
}

#[test]
fn inability_to_select_a_snapshot_has_no_pricing_commit() {
    let (db, store) = paused_database();
    let branch = db.begin().unwrap();
    store.fail.store(true, Ordering::SeqCst);
    let calls = [
        db.get(ReadTarget::Head, RecordOp::get(KEY, BUDGET))
            .map(|_| ()),
        db.create(branch, create()).map(|_| ()),
        db.query(None, QueryOp::from_filter(filter("color"), BUDGET))
            .map(|_| ()),
        db.count(None, CountOp::filter(filter("color"), BUDGET))
            .map(|_| ()),
    ];
    for call in calls {
        unchanged(&call, None);
        assert!(matches!(call.result, Err(ApiError::Internal { .. })));
    }
}

#[test]
fn result_and_error_mapping_preserve_the_receipt() {
    let mut receipt = Receipt::default();
    receipt.cost = 12;
    receipt.priced_at = Some(CommitId::new(3));
    let success = Metered::new(Ok(4), receipt.clone()).map(|n| n * 2);
    assert_eq!(success.receipt, receipt);
    assert_eq!(success.into_result().unwrap(), 8);
    let failure: Metered<()> = Metered::new(Err(ApiError::NotFound), receipt.clone());
    let mapped = failure.map_err(|_| ApiError::AlreadyExists);
    assert_eq!(mapped.receipt, receipt);
    assert!(matches!(mapped.into_result(), Err(ApiError::AlreadyExists)));
}
