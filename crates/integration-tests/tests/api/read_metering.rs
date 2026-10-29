//! Hand-counted get prices and stopping points, shared by both storage backends.
use golemdb_api::*;
use golemdb_cells::system;

const KEY: RecordKey = RecordKey([0x42; 32]);

fn genesis() -> Genesis {
    let mut prices = PricingConfig::zero();
    for (name, value) in [
        ("read.admission.base", 2),
        ("read.admission.entry", 5),
        ("read.admission.byte", 1),
        ("read.base", 3),
        ("read.key", 7),
        ("read.cell", 11),
        ("read.cell_scan", 13),
        ("read.byte", 2),
    ] {
        prices.weights.insert(name.into(), value);
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
        prices,
    )
}

fn each_backend(genesis: Genesis, run: impl Fn(Database)) {
    run(Database::open_memory(&genesis).unwrap());
    let dir = tempfile::tempdir().unwrap();
    run(Database::open(dir.path(), &OpenConfig::new(genesis)).unwrap());
}

fn create(db: &Database) -> BranchId {
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(KEY, Budget::Unlimited)
            .field("age", CellValue::from_u32(7))
            .attribute("color", CellValue::from_str("red")),
    )
    .result
    .unwrap();
    branch
}

fn check(call: &Metered<Record>, spent: u64, commit: CommitId, db: &Database) {
    assert_eq!(call.receipt.cost, spent);
    assert_eq!(call.receipt.priced_at, Some(commit));
    assert_eq!(call.receipt.details, Details::default());
    let weights = db.model_weights(None, None).unwrap();
    assert_eq!(
        spent,
        call.receipt
            .ledger
            .iter()
            .map(|(name, count)| weights.weights[name.as_str()] * count)
            .sum::<u64>()
    );
}

#[test]
fn get_shapes_and_budget_cutoffs_match_on_branches_and_committed_storage() {
    each_backend(genesis(), |db| {
        let branch = create(&db);
        let exercise = |target, commit| {
            // User cells occupy 16 + 17 bytes, and #key/#meta occupy 91.
            // Base = 2 + 3 + 7; full = base + 2*11 + 2*13 + 124*2 = 308.
            for (budget, spent) in [
                (0, 0),
                (1, 0),
                (2, 2),
                (4, 2),
                (5, 5),
                (11, 5),
                (12, 12),
                (22, 12),
                (23, 23),
                (33, 23),
                (34, 34),
                (307, 34),
                (308, 308),
            ] {
                let call = db.get(target, RecordOp::get(KEY, Budget::Limited(budget)));
                check(&call, spent, commit, &db);
                if budget == 308 {
                    assert_eq!(call.result.unwrap().cells.len(), 4);
                    assert_eq!(call.receipt.ledger.count("read.cell_scan"), 2);
                    assert_eq!(call.receipt.ledger.count("read.byte"), 124);
                } else {
                    assert!(matches!(
                        call.result,
                        Err(ApiError::OutOfBudget { required: None })
                    ));
                    assert_eq!(call.receipt.ledger.count("read.cell_scan"), 0);
                    assert_eq!(call.receipt.ledger.count("read.byte"), 0);
                }
            }
            // All three entries pay admission (28); only two distinct lookups.
            for (budget, spent) in [
                (7, 2),
                (29, 22),
                (50, 40),
                (51, 51),
                (82, 51),
                (83, 83),
                (93, 83),
                (94, 94),
            ] {
                let call = db.get(
                    target,
                    RecordOp::get(KEY, Budget::Limited(budget)).only(["age", "missing", "age"]),
                );
                check(&call, spent, commit, &db);
                if budget == 94 {
                    assert_eq!(call.result.unwrap().cells.len(), 1);
                    assert_eq!(call.receipt.ledger.count("read.cell"), 2);
                    assert_eq!(call.receipt.ledger.count("read.byte"), 16);
                } else {
                    assert!(matches!(
                        call.result,
                        Err(ApiError::OutOfBudget { required: None })
                    ));
                }
            }
            let exists = db.get(
                target,
                RecordOp::get(KEY, Budget::Limited(12)).only([] as [&str; 0]),
            );
            check(&exists, 12, commit, &db);
            assert!(exists.result.unwrap().cells.is_empty());
            assert_eq!(exists.receipt.ledger.count("read.cell"), 0);
            let absent = db.get(
                target,
                RecordOp::get(RecordKey([0x43; 32]), Budget::Unlimited),
            );
            check(&absent, 12, commit, &db);
            assert!(matches!(absent.result, Err(ApiError::NotFound)));
            // #alloc has #key (45 bytes) and #nextRecordID (30 bytes), no #meta.
            // Two rows plus the terminating probe: 12 + 3*13 + 75*2 = 201.
            for (budget, spent) in [
                (24, 12),
                (25, 25),
                (114, 25),
                (115, 115),
                (128, 128),
                (187, 128),
                (188, 188),
                (200, 188),
                (201, 201),
            ] {
                let call = db.get(
                    target,
                    RecordOp::get(RecordKey(system::ALLOC.key), Budget::Limited(budget)),
                );
                check(&call, spent, commit, &db);
                if budget == 201 {
                    assert_eq!(call.result.unwrap().cells.len(), 2);
                } else {
                    assert!(matches!(
                        call.result,
                        Err(ApiError::OutOfBudget { required: None })
                    ));
                }
            }
        };
        exercise(ReadTarget::Branch(branch), db.head().unwrap());
        db.seal(branch).unwrap();
        exercise(ReadTarget::Branch(branch), db.head().unwrap());
        let head = db.commit(branch).unwrap();
        exercise(ReadTarget::Head, head);
        let stale = db.get(
            ReadTarget::Branch(branch),
            RecordOp::get(KEY, Budget::Unlimited),
        );
        assert!(matches!(stale.result, Err(ApiError::HandleInvalid)));
        assert_eq!(stale.receipt, Receipt::default());
        let unavailable = db.get(
            ReadTarget::Commit(CommitId::GENESIS),
            RecordOp::get(KEY, Budget::Unlimited),
        );
        assert!(matches!(
            unavailable.result,
            Err(ApiError::CommitUnavailable { .. })
        ));
        check(&unavailable, 0, head, &db); // Existing target-before-admission order.

        // Reserved scans merge origin rows, overlay-only rows and both kinds
        // of tombstones. Hidden work is charged, but hidden bytes are not.
        let branch = db.begin().unwrap();
        db.delete(branch, RecordOp::delete(KEY, Budget::Unlimited))
            .result
            .unwrap();
        for byte in [0x11, 0x60, 0x77] {
            db.create(
                branch,
                RecordOp::create(RecordKey([byte; 32]), Budget::Unlimited),
            )
            .result
            .unwrap();
        }
        db.delete(
            branch,
            RecordOp::delete(RecordKey([0x60; 32]), Budget::Unlimited),
        )
        .result
        .unwrap();
        let bindings = || RecordOp::get(RecordKey(system::RECORD_KEYS.key), Budget::Unlimited);
        let pending = db.get(ReadTarget::Branch(branch), bindings());
        let cells = &pending.result.as_ref().unwrap().cells;
        assert!(cells.contains_key([0x11; 32].as_slice()));
        assert!(cells.contains_key([0x77; 32].as_slice()));
        assert!(!cells.contains_key(KEY.0.as_slice()));
        assert!(!cells.contains_key([0x60; 32].as_slice()));
        db.seal(branch).unwrap();
        db.commit(branch).unwrap();
        let committed = db.get(ReadTarget::Head, bindings());
        assert_eq!(pending.result.unwrap(), committed.result.unwrap());
        assert_eq!(
            pending.receipt.ledger.count("read.byte"),
            committed.receipt.ledger.count("read.byte")
        );
        assert_eq!(
            pending.receipt.ledger.count("read.cell_scan"),
            committed.receipt.ledger.count("read.cell_scan") + 2
        );
    });
}

#[test]
fn get_zero_missing_and_overflowing_prices_preserve_completed_accounting() {
    for missing in [true, false] {
        let mut config = genesis();
        if missing {
            config.metering.weights.remove("read.byte");
        } else {
            config.metering.weights.insert("read.byte".into(), u64::MAX);
        }
        each_backend(config, |db| {
            let branch = create(&db);
            for budget in [Budget::Limited(u64::MAX), Budget::Unlimited] {
                let call = db.get(
                    ReadTarget::Branch(branch),
                    RecordOp::get(KEY, budget).only(["age"]),
                );
                // Admission 10 + read.base 3 + read.key 7 + read.cell 11.
                check(&call, 31, db.head().unwrap(), &db);
                assert!(matches!(
                    call.result,
                    Err(ApiError::OutOfBudget { required: None })
                ));
                assert_eq!(call.receipt.ledger.count("read.byte"), 0);
                db.get(
                    ReadTarget::Branch(branch),
                    RecordOp::get(KEY, budget).only([] as [&str; 0]),
                )
                .result
                .unwrap();
            }
        });
    }
    let mut config = genesis();
    config.metering = PricingConfig::zero();
    each_backend(config, |db| {
        let branch = create(&db);
        let call = db.get(
            ReadTarget::Branch(branch),
            RecordOp::get(KEY, Budget::Limited(0)),
        );
        check(&call, 0, db.head().unwrap(), &db);
        assert_eq!(call.result.unwrap().cells.len(), 4);
        assert_eq!(call.receipt.ledger.count("read.cell_scan"), 2);
        assert_eq!(call.receipt.ledger.count("read.byte"), 124);
    });
}
