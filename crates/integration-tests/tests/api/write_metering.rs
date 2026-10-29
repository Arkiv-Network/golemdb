//! Public API conformance with persisted, nonzero prices. Expected totals below
//! are hand-counted at the temporary depth 6, not obtained by executing an estimate.
use golemdb_api::*;
use golemdb_cells::system;

const KEY: RecordKey = RecordKey([0x42; 32]);
const OTHER: RecordKey = RecordKey([0x43; 32]);

fn genesis() -> Genesis {
    let mut pricing = PricingConfig::zero();
    for (name, value) in [
        ("write.admission.base", 2),
        ("write.admission.entry", 1),
        ("write.admission.byte", 1),
        ("read.key", 1),
        ("write.record.create", 2),
        ("write.record.patch", 2),
        ("write.record.delete", 2),
        ("read.cell", 2),
        ("read.term", 2),
        ("write.cell.create", 5),
        ("write.cell.update", 6),
        ("write.cell.delete", 4),
        ("write.cell_trie_update", 1),
        ("write.cell_byte", 1),
        ("write.index.join", 3),
        ("write.index.leave", 2),
        ("write.index_trie_update", 1),
        ("write.index_term_create", 7),
        ("write.index_byte", 1),
    ] {
        pricing.weights.insert(name.into(), value);
    }
    Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
        pricing,
    )
}

fn each_backend(mut genesis: Genesis, test: impl Fn(Database)) {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        genesis.hash_function = hash;
        test(Database::open_memory(&genesis).unwrap());
        let dir = tempfile::tempdir().unwrap();
        test(Database::open(dir.path(), &OpenConfig::new(genesis.clone())).unwrap());
    }
}

fn create(key: RecordKey, budget: Budget) -> RecordOp<op::Create> {
    RecordOp::create(key, budget)
        .field("age", CellValue::from_u32(7))
        .attribute("color", CellValue::from_str("red"))
}

fn record(db: &Database, branch: BranchId, key: RecordKey) -> Record {
    db.get(
        ReadTarget::Branch(branch),
        RecordOp::get(key, Budget::Limited(0)),
    )
    .result
    .unwrap()
}

fn accounting(db: &Database, receipt: &Receipt, cost: u64, commit: CommitId) {
    assert_eq!(receipt.cost, cost);
    assert_eq!(receipt.priced_at, Some(commit));
    let weights = db.model_weights(Some(commit), None).unwrap();
    let reconstructed: u64 = receipt
        .ledger
        .iter()
        .map(|(name, count)| weights.weights[name.as_str()] * count)
        .sum();
    assert_eq!(reconstructed, cost);
}

fn refused<T>(call: &Metered<T>, spent: u64, required: Option<u64>) {
    assert!(
        matches!(&call.result, Err(ApiError::OutOfBudget { required: actual }) if *actual == required)
    );
    assert_eq!(call.receipt.cost, spent);
    assert_eq!(call.receipt.details, Details::default());
    for name in [
        "write.cell.create",
        "write.cell.update",
        "write.cell.delete",
        "write.index.join",
        "write.index.leave",
    ] {
        assert_eq!(call.receipt.ledger.count(name), 0);
    }
}

#[test]
fn cud_exact_budgets_preserve_state_and_return_only_completed_work_on_failure() {
    each_backend(genesis(), |db| {
        let branch = db.begin().unwrap();
        let head = db.head().unwrap();
        let alloc = record(&db, branch, RecordKey(system::ALLOC.key));
        let info = db.branch_info(branch).unwrap();
        for (budget, spent, required) in [
            (1, 0, None),
            (18, 11, None),
            (23, 22, None),
            (24, 24, Some(278)),
            (277, 24, Some(278)),
        ] {
            let call = db.create(branch, create(KEY, Budget::Limited(budget)));
            refused(&call, spent, required);
            accounting(&db, &call.receipt, spent, head);
            assert_eq!(db.branch_info(branch).unwrap(), info);
            assert_eq!(record(&db, branch, RecordKey(system::ALLOC.key)), alloc);
            assert!(matches!(
                db.get(
                    ReadTarget::Branch(branch),
                    RecordOp::get(KEY, Budget::Limited(0))
                )
                .result,
                Err(ApiError::NotFound)
            ));
        }
        let created = db.create(branch, create(KEY, Budget::Limited(278)));
        created.result.unwrap();
        accounting(&db, &created.receipt, 278, head);
        assert_eq!(created.receipt.ledger.count("write.cell.create"), 5);
        assert_eq!(created.receipt.ledger.count("write.cell_trie_update"), 30);
        assert_eq!(created.receipt.ledger.count("write.cell_byte"), 173);
        assert_eq!(created.receipt.details.cells_created, 2);
        assert_eq!(created.receipt.details.cell_bytes_written, 33);
        let before = record(&db, branch, KEY);
        let info = db.branch_info(branch).unwrap();
        for (budget, spent, required) in [(18, 17, None), (46, 19, Some(47))] {
            let call = db.patch(
                branch,
                RecordOp::patch(KEY, Budget::Limited(budget)).set("age", CellValue::from_u32(8)),
            );
            refused(&call, spent, required);
            accounting(&db, &call.receipt, spent, head);
            assert_eq!(record(&db, branch, KEY), before);
            assert_eq!(db.branch_info(branch).unwrap(), info);
        }
        let patched = db.patch(
            branch,
            RecordOp::patch(KEY, Budget::Limited(47)).set("age", CellValue::from_u32(8)),
        );
        patched.result.unwrap();
        accounting(&db, &patched.receipt, 47, head);
        assert_eq!(patched.receipt.details.cells_updated, 1);

        // Exercise deletion over committed cells, including rollback of its scan state.
        let head = db.commit(branch).unwrap();
        let branch = db.begin().unwrap();
        let before = record(&db, branch, KEY);
        let info = db.branch_info(branch).unwrap();
        for (budget, spent, required) in [(14, 13, None), (72, 15, Some(73))] {
            let call = db.delete(branch, RecordOp::delete(KEY, Budget::Limited(budget)));
            refused(&call, spent, required);
            accounting(&db, &call.receipt, spent, head);
            assert_eq!(record(&db, branch, KEY), before);
            assert_eq!(db.branch_info(branch).unwrap(), info);
        }
        let deleted = db.delete(branch, RecordOp::delete(KEY, Budget::Limited(73)));
        deleted.result.unwrap();
        accounting(&db, &deleted.receipt, 73, head);
        assert_eq!(deleted.receipt.details.cells_deleted, 2);
        assert_eq!(deleted.receipt.ledger.count("write.cell_byte"), 0);
        let receipt = deleted.receipt.clone();
        db.rollback(branch).unwrap();
        assert_eq!(record(&db, branch, KEY), before);
        assert_eq!(deleted.receipt, receipt);
        db.discard(branch).unwrap();
        assert_eq!(deleted.receipt, receipt);
    });
}

#[test]
fn invalid_input_noops_and_invalid_handles_have_their_defined_charges() {
    each_backend(genesis(), |db| {
        let branch = db.begin().unwrap();
        let head = db.head().unwrap();
        let invalid = db.create(
            branch,
            RecordOp::create(KEY, Budget::Unlimited).field("bad name", CellValue::from_u32(1)),
        );
        assert!(matches!(
            invalid.result,
            Err(ApiError::InvalidArgument { .. })
        ));
        accounting(&db, &invalid.receipt, 15, head);
        assert_eq!(invalid.receipt.ledger.count("read.key"), 0);
        assert_eq!(invalid.receipt.details, Details::default());
        assert_eq!(db.branch_info(branch).unwrap().version, 0);
        let missing = db.delete(branch, RecordOp::delete(KEY, Budget::Unlimited));
        assert!(matches!(missing.result, Err(ApiError::NotFound)));
        accounting(&db, &missing.receipt, 5, head);
        db.create(branch, create(KEY, Budget::Unlimited))
            .result
            .unwrap();
        let duplicate = db.create(branch, create(KEY, Budget::Unlimited));
        assert!(matches!(duplicate.result, Err(ApiError::AlreadyExists)));
        accounting(&db, &duplicate.receipt, 22, head);
        assert_eq!(duplicate.receipt.details, Details::default());
        let before = record(&db, branch, KEY);
        let info = db.branch_info(branch).unwrap();
        let noop = db.patch(
            branch,
            RecordOp::patch(KEY, Budget::Unlimited).set("age", CellValue::from_u32(7)),
        );
        noop.result.unwrap();
        accounting(&db, &noop.receipt, 19, head);
        assert_eq!(noop.receipt.details, Details::default());
        assert_eq!(noop.receipt.ledger.count("write.cell.update"), 0);
        let missing = db.patch(
            branch,
            RecordOp::patch(KEY, Budget::Unlimited).set("absent", CellValue::from_u32(1)),
        );
        assert!(matches!(missing.result, Err(ApiError::CellNotFound { .. })));
        accounting(&db, &missing.receipt, 22, head);
        assert_eq!(missing.receipt.details, Details::default());
        assert_eq!(record(&db, branch, KEY), before);
        assert_eq!(db.branch_info(branch).unwrap(), info);

        let changed = db.patch(
            branch,
            RecordOp::patch(KEY, Budget::Unlimited).set("color", CellValue::from_str("blue")),
        );
        changed.result.unwrap();
        accounting(&db, &changed.receipt, 148, head);
        assert_eq!(changed.receipt.ledger.count("write.index.leave"), 1);
        assert_eq!(changed.receipt.ledger.count("write.index.join"), 1);
        assert!(record(&db, branch, KEY).cells[b"color".as_slice()].is_indexable());

        db.seal(branch).unwrap();
        let sealed = db.create(branch, create(OTHER, Budget::Unlimited));
        assert!(matches!(sealed.result, Err(ApiError::Sealed)));
        assert_eq!(sealed.receipt, Receipt::default());
        let stale = db.begin().unwrap();
        db.commit(branch).unwrap();
        for branch in [branch, stale] {
            let invalid = db.create(branch, create(OTHER, Budget::Unlimited));
            assert!(matches!(invalid.result, Err(ApiError::HandleInvalid)));
            assert_eq!(invalid.receipt, Receipt::default());
        }
    });
}

#[test]
fn missing_weights_and_overflow_fail_under_limited_and_unlimited_budgets() {
    for (weight, value, spent) in [
        ("write.admission.byte", None, 3),
        ("write.cell.create", None, 22),
        ("write.cell_byte", Some(u64::MAX), 22),
        ("write.admission.byte", Some(u64::MAX), 3),
    ] {
        let mut config = genesis();
        match value {
            Some(value) => {
                config.metering.weights.insert(weight.into(), value);
            }
            None => {
                config.metering.weights.remove(weight);
            }
        }
        each_backend(config, |db| {
            let branch = db.begin().unwrap();
            let alloc = record(&db, branch, RecordKey(system::ALLOC.key));
            for budget in [Budget::Limited(u64::MAX), Budget::Unlimited] {
                let call = db.create(branch, create(KEY, budget));
                refused(&call, spent, None);
                accounting(&db, &call.receipt, spent, CommitId::GENESIS);
                assert_eq!(record(&db, branch, RecordKey(system::ALLOC.key)), alloc);
                assert_eq!(db.branch_info(branch).unwrap().version, 0);
            }
        });
    }
    let mut config = genesis();
    config.metering.weights.remove("write.cell.delete");
    each_backend(config, |db| {
        let branch = db.begin().unwrap();
        db.create(branch, create(KEY, Budget::Limited(278)))
            .result
            .unwrap();
        let before = record(&db, branch, KEY);
        let call = db.delete(branch, RecordOp::delete(KEY, Budget::Unlimited));
        refused(&call, 11, None);
        assert_eq!(record(&db, branch, KEY), before);
    });
}

#[test]
fn record_caps_reject_the_plan_without_allocating_or_applying_it() {
    let mut config = genesis();
    config.record_limits.max_cells = 1;
    each_backend(config, |db| {
        let branch = db.begin().unwrap();
        let alloc = record(&db, branch, RecordKey(system::ALLOC.key));
        let call = db.create(branch, create(KEY, Budget::Unlimited));
        assert!(matches!(call.result, Err(ApiError::InvalidArgument { .. })));
        accounting(&db, &call.receipt, 24, CommitId::GENESIS);
        assert_eq!(call.receipt.details, Details::default());
        assert_eq!(call.receipt.ledger.count("write.cell.create"), 0);
        assert_eq!(record(&db, branch, RecordKey(system::ALLOC.key)), alloc);
        assert_eq!(db.branch_info(branch).unwrap().version, 0);
    });
}

#[test]
fn term_creation_uses_committed_origin_across_repeated_joins_and_recreation() {
    each_backend(genesis(), |db| {
        let branch = db.begin().unwrap();
        for key in [KEY, OTHER] {
            let call = db.create(branch, create(key, Budget::Unlimited));
            call.result.unwrap();
            accounting(&db, &call.receipt, 278, CommitId::GENESIS);
            assert_eq!(call.receipt.ledger.count("write.index_term_create"), 1);
        }
        db.delete(branch, RecordOp::delete(KEY, Budget::Unlimited))
            .result
            .unwrap();
        let recreated = db.create(branch, create(KEY, Budget::Unlimited));
        recreated.result.unwrap();
        accounting(&db, &recreated.receipt, 278, CommitId::GENESIS);
        db.commit(branch).unwrap();
        let head = db.head().unwrap();
        let branch = db.begin().unwrap();
        for key in [KEY, OTHER] {
            db.delete(branch, RecordOp::delete(key, Budget::Unlimited))
                .result
                .unwrap();
        }
        let recreated = db.create(branch, create(KEY, Budget::Unlimited));
        recreated.result.unwrap();
        accounting(&db, &recreated.receipt, 271, head);
        assert_eq!(recreated.receipt.ledger.count("write.index_term_create"), 0);
    });
}

#[test]
fn public_reads_remain_unmetered_with_nonzero_write_and_planning_read_prices() {
    each_backend(genesis(), |db| {
        let branch = db.begin().unwrap();
        db.create(branch, create(KEY, Budget::Unlimited))
            .result
            .unwrap();
        let head = db.commit(branch).unwrap();
        let get = db.get(ReadTarget::Head, RecordOp::get(KEY, Budget::Limited(0)));
        get.result.unwrap();
        let filter = Filter::Eq {
            field: CellNameRef::raw(b"color").into(),
            value: EqValue::try_from(CellValue::from_str("red")).unwrap(),
        };
        let query = db.query(
            None,
            QueryOp::from_filter(filter.clone(), Budget::Limited(0)),
        );
        assert_eq!(query.result.unwrap().records.len(), 1);
        let count = db.count(None, CountOp::filter(filter, Budget::Limited(0)));
        assert_eq!(count.result.unwrap(), 1);
        for receipt in [get.receipt, query.receipt, count.receipt] {
            assert_eq!(receipt.priced_at, Some(head));
            assert_eq!(receipt.cost, 0);
            assert_eq!(receipt.details, Details::default());
            assert!(receipt.ledger.is_empty());
        }
    });
}
