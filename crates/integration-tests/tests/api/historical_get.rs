//! Public historical reads: identity, retention, current prices and upfront budgets.
use golemdb_api::*;
use golemdb_branch::Branches;
use golemdb_cells::{CellKey, CellNameRef, system, tables};
use golemdb_history::changeset_key;
use golemdb_merkle::Keccak256Hasher;
use golemdb_record::CommittedRead;
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;

const KEY: RecordKey = RecordKey([0x42; 32]);
const UNLIMITED: Budget = Budget::Unlimited;

fn config() -> OpenConfig {
    let mut prices = PricingConfig::zero();
    for (name, price) in [
        ("read.admission.base", 2),
        ("read.admission.entry", 5),
        ("read.admission.byte", 1),
        ("read.base", 3),
        ("read.key", 7),
        ("read.cell", 11),
        ("read.cell_scan", 13),
        ("read.byte", 2),
        ("read.commit_set", 17),
    ] {
        prices.weights.insert(name.into(), price);
    }
    OpenConfig::new(Genesis::new(
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
    ))
}

fn get(db: &Database, target: u64) -> Metered<Record> {
    db.get(
        ReadTarget::Commit(CommitId::new(target)),
        RecordOp::get(KEY, UNLIMITED),
    )
}

fn creation(db: &Database) -> CommitId {
    let b = db.begin().unwrap();
    db.create(
        b,
        RecordOp::create(KEY, UNLIMITED).field("a", CellValue::from_u64(7)),
    )
    .result
    .unwrap();
    db.commit(b).unwrap()
}

fn lifecycle(db: Database) {
    assert!(matches!(get(&db, 0).result, Err(ApiError::NotFound)));
    let first = creation(&db);
    let original = get(&db, first.get()).result.unwrap();
    let b = db.begin().unwrap();
    db.patch(
        b,
        RecordOp::patch(KEY, UNLIMITED)
            .attribute("a", CellValue::from_str("changed"))
            .field("b", CellValue::from_bytes(&[1, 2, 3])),
    )
    .result
    .unwrap();
    let second = db.commit(b).unwrap();
    let patched = get(&db, second.get()).result.unwrap();
    let b = db.begin().unwrap();
    db.delete(b, RecordOp::delete(KEY, UNLIMITED))
        .result
        .unwrap();
    let deleted = db.commit(b).unwrap();
    let b = db.begin().unwrap();
    db.create(b, RecordOp::create(KEY, UNLIMITED))
        .result
        .unwrap();
    let recreated = db.commit(b).unwrap();
    let empty = get(&db, recreated.get()).result.unwrap();
    let head = db.commit(db.begin().unwrap()).unwrap();

    for (target, expected) in [(first, original), (second, patched), (recreated, empty)] {
        let actual = get(&db, target.get());
        assert_eq!(actual.receipt.priced_at, Some(head));
        assert_eq!(actual.receipt.details, Details::default());
        assert_eq!(actual.result.unwrap(), expected);
        let projection = db.get(
            ReadTarget::Commit(target),
            RecordOp::get(KEY, UNLIMITED).only(["a", "missing", "a", "#meta"]),
        );
        let projected = projection.result.unwrap();
        assert_eq!(
            projected.cells.get(b"a".as_slice()),
            expected.cells.get(b"a".as_slice())
        );
        assert_eq!(
            projected.cells[b"#meta".as_slice()],
            expected.cells[b"#meta".as_slice()]
        );
        assert!(!projected.cells.contains_key(b"missing".as_slice()));
        assert_eq!(projection.receipt.ledger.count("read.admission.entry"), 4);
        assert!(
            db.get(
                ReadTarget::Commit(target),
                RecordOp::get(KEY, UNLIMITED).only(std::iter::empty::<&str>())
            )
            .result
            .unwrap()
            .cells
            .is_empty()
        );
    }
    for target in [CommitId::GENESIS, deleted] {
        for request in [
            RecordOp::get(KEY, UNLIMITED),
            RecordOp::get(KEY, UNLIMITED).only([] as [&str; 0]),
        ] {
            let missing = db.get(ReadTarget::Commit(target), request);
            assert!(matches!(missing.result, Err(ApiError::NotFound)));
            assert_eq!(missing.receipt.priced_at, Some(head));
        }
    }
    // Bindings use raw binary names and must select the old/new internal identity.
    for (target, id) in [(first, 64), (recreated, 65)] {
        let bindings = db.get(
            ReadTarget::Commit(target),
            RecordOp::get(RecordKey(system::RECORD_KEYS.key), UNLIMITED).only([KEY.0]),
        );
        assert_eq!(
            bindings.result.unwrap().cells[KEY.0.as_slice()].as_u64(),
            Some(id)
        );
        let all = db.get(
            ReadTarget::Commit(target),
            RecordOp::get(RecordKey(system::RECORD_KEYS.key), UNLIMITED),
        );
        assert_eq!(
            all.result.unwrap().cells[KEY.0.as_slice()].as_u64(),
            Some(id)
        );
    }
    let allocator = db.get(
        ReadTarget::Commit(CommitId::GENESIS),
        RecordOp::get(RecordKey(system::ALLOC.key), UNLIMITED),
    );
    assert_eq!(
        allocator.result.unwrap().cells[b"#nextRecordID".as_slice()].as_u64(),
        Some(64)
    );
}

#[test]
fn historical_get_preserves_records_across_retyping_deletion_and_recreation() {
    lifecycle(Database::open_memory(&config().genesis).unwrap());
    let dir = tempfile::tempdir().unwrap();
    lifecycle(Database::open(dir.path(), &config()).unwrap());
}

fn pricing_contract<S: Store + Clone + Send + Sync + 'static>(store: S) {
    let db = Database::from_store(store.clone(), &config()).unwrap();
    creation(&db);
    let b = db.begin().unwrap();
    let patch = ["b", "c", "d", "e"].into_iter().fold(
        RecordOp::patch(KEY, UNLIMITED).field("a", CellValue::from_u64(8)),
        |patch, name| patch.field(name, CellValue::from_u64(9)),
    );
    db.patch(b, patch).result.unwrap();
    let head = db.commit(b).unwrap();
    let tx = store.begin_read().unwrap();
    let bitmap_bytes = |id, name: &[u8]| {
        tx.get(
            tables::CELL_HISTORY,
            &CellKey::new(id, CellNameRef::raw(name)).encode(),
        )
        .unwrap()
        .unwrap()
        .len() as u64
    };
    let binding_bytes = bitmap_bytes(system::RECORD_KEYS.id, &KEY.0);
    let a_bytes = bitmap_bytes(64, b"a");
    // Admission (including the projection), base, binding discovery/selection,
    // cell discovery/selection, then 8-byte ID + name + tagged u64.
    let charges = [
        2,
        6,
        3,
        11,
        17 + binding_bytes * 2,
        7,
        11,
        17 + a_bytes * 2,
        11,
        18 * 2,
    ];
    let total: u64 = charges.iter().sum();
    for budget in 0..=total {
        let result = db.get(
            ReadTarget::Commit(CommitId::new(1)),
            RecordOp::get(KEY, Budget::Limited(budget)).only(["a"]),
        );
        let mut spent = 0;
        for charge in charges {
            if spent + charge > budget {
                break;
            }
            spent += charge;
        }
        assert_eq!(result.receipt.cost, spent, "budget {budget}");
        assert_eq!(result.receipt.priced_at, Some(head));
        assert_eq!(result.receipt.details, Details::default());
        if budget == total {
            assert_eq!(
                result.result.unwrap().cells[b"a".as_slice()].as_u64(),
                Some(7)
            );
            assert_eq!(result.receipt.ledger.count("read.cell"), 3);
            assert_eq!(result.receipt.ledger.count("read.key"), 1);
            assert_eq!(result.receipt.ledger.count("read.commit_set"), 2);
            assert_eq!(
                result.receipt.ledger.count("read.byte"),
                binding_bytes + a_bytes + 18
            );
        } else {
            assert!(matches!(
                result.result,
                Err(ApiError::OutOfBudget { required: None })
            ));
        }
    }
    let full = get(&db, 1);
    let user_and_system_bitmap_bytes: u64 =
        [b"#key".as_slice(), b"#meta", b"a", b"b", b"c", b"d", b"e"]
            .into_iter()
            .map(|name| bitmap_bytes(64, name))
            .sum();
    // Only a exists at T, but both scanners traverse five names and an end probe.
    assert_eq!(full.result.as_ref().unwrap().cells.len(), 3);
    assert_eq!(full.receipt.ledger.count("read.cell_scan"), 12);
    assert_eq!(full.receipt.ledger.count("read.cell"), 10);
    assert_eq!(full.receipt.ledger.count("read.commit_set"), 8);
    assert_eq!(
        full.receipt.ledger.count("read.byte"),
        18 + 91 + binding_bytes + user_and_system_bitmap_bytes
    );
    let weights = db.model_weights(None, None).unwrap();
    assert_eq!(
        full.receipt.cost,
        full.receipt
            .ledger
            .iter()
            .map(|(name, count)| weights.weights[name.as_str()] * count)
            .sum::<u64>()
    );
    for budget in [full.receipt.cost - 1, full.receipt.cost] {
        let result = db.get(
            ReadTarget::Commit(CommitId::new(1)),
            RecordOp::get(KEY, Budget::Limited(budget)),
        );
        assert_eq!(result.result.is_ok(), budget == full.receipt.cost);
        assert_eq!(result.receipt.priced_at, Some(head));
    }
    let live = db.get(ReadTarget::Head, RecordOp::get(KEY, UNLIMITED));
    let explicit = get(&db, head.get());
    assert_eq!(live.result.unwrap(), explicit.result.unwrap());
    assert_eq!(live.receipt, explicit.receipt);
    assert_eq!(live.receipt.ledger.count("read.commit_set"), 0);
}

#[test]
fn historical_get_charges_exact_steps_and_stops_before_each_unaffordable_package() {
    pricing_contract(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    pricing_contract(MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn target_validation_precedes_resolution_and_preserves_the_snapshot_head() {
    for window in [None, Some(0), Some(2), Some(20)] {
        let store = MemoryStore::new();
        let mut cfg = config();
        cfg.pruning = window.map(PruningConfig::new);
        let db = Database::from_store(store.clone(), &cfg).unwrap();
        for _ in 0..5 {
            db.commit(db.begin().unwrap()).unwrap();
        }
        for target in 0..=6 {
            let call = db.get(
                ReadTarget::Commit(CommitId::new(target)),
                RecordOp::get(RecordKey(system::PARAMS.key), UNLIMITED).only([] as [&str; 0]),
            );
            assert_eq!(call.receipt.priced_at, Some(CommitId::new(5)));
            let earliest = window.map_or(0, |w| 5u64.saturating_sub(w));
            if target > 5 {
                assert!(
                    matches!(call.result, Err(ApiError::CommitInFuture { requested, head }) if requested.get() == target && head.get() == 5)
                );
                assert!(call.receipt.ledger.is_empty());
            } else if target < earliest {
                assert!(
                    matches!(call.result, Err(ApiError::CommitOutsideHistory { requested, earliest: e, head }) if requested.get() == target && e.get() == earliest && head.get() == 5)
                );
                assert!(call.receipt.ledger.is_empty());
            } else {
                call.result.unwrap();
            }
        }
        // Fixture for previously cleaned history; the private metadata is never
        // changed by the public read path, even if retention is later widened.
        let mut tx = store.begin_write().unwrap();
        tx.put(Table("Superblock"), b"history_floor", &4u64.to_be_bytes())
            .unwrap();
        tx.commit().unwrap();
        let outside = get(&db, 3);
        assert!(
            matches!(outside.result, Err(ApiError::CommitOutsideHistory { earliest, .. }) if earliest.get() == if window == Some(0) { 5 } else { 4 })
        );
        assert!(outside.receipt.ledger.is_empty());
        for bytes in [None, Some(vec![0; 7]), Some(6u64.to_be_bytes().to_vec())] {
            let mut tx = store.begin_write().unwrap();
            if let Some(bytes) = bytes {
                tx.put(Table("Superblock"), b"history_floor", &bytes)
                    .unwrap();
            } else {
                tx.delete(Table("Superblock"), b"history_floor").unwrap();
            }
            tx.commit().unwrap();
            let invalid = get(&db, 0);
            assert!(matches!(
                invalid.result,
                Err(ApiError::InvalidHistoryMetadata { .. })
            ));
            assert_eq!(invalid.receipt.priced_at, Some(CommitId::new(5)));
            assert_eq!(invalid.receipt.cost, 0);
        }
    }
}

#[test]
fn committed_context_keeps_head_target_and_coverage_in_one_snapshot() {
    let store = MemoryStore::new();
    let db = Database::from_store(store.clone(), &config()).unwrap();
    creation(&db);
    db.commit(db.begin().unwrap()).unwrap();
    let mut context = CommittedRead::new(store.begin_read().unwrap()).unwrap();
    let b = db.begin().unwrap();
    db.delete(b, RecordOp::delete(KEY, UNLIMITED))
        .result
        .unwrap();
    db.commit(b).unwrap();
    let mut tx = store.begin_write().unwrap();
    tx.put(Table("Superblock"), b"history_floor", &3u64.to_be_bytes())
        .unwrap();
    tx.commit().unwrap();
    context.select_target(Some(CommitId::new(1)), None).unwrap();
    assert_eq!(context.head().get(), 2);
    assert_eq!(context.target().get(), 1);
    let record = golemdb_record::read_committed_by_id(&context.cells(), 64, None).unwrap();
    assert_eq!(record.cells[b"a".as_slice()].as_u64(), Some(7));
    assert!(matches!(
        get(&db, 1).result,
        Err(ApiError::CommitOutsideHistory { .. })
    ));
}

fn current_prices_contract<S: Store + Clone + Send + Sync + 'static>(store: S) {
    let cfg = config();
    let db = Database::from_store(store.clone(), &cfg).unwrap();
    creation(&db);
    drop(db);
    // Simulate a committed admin update, then reload the provider as required
    // by its existing contract. No runtime admin-refresh feature is introduced.
    let branches = Branches::new(store.clone(), Keccak256Hasher, None, None).unwrap();
    let b = branches.begin().unwrap();
    let name = [1u64.to_be_bytes().as_slice(), b"read.admission.base"].concat();
    branches
        .write(b, |writer| {
            writer.put(
                CellKey::new(system::MODEL_WEIGHT.id, CellNameRef::raw(&name)),
                CellValue::from_u64(101),
            );
            Ok::<_, golemdb_branch::BranchError>(())
        })
        .unwrap();
    let head = branches.commit(b).unwrap();
    drop(branches);
    let db = Database::from_store(store, &cfg).unwrap();
    let failure = db.get(
        ReadTarget::Commit(CommitId::new(1)),
        RecordOp::get(KEY, Budget::Limited(100)),
    );
    assert!(matches!(failure.result, Err(ApiError::OutOfBudget { .. })));
    assert_eq!(failure.receipt.cost, 0);
    assert_eq!(failure.receipt.priced_at, Some(head));
    let result = get(&db, 1);
    assert_eq!(result.receipt.priced_at, Some(head));
    assert_eq!(
        result.result.unwrap().cells[b"a".as_slice()].as_u64(),
        Some(7)
    );
    // Historical configuration data still returns its old value at current prices.
    let old_weight = db.get(
        ReadTarget::Commit(CommitId::new(1)),
        RecordOp::get(RecordKey(system::MODEL_WEIGHT.key), UNLIMITED).only([name.as_slice()]),
    );
    assert_eq!(
        old_weight.result.unwrap().cells[name.as_slice()].as_u64(),
        Some(2)
    );
    assert!(old_weight.receipt.cost >= 101);
}

#[test]
fn historical_data_uses_current_prices_after_reopening_at_a_later_head() {
    current_prices_contract(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    current_prices_contract(MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn historical_metadata_mismatch_and_missing_preimage_are_errors_not_partial_records() {
    let store = MemoryStore::new();
    let db = Database::from_store(store.clone(), &config()).unwrap();
    creation(&db);
    let b = db.begin().unwrap();
    db.patch(
        b,
        RecordOp::patch(KEY, UNLIMITED).field("b", CellValue::from_u64(9)),
    )
    .result
    .unwrap();
    db.commit(b).unwrap();
    let key = changeset_key(2, &CellKey::new(64, CellNameRef::raw(b"#meta")).encode());
    for meta in [
        RecordMeta::default(),
        RecordMeta {
            cells: 2,
            cell_bytes: 36,
            ..Default::default()
        },
        RecordMeta {
            cells: 1,
            cell_bytes: 17,
            ..Default::default()
        },
        RecordMeta {
            cells: 1,
            cell_bytes: 19,
            ..Default::default()
        },
    ] {
        let mut tx = store.begin_write().unwrap();
        tx.put(tables::CELL_CHANGE_SET, &key, meta.encode().encoded_bytes())
            .unwrap();
        tx.commit().unwrap();
        let result = get(&db, 1);
        assert!(matches!(result.result, Err(ApiError::Internal { .. })));
        assert!(result.receipt.cost > 0);
        assert_eq!(result.receipt.priced_at, Some(CommitId::new(2)));
    }
    let mut tx = store.begin_write().unwrap();
    tx.delete(tables::CELL_CHANGE_SET, &key).unwrap();
    tx.commit().unwrap();
    assert!(matches!(get(&db, 1).result, Err(ApiError::Internal { .. })));
}

#[test]
fn explicit_head_uses_live_prices_without_a_commit_set_weight() {
    let mut cfg = config();
    cfg.genesis.metering.weights.remove("read.commit_set");
    let db = Database::open_memory(&cfg.genesis).unwrap();
    let head = creation(&db);
    let live = db.get(ReadTarget::Head, RecordOp::get(KEY, UNLIMITED));
    let explicit = get(&db, head.get());
    assert_eq!(live.result.unwrap(), explicit.result.unwrap());
    assert_eq!(live.receipt, explicit.receipt);
    let history = get(&db, 0);
    assert!(matches!(history.result, Err(ApiError::OutOfBudget { .. })));
    assert_eq!(history.receipt.ledger.count("read.commit_set"), 0);
}

#[test]
fn historical_bindings_and_returned_identity_are_validated() {
    let store = MemoryStore::new();
    let db = Database::from_store(store.clone(), &config()).unwrap();
    creation(&db);
    let b = db.begin().unwrap();
    db.delete(b, RecordOp::delete(KEY, UNLIMITED))
        .result
        .unwrap();
    db.commit(b).unwrap();
    let binding = changeset_key(
        2,
        &CellKey::new(system::RECORD_KEYS.id, CellNameRef::raw(&KEY.0)).encode(),
    );
    let identity = changeset_key(2, &CellKey::new(64, CellNameRef::raw(b"#key")).encode());
    let mut tx = store.begin_write().unwrap();
    tx.put(
        tables::CELL_CHANGE_SET,
        &binding,
        CellValue::from_u64(0).encoded_bytes(),
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(matches!(get(&db, 1).result, Err(ApiError::Internal { .. })));

    let mut tx = store.begin_write().unwrap();
    tx.put(
        tables::CELL_CHANGE_SET,
        &binding,
        CellValue::from_u64(64).encoded_bytes(),
    )
    .unwrap();
    tx.put(
        tables::CELL_CHANGE_SET,
        &identity,
        CellValue::from_bytes32([9; 32]).encoded_bytes(),
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(matches!(get(&db, 1).result, Err(ApiError::Internal { .. })));
    // Preserve projection semantics: an existence-only read resolves the binding,
    // and never performs an uncharged identity audit.
    let exists = db.get(
        ReadTarget::Commit(CommitId::new(1)),
        RecordOp::get(KEY, UNLIMITED).only([] as [&str; 0]),
    );
    assert!(exists.result.unwrap().cells.is_empty());
}
