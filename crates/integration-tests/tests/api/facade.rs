use std::sync::{Arc, Barrier};

use golemdb_api::*;
use golemdb_cells::system;
use golemdb_storage::MemoryStore;

const KEY: RecordKey = RecordKey([0x42; 32]);
const MISSING: RecordKey = RecordKey([0x43; 32]);

fn config(hash_function: HashAlgorithm) -> OpenConfig {
    OpenConfig::new(Genesis::new(
        hash_function,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
    ))
}

fn input(key: RecordKey, price: i32) -> RecordOp<op::Create> {
    RecordOp::create(key)
        .attribute("price", CellValue::from_i32(price))
        .unwrap()
        .field("description", CellValue::from_str("product"))
        .unwrap()
}

// Every contract is exercised through the same public handle on both hashes
// and both backends. A directory remains alive until all facade clones drop.
fn each_backend(contract: fn(Database)) {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        contract(Database::open_memory(&config(hash).genesis).unwrap());
        {
            let dir = tempfile::tempdir().unwrap();
            contract(Database::open(dir.path(), &config(hash)).unwrap());
        }
    }
}

fn get(api: &dyn Api, target: ReadTarget) -> Record {
    api.get(target, RecordOp::get(KEY)).unwrap()
}

fn allocator(api: &dyn Api, target: ReadTarget) -> u64 {
    api.get(
        target,
        RecordOp::get(RecordKey(system::ALLOC.key)).only(["#nextRecordID"]),
    )
    .unwrap()
    .cells[b"#nextRecordID".as_slice()]
    .as_u64()
    .unwrap()
}

#[test]
fn crud_pending_reads_projections_and_recreation() {
    each_backend(|db| {
        let api: Arc<dyn Api + Send + Sync> = Arc::new(db.clone());
        assert_eq!(api.head().unwrap(), golemdb_branch::CommitId::new(0));
        let branch = api.begin().unwrap();
        let other = api.begin().unwrap();
        assert_eq!(api.create(branch, input(KEY, 50)).unwrap(), KEY);
        assert!(matches!(
            api.create(branch, input(KEY, 60)),
            Err(ApiError::AlreadyExists)
        ));
        for target in [
            ReadTarget::Head,
            ReadTarget::Commit(golemdb_branch::CommitId::new(0)),
            ReadTarget::Branch(other),
        ] {
            assert!(matches!(
                api.get(target, RecordOp::get(KEY)),
                Err(ApiError::NotFound)
            ));
        }
        let pending = get(api.as_ref(), ReadTarget::Branch(branch));
        assert_eq!(pending.key, KEY);
        assert_eq!(pending.cells.len(), 3);
        assert_eq!(pending.cells[b"#key".as_slice()].as_bytes32(), Some(KEY.0));
        assert!(pending.cells[b"price".as_slice()].is_indexable());
        let selected = api
            .get(
                ReadTarget::Branch(branch),
                RecordOp::get(KEY).only(["price", "missing", "price"]),
            )
            .unwrap();
        assert_eq!(selected.cells.len(), 1);
        assert_eq!(selected.cells[b"price".as_slice()].as_i32(), Some(50));
        assert!(
            api.get(
                ReadTarget::Branch(branch),
                RecordOp::get(KEY).only([] as [&str; 0])
            )
            .unwrap()
            .cells
            .is_empty()
        );
        assert!(matches!(
            api.get(
                ReadTarget::Branch(branch),
                RecordOp::get(MISSING).only([] as [&str; 0])
            ),
            Err(ApiError::NotFound)
        ));
        let bindings = api
            .get(
                ReadTarget::Branch(branch),
                RecordOp::get(RecordKey(system::RECORD_KEYS.key)).only([KEY.0]),
            )
            .unwrap();
        assert_eq!(bindings.cells[KEY.0.as_slice()].as_u64(), Some(64));

        api.patch(
            branch,
            RecordOp::patch(KEY)
                .field("price", CellValue::from_i32(75))
                .unwrap()
                .remove("description")
                .unwrap(),
        )
        .unwrap();
        let edited = get(api.as_ref(), ReadTarget::Branch(branch));
        assert_eq!(edited.cells[b"price".as_slice()].as_i32(), Some(75));
        assert!(!edited.cells[b"price".as_slice()].is_indexable());
        let before = api.branch_info(branch).unwrap();
        api.patch(branch, RecordOp::patch(KEY).remove("absent").unwrap())
            .unwrap();
        assert_eq!(api.branch_info(branch).unwrap(), before);
        assert_eq!(
            api.commit(branch).unwrap(),
            golemdb_branch::CommitId::new(1)
        );
        assert_eq!(get(api.as_ref(), ReadTarget::Head), edited);
        assert_eq!(
            get(
                api.as_ref(),
                ReadTarget::Commit(golemdb_branch::CommitId::new(1))
            ),
            edited
        );
        assert_eq!(db.info().commit_id, golemdb_branch::CommitId::new(0)); // opening metadata is not a live head
        assert_eq!(db.head().unwrap(), golemdb_branch::CommitId::new(1));
        assert!(matches!(
            api.get(ReadTarget::Commit(golemdb_branch::CommitId::new(0)), RecordOp::get(KEY)),
            Err(ApiError::CommitUnavailable {
                requested, head }) if requested.get() == 0 && head.get() == 1
        ));
        assert!(matches!(api.commit(other), Err(ApiError::Conflict)));

        let deleting = api.begin().unwrap();
        api.delete(deleting, RecordOp::delete(KEY)).unwrap();
        assert!(matches!(
            api.get(ReadTarget::Branch(deleting), RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        assert_eq!(get(api.as_ref(), ReadTarget::Head), edited);
        api.commit(deleting).unwrap();
        assert!(matches!(
            api.get(ReadTarget::Head, RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        let recreated = api.begin().unwrap();
        api.create(recreated, input(KEY, 100)).unwrap();
        assert_eq!(allocator(api.as_ref(), ReadTarget::Branch(recreated)), 66);
        api.commit(recreated).unwrap();
        assert_eq!(
            get(api.as_ref(), ReadTarget::Head).cells[b"price".as_slice()].as_i32(),
            Some(100)
        );
    });
}

#[test]
fn checkpoints_rollback_seal_and_discard_share_state_across_clones() {
    each_backend(|db| {
        let clone = db.clone();
        let branch = db.begin().unwrap();
        clone.create(branch, input(KEY, 50)).unwrap();
        let before = get(&db, ReadTarget::Branch(branch));
        db.checkpoint(branch).unwrap();
        clone
            .patch(
                branch,
                RecordOp::patch(KEY)
                    .attribute("price", CellValue::from_i32(90))
                    .unwrap(),
            )
            .unwrap();
        db.rollback(branch).unwrap();
        assert_eq!(get(&clone, ReadTarget::Branch(branch)), before);
        clone.rollback(branch).unwrap();
        assert!(matches!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        assert_eq!(allocator(&db, ReadTarget::Branch(branch)), 64);
        assert!(matches!(
            db.rollback(branch),
            Err(ApiError::NoFrameToRollback)
        ));
        clone.create(branch, input(KEY, 50)).unwrap();
        let sealed = db.seal(branch).unwrap();
        assert_eq!(clone.seal(branch).unwrap(), sealed);
        assert_eq!(sealed.commit_id, golemdb_branch::CommitId::new(1));
        assert_eq!(db.head().unwrap(), golemdb_branch::CommitId::new(0));
        assert!(clone.branch_info(branch).unwrap().sealed);
        assert_eq!(get(&db, ReadTarget::Branch(branch)), before);
        let roots_key = RecordKey(system::ROOTS.key);
        let roots = db
            .get(ReadTarget::Branch(branch), RecordOp::get(roots_key))
            .unwrap();
        assert!(!roots.cells.contains_key(0u64.to_be_bytes().as_slice()));
        assert!(matches!(
            db.patch(branch, RecordOp::patch(KEY)),
            Err(ApiError::Sealed)
        ));
        assert!(matches!(db.checkpoint(branch), Err(ApiError::Sealed)));
        assert!(matches!(db.rollback(branch), Err(ApiError::Sealed)));
        assert_eq!(clone.commit(branch).unwrap(), sealed.commit_id);
        let roots = db.get(ReadTarget::Head, RecordOp::get(roots_key)).unwrap();
        assert!(roots.cells.contains_key(0u64.to_be_bytes().as_slice()));
        assert!(matches!(db.commit(branch), Err(ApiError::HandleInvalid)));
        let discarded = db.begin().unwrap();
        db.delete(discarded, RecordOp::delete(KEY)).unwrap();
        db.seal(discarded).unwrap();
        clone.discard(discarded).unwrap();
        assert!(matches!(
            db.branch_info(discarded),
            Err(ApiError::HandleInvalid)
        ));
        assert_eq!(get(&db, ReadTarget::Head), before);
    });
}

#[test]
fn reserved_records_limits_and_failed_mutations_preserve_state() {
    each_backend(|db| {
        let branch = db.begin().unwrap();
        let original = db.branch_info(branch).unwrap();
        for system in system::ALL {
            let key = RecordKey(system.key);
            assert!(db.get(ReadTarget::Head, RecordOp::get(key)).is_ok());
            assert!(matches!(
                db.create(branch, input(key, 1)),
                Err(ApiError::Reserved)
            ));
            assert!(matches!(
                db.patch(branch, RecordOp::patch(key)),
                Err(ApiError::Reserved)
            ));
            assert!(matches!(
                db.delete(branch, RecordOp::delete(key)),
                Err(ApiError::Reserved)
            ));
        }
        assert_eq!(db.branch_info(branch).unwrap(), original);
        for bad in [
            RecordOp::create(KEY)
                .field(&"a".repeat(33), CellValue::from_bool(true))
                .unwrap(),
            RecordOp::create(KEY)
                .field("long", CellValue::from_str(&"v".repeat(65)))
                .unwrap(),
            RecordOp::create(KEY)
                .field("long", CellValue::from_bytes(&[0; 129]))
                .unwrap(),
        ] {
            assert!(matches!(
                db.create(branch, bad),
                Err(ApiError::InvalidArgument { .. })
            ));
            assert_eq!(db.branch_info(branch).unwrap(), original);
            assert_eq!(allocator(&db, ReadTarget::Branch(branch)), 64);
        }
        assert!(matches!(
            db.patch(branch, RecordOp::patch(MISSING)),
            Err(ApiError::NotFound)
        ));
        assert!(matches!(
            db.delete(branch, RecordOp::delete(MISSING)),
            Err(ApiError::NotFound)
        ));
        db.create(branch, input(KEY, 50)).unwrap();
        let before = get(&db, ReadTarget::Branch(branch));
        let info = db.branch_info(branch).unwrap();
        let bad = RecordOp::patch(KEY)
            .field("aaa", CellValue::from_bool(true))
            .unwrap()
            .field("zzz", CellValue::from_str(&"v".repeat(65)))
            .unwrap();
        assert!(matches!(
            db.patch(branch, bad),
            Err(ApiError::InvalidArgument { .. })
        ));
        assert_eq!(get(&db, ReadTarget::Branch(branch)), before);
        assert_eq!(db.branch_info(branch).unwrap(), info);
        db.commit(branch).unwrap();
    });
}

#[test]
fn competing_commits_and_cross_thread_clones_use_one_registry() {
    each_backend(|db| {
        let first = db.begin().unwrap();
        let clone = db.clone();
        std::thread::spawn(move || clone.create(first, input(KEY, 10)).unwrap())
            .join()
            .unwrap();
        let second = db.begin().unwrap();
        db.create(second, input(KEY, 20)).unwrap();
        let gate = Arc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let run = |branch| {
                gate.wait();
                db.commit(branch)
            };
            let one = scope.spawn(move || run(first));
            let two = scope.spawn(move || run(second));
            (one.join().unwrap(), two.join().unwrap())
        });
        let price = match results {
            (Ok(commit), Err(ApiError::Conflict)) if commit.get() == 1 => 10,
            (Err(ApiError::Conflict), Ok(commit)) if commit.get() == 1 => 20,
            other => panic!("unexpected commit results: {other:?}"),
        };
        assert_eq!(
            get(&db, ReadTarget::Head).cells[b"price".as_slice()].as_i32(),
            Some(price)
        );
        assert!(matches!(db.commit(first), Err(ApiError::HandleInvalid)));
        assert!(matches!(db.commit(second), Err(ApiError::HandleInvalid)));
        let survivor = db.clone();
        drop(db);
        assert_eq!(survivor.head().unwrap(), golemdb_branch::CommitId::new(1));
    });
}

#[test]
fn opening_setup_conversion_and_separate_engines_do_not_share_branch_handles() {
    let cfg = config(HashAlgorithm::Blake3);
    let storage = MemoryStore::new();
    let setup = Database::from_store(storage.clone(), &cfg).unwrap();
    let info = *setup.info();
    let first = setup;
    assert_eq!(first.info(), &info);
    let branch = first.begin().unwrap();
    first.create(branch, input(KEY, 50)).unwrap();
    let second = Database::from_store(storage, &cfg).unwrap();
    assert!(matches!(
        second.branch_info(branch),
        Err(ApiError::HandleInvalid)
    ));
    assert!(matches!(
        second.get(ReadTarget::Head, RecordOp::get(KEY)),
        Err(ApiError::NotFound)
    ));
    first.commit(branch).unwrap();
    assert_eq!(
        get(&first, ReadTarget::Head),
        get(&second, ReadTarget::Head)
    );
}

#[test]
fn durable_facade_reopens_committed_state_and_discards_pending_work() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let cfg = config(hash);
        let dir = tempfile::tempdir().unwrap();
        let (sealed, pending, genesis_id) = {
            let db = Database::open_with_options(dir.path(), &cfg, MdbxOptions::default()).unwrap();
            let branch = db.begin().unwrap();
            db.create(branch, input(KEY, 50)).unwrap();
            let sealed = db.seal(branch).unwrap();
            db.commit(branch).unwrap();
            let pending = db.begin().unwrap();
            db.delete(pending, RecordOp::delete(KEY)).unwrap();
            (sealed, pending, db.info().genesis_id)
        };
        let db = Database::open(dir.path(), &cfg.with_mode(OpenMode::ExistingOnly)).unwrap();
        assert!(!db.info().created);
        assert_eq!(db.info().genesis_id, genesis_id);
        assert_eq!(db.info().commit_id, sealed.commit_id);
        assert_eq!(db.info().state_root, sealed.state_root);
        assert_eq!(db.info().index_root, sealed.index_root);
        assert!(matches!(
            db.branch_info(pending),
            Err(ApiError::HandleInvalid)
        ));
        assert_eq!(
            get(&db, ReadTarget::Head).cells[b"price".as_slice()].as_i32(),
            Some(50)
        );
        let branch = db.begin().unwrap();
        db.patch(
            branch,
            RecordOp::patch(KEY)
                .attribute("price", CellValue::from_i32(70))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(db.commit(branch).unwrap(), golemdb_branch::CommitId::new(2));
    }
}

#[test]
fn empty_records_keep_identity_through_patch_rollback_commit_and_delete() {
    each_backend(|db| {
        let branch = db.begin().unwrap();
        db.create(branch, RecordOp::create(KEY)).unwrap();
        let empty = get(&db, ReadTarget::Branch(branch));
        assert_eq!(empty.key, KEY);
        assert_eq!(empty.cells.len(), 1);
        assert_eq!(empty.cells[b"#key".as_slice()].as_bytes32(), Some(KEY.0));
        assert_eq!(allocator(&db, ReadTarget::Branch(branch)), 65);
        assert!(matches!(
            db.create(branch, RecordOp::create(KEY)),
            Err(ApiError::AlreadyExists)
        ));
        assert!(
            db.get(
                ReadTarget::Branch(branch),
                RecordOp::get(KEY).only([] as [&str; 0])
            )
            .unwrap()
            .cells
            .is_empty()
        );
        assert!(matches!(
            db.get(ReadTarget::Head, RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        db.commit(branch).unwrap();
        assert_eq!(get(&db, ReadTarget::Head), empty);

        let branch = db.begin().unwrap();
        let info = db.branch_info(branch).unwrap();
        db.patch(branch, RecordOp::patch(KEY)).unwrap();
        db.patch(branch, RecordOp::patch(KEY).remove("missing").unwrap())
            .unwrap();
        assert_eq!(db.branch_info(branch).unwrap(), info);
        db.patch(
            branch,
            RecordOp::patch(KEY)
                .attribute("price", CellValue::from_i32(50))
                .unwrap(),
        )
        .unwrap();
        db.commit(branch).unwrap();
        let populated = get(&db, ReadTarget::Head);

        let branch = db.begin().unwrap();
        db.checkpoint(branch).unwrap();
        db.patch(branch, RecordOp::patch(KEY).remove("price").unwrap())
            .unwrap();
        assert_eq!(get(&db, ReadTarget::Branch(branch)), empty);
        assert_eq!(get(&db, ReadTarget::Head), populated);
        db.rollback(branch).unwrap();
        assert_eq!(get(&db, ReadTarget::Branch(branch)), populated);
        db.patch(branch, RecordOp::patch(KEY).remove("price").unwrap())
            .unwrap();
        let seal = db.seal(branch).unwrap();
        assert_eq!(seal.index_root, db.info().index_root); // Removing the last attribute empties the index.
        assert_eq!(get(&db, ReadTarget::Branch(branch)), empty);
        db.commit(branch).unwrap();
        assert_eq!(get(&db, ReadTarget::Head), empty);

        let branch = db.begin().unwrap();
        db.checkpoint(branch).unwrap();
        db.delete(branch, RecordOp::delete(KEY)).unwrap();
        assert!(matches!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        db.rollback(branch).unwrap();
        assert_eq!(get(&db, ReadTarget::Branch(branch)), empty);
        db.delete(branch, RecordOp::delete(KEY)).unwrap();
        db.commit(branch).unwrap();
        assert!(matches!(
            db.get(ReadTarget::Head, RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        let branch = db.begin().unwrap();
        db.create(branch, RecordOp::create(KEY)).unwrap();
        assert_eq!(allocator(&db, ReadTarget::Branch(branch)), 66);
        db.commit(branch).unwrap();
        assert_eq!(get(&db, ReadTarget::Head), empty);
    });
}

#[test]
fn mdbx_reopens_empty_records_created_directly_or_by_removing_all_cells() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(hash);
        {
            let db = Database::open(dir.path(), &cfg).unwrap();
            let branch = db.begin().unwrap();
            db.create(branch, RecordOp::create(KEY)).unwrap();
            db.create(branch, input(MISSING, 50)).unwrap();
            db.commit(branch).unwrap();
            let branch = db.begin().unwrap();
            db.patch(
                branch,
                RecordOp::patch(MISSING)
                    .remove("price")
                    .unwrap()
                    .remove("description")
                    .unwrap(),
            )
            .unwrap();
            db.commit(branch).unwrap();
        }
        let db = Database::open(dir.path(), &cfg.with_mode(OpenMode::ExistingOnly)).unwrap();
        for key in [KEY, MISSING] {
            let record = db.get(ReadTarget::Head, RecordOp::get(key)).unwrap();
            assert_eq!(record.cells.len(), 1);
            assert_eq!(record.cells[b"#key".as_slice()].as_bytes32(), Some(key.0));
        }
        assert_eq!(allocator(&db, ReadTarget::Head), 66);
    }
}
