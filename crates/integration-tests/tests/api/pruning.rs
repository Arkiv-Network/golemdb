use golemdb_api::*;
use golemdb_cells::tables;
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};

const KEY: RecordKey = RecordKey([0x42; 32]);
const BUDGET: Budget = Budget::Limited(u64::MAX);
/// Past the wait after which rows go without a full batch.
const COMMITS: i32 = golemdb_pruning::MAX_WAIT_COMMITS as i32 + 10;

fn config() -> OpenConfig {
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
        0,
        None,
        PricingConfig::zero(),
    ))
}

fn count(db: &MemoryStore, table: Table) -> usize {
    let tx = db.begin_read().unwrap();
    scan_prefix(&tx, table, vec![]).unwrap().count()
}

/// Commits patching one record, past the wait before rows go without a full
/// batch: the run with pruning keeps only the window's `Roots` and fewer trie
/// rows, and both reach the same head.
#[test]
fn open_config_pruning_deletes_superseded_rows() {
    let run = |config: OpenConfig| {
        let db = MemoryStore::new();
        let api = Database::from_store(db.clone(), &config).unwrap();
        let branch = api.begin().unwrap();
        let create = RecordOp::create(KEY, BUDGET).attribute("price", CellValue::from_i32(0));
        api.create(branch, create).into_result().unwrap();
        api.commit(branch).unwrap();
        for price in 1..COMMITS {
            let branch = api.begin().unwrap();
            let patch = RecordOp::patch(KEY, BUDGET).attribute("price", CellValue::from_i32(price));
            api.patch(branch, patch).into_result().unwrap();
            api.commit(branch).unwrap();
        }
        let record = api.get(ReadTarget::Head, RecordOp::get(KEY, BUDGET));
        let price = record.into_result().unwrap().cells[b"price".as_slice()].as_i32();
        assert_eq!(price, Some(COMMITS - 1));
        (db, api.head().unwrap())
    };
    let (plain, plain_head) = run(config());
    let (pruned, pruned_head) = run(config().with_pruning(PruningConfig::new(2)));
    assert_eq!(pruned_head, plain_head);
    assert_eq!(count(&pruned, Table("Roots")), 3);
    let cells = tables::CELL_TRIE.table();
    assert!(count(&pruned, cells) < count(&plain, cells));
}

#[test]
fn minimum_retention_is_a_required_genesis_parameter_and_applies_before_creation() {
    let mut cfg = config();
    cfg.genesis.min_retention = 64;
    let too_short = cfg.clone().with_pruning(PruningConfig::new(63));
    let store = MemoryStore::new();
    assert!(matches!(
        Database::from_store(store.clone(), &too_short),
        Err(OpenError::InvalidConfig(_))
    ));
    assert!(store.begin_write().unwrap().is_empty().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new");
    assert!(matches!(
        Database::open(&path, &too_short),
        Err(OpenError::InvalidConfig(_))
    ));
    assert!(!path.exists());

    let db = Database::from_store(store.clone(), &cfg).unwrap();
    let params = db
        .get(
            ReadTarget::Head,
            RecordOp::get(RecordKey(golemdb_cells::system::PARAMS.key), BUDGET),
        )
        .into_result()
        .unwrap();
    assert_eq!(
        params.cells[b"#minRetention".as_slice()],
        CellValue::from_u64(64)
    );
    let genesis_id = db.info().genesis_id;
    drop(db);
    for window in [64, 256] {
        let db = Database::from_store(
            store.clone(),
            &cfg.clone().with_pruning(PruningConfig::new(window)),
        )
        .unwrap();
        assert_eq!(db.info().genesis_id, genesis_id);
    }
    assert!(matches!(
        Database::from_store(store.clone(), &too_short),
        Err(OpenError::InvalidConfig(_))
    ));
    assert!(matches!(
        Database::from_store(store, &config()),
        Err(OpenError::GenesisMismatch)
    ));
    assert_ne!(
        Database::open_memory(&config().genesis)
            .unwrap()
            .info()
            .genesis_id,
        genesis_id
    );
}

fn publication_sweep<S: Store + Clone + Send + Sync + 'static>(store: S) {
    use golemdb_branch::ROOTS;
    use golemdb_cells::{CellKey, CellNameRef, CellReader};
    use golemdb_history::changeset_key;
    use golemdb_index::tables::HISTORY as INDEX;

    let mut cfg = config();
    cfg.genesis.min_retention = 2;
    // Accumulate pre-images on an archival node, then enable finite retention.
    let db = Database::from_store(store.clone(), &cfg).unwrap();
    let key = CellKey::new(64, CellNameRef::raw(b"price"));
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(KEY, BUDGET).attribute("price", CellValue::from_i32(1)),
    )
    .into_result()
    .unwrap();
    db.commit(branch).unwrap();
    let cutoff = 1 + golemdb_pruning::MAX_WAIT_COMMITS;
    for _ in 2..=cutoff {
        db.commit(db.begin().unwrap()).unwrap();
    }
    let branch = db.begin().unwrap();
    db.patch(
        branch,
        RecordOp::patch(KEY, BUDGET).attribute("price", CellValue::from_i32(2)),
    )
    .into_result()
    .unwrap();
    db.commit(branch).unwrap();
    let get = |db: &Database, target| {
        db.get(
            ReadTarget::Commit(CommitId::new(target)),
            RecordOp::get(KEY, BUDGET),
        )
        .into_result()
    };
    let at_cutoff = get(&db, cutoff).unwrap();
    assert_eq!(get(&db, 1).unwrap(), at_cutoff);
    drop(db);
    cfg.pruning = Some(PruningConfig::new(2));
    let db = Database::from_store(store.clone(), &cfg).unwrap();

    // Cell cleanup is staged first. An unpaired index changeset must roll it back,
    // together with the head, roots, and floor. The same sealed branch can retry.
    let branch = db.begin().unwrap();
    db.seal(branch).unwrap();
    let bad = changeset_key(1, b"unpaired");
    let mut tx = store.begin_write().unwrap();
    tx.put(INDEX.changeset, &bad, b"").unwrap();
    tx.commit().unwrap();
    let old = store.begin_read().unwrap();
    assert!(matches!(db.commit(branch), Err(ApiError::Internal { .. })));
    assert_eq!(db.head().unwrap().get(), cutoff + 1);
    let fresh = store.begin_read().unwrap();
    let creation = changeset_key(1, &key.encode());
    assert!(
        fresh
            .get(tables::CELL_CHANGE_SET, &creation)
            .unwrap()
            .is_some()
    );
    golemdb_pruning::Pruner::validate_target(&fresh, cutoff + 1, 0, None).unwrap();
    drop(fresh);

    let mut tx = store.begin_write().unwrap();
    tx.delete(INDEX.changeset, &bad).unwrap();
    tx.commit().unwrap();
    assert_eq!(db.commit(branch).unwrap().get(), cutoff + 2);
    let fresh = store.begin_read().unwrap();
    assert_eq!(get(&db, cutoff).unwrap(), at_cutoff);
    assert!(
        matches!(get(&db, cutoff - 1), Err(ApiError::CommitOutsideHistory { earliest, .. }) if earliest.get() == cutoff)
    );
    assert!(
        old.get(tables::CELL_CHANGE_SET, &creation)
            .unwrap()
            .is_some()
    );
    assert!(
        fresh
            .get(tables::CELL_CHANGE_SET, &creation)
            .unwrap()
            .is_none()
    );
    assert!(
        fresh
            .get(
                tables::CELL_CHANGE_SET,
                &changeset_key(cutoff + 1, &key.encode())
            )
            .unwrap()
            .is_some()
    );
    assert!(fresh.get(ROOTS, &cutoff.to_be_bytes()).unwrap().is_some());
    assert!(
        fresh
            .get(ROOTS, &(cutoff - 1).to_be_bytes())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        CellReader::at(&old, 1).get(&key).unwrap().unwrap().as_i32(),
        Some(1)
    );
    assert_eq!(golemdb_branch::read_head(&old).unwrap().get(), cutoff + 1);
    drop(fresh);
    drop(old);
    drop(db);
    for local in [None, Some(256), Some(2)] {
        cfg.pruning = local.map(PruningConfig::new);
        let db = Database::from_store(store.clone(), &cfg).unwrap();
        assert_eq!(get(&db, cutoff).unwrap(), at_cutoff);
        assert!(
            matches!(get(&db, cutoff - 1), Err(ApiError::CommitOutsideHistory { earliest, .. }) if earliest.get() == cutoff)
        );
    }

    // Even with local pruning off, a floor inside the host's guaranteed window
    // makes this database invalid on reopen.
    let mut tx = store.begin_write().unwrap();
    tx.put(
        Table("Superblock"),
        b"history_floor",
        &(cutoff + 1).to_be_bytes(),
    )
    .unwrap();
    tx.commit().unwrap();
    cfg.pruning = None;
    assert!(matches!(
        Database::from_store(store, &cfg),
        Err(OpenError::Pruning(
            golemdb_pruning::PruningError::InvalidHistoryMetadata(_)
        ))
    ));
}

#[test]
fn publication_prunes_atomically_and_reopen_respects_the_physical_floor() {
    publication_sweep(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    publication_sweep(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
}
