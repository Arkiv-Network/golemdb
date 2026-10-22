use std::sync::{Arc, Barrier};

use golemdb_api::*;
use golemdb_branch::Branches;
use golemdb_cells::{CellKey, CellReader, reserved, system, tables};
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher};
use golemdb_record::Records;
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};

const SUPERBLOCK: Table = Table("Superblock");
const KEY: RecordKey = RecordKey([0x42; 32]);
const YAML: &str = "hash_function: keccak-256\ncell_limits:\n  max_cell_name_len: 32\n  max_str_len: 64\n  max_bytes_len: 128\n";

fn config() -> OpenConfig {
    OpenConfig::new(Genesis::from_yaml(YAML).unwrap())
}

pub(super) fn snapshot(db: &impl Store) -> Vec<Vec<golemdb_storage::Entry>> {
    let tx = db.begin_read().unwrap();
    [
        SUPERBLOCK,
        tables::CELL,
        tables::CELL_TRIE,
        golemdb_index::tables::INDEX,
        golemdb_index::tables::INDEX_TRIE,
        golemdb_index::tables::BITMAP_TRIE,
        golemdb_index::tables::BITMAP_CONTAINER,
    ]
    .into_iter()
    .map(|table| {
        scan_prefix(&tx, table, vec![])
            .unwrap()
            .collect::<golemdb_storage::Result<_>>()
            .unwrap()
    })
    .collect()
}

#[test]
fn yaml_is_explicit_strict_and_has_canonical_identity() {
    let first = Genesis::from_yaml(YAML).unwrap();
    let reordered = Genesis::from_yaml("# same deployment\ncell_limits: {max_bytes_len: 128, max_str_len: 64, max_cell_name_len: 32}\nhash_function: keccak-256\nimmutable_data: null\n").unwrap();
    assert_eq!(first, reordered);
    assert_eq!(
        Database::open_memory(&first).unwrap().info(),
        Database::open_memory(&reordered).unwrap().info()
    );
    for bad in [
        "",
        "hash_function: keccak-256",
        "cell_limits: {}",
        "hash_function: sha256\ncell_limits: {}",
        "hash_function: blake3\nhash_function: keccak-256\ncell_limits: {}",
    ] {
        assert!(matches!(Genesis::from_yaml(bad), Err(OpenError::Yaml(_))));
    }
    for bad in [
        YAML.replace("max_str_len: 64", "max_str_len: 64\n  unexpected: 1"),
        format!("{YAML}unknown: 1\n"),
        YAML.replace("max_str_len: 64", "max_str_len: 64\n  max_str_len: 65"),
        YAML.replace("max_str_len: 64", "max_str_len: -1"),
    ] {
        assert!(Genesis::from_yaml(&bad).is_err());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("genesis.yaml");
    std::fs::write(&path, YAML).unwrap();
    assert_eq!(Genesis::load(&path).unwrap(), first);
    assert!(matches!(
        Genesis::load(dir.path().join("missing")),
        Err(OpenError::Io(_))
    ));
}

fn lifecycle<S: Store + Clone + Send + Sync + 'static, H: HashProvider + Copy>(
    db: S,
    config: OpenConfig,
    hasher: H,
) -> OpenInfo {
    let opened = Database::from_store(db.clone(), &config).unwrap();
    let genesis = *opened.info();
    assert!(genesis.created);
    assert_eq!(genesis.commit_id, golemdb_branch::CommitId::new(0));
    assert_ne!(genesis.state_root, hasher.hash(&[]));
    assert_eq!(genesis.index_root, hasher.hash(&[]));
    assert_eq!(opened.genesis(), &config.genesis);
    let tx = db.begin_read().unwrap();
    let reader = CellReader::new(&tx);
    for record in system::ALL {
        assert_eq!(
            reader.get(&CellKey::new(record.id, reserved::KEY)).unwrap(),
            Some(CellValue::from_bytes32(record.key))
        );
        assert_eq!(
            reader
                .get(&CellKey::new(
                    system::RECORD_KEYS.id,
                    CellNameRef::raw(&record.key)
                ))
                .unwrap(),
            Some(CellValue::from_u64(record.id))
        );
    }
    for id in [
        system::ROOTS.id,
        system::ROOT_INDEX.id,
        system::METERING_MODEL.id,
        system::MODEL_WEIGHT.id,
    ] {
        assert_eq!(reader.scan_record(id).unwrap().count(), 1);
    }
    let before = snapshot(&db);
    let reopened = Database::from_store(db.clone(), &config).unwrap();
    assert_eq!(
        *reopened.info(),
        OpenInfo {
            created: false,
            ..genesis
        }
    );
    assert_eq!(snapshot(&db), before);

    let branches = Branches::new(db.clone(), hasher, None).unwrap();
    let records = Records::new(branches.clone());
    let branch = branches.begin().unwrap();
    let cells = golemdb_record::RecordCells::from([(
        CellNameRef::raw(b"price").into(),
        CellValue::from_i32(50)
            .with_kind(CellKind::Attribute)
            .unwrap(),
    )]);
    records
        .create(branch, KEY, create_entries(cells), Budget::Unlimited)
        .result
        .unwrap();
    assert_eq!(
        branches.commit(branch).unwrap(),
        golemdb_branch::CommitId::new(1)
    );
    let before = snapshot(&db);
    let reopened = Database::from_store(
        db.clone(),
        &config.clone().with_mode(OpenMode::ExistingOnly),
    )
    .unwrap();
    assert!(!reopened.info().created);
    assert_eq!(reopened.info().commit_id, golemdb_branch::CommitId::new(1));
    assert_eq!(reopened.info().genesis_id, genesis.genesis_id);
    assert_eq!(snapshot(&db), before);
    let fresh = Records::new(Branches::new(db.clone(), hasher, None).unwrap());
    assert_eq!(
        fresh
            .get(ReadTarget::Head, KEY, None, Budget::Unlimited)
            .result
            .unwrap()
            .cells[b"price".as_slice()]
        .as_i32(),
        Some(50)
    );
    let tx = db.begin_read().unwrap();
    let reader = CellReader::new(&tx);
    assert_eq!(
        reader
            .get(&CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID))
            .unwrap(),
        Some(CellValue::from_u64(65))
    );
    assert_eq!(
        reader
            .get(&CellKey::new(
                system::ROOTS.id,
                CellNameRef::raw(&0u64.to_be_bytes())
            ))
            .unwrap()
            .unwrap()
            .as_bytes(),
        Some([genesis.state_root, genesis.index_root].concat().as_slice())
    );
    // `#roots` commits the hashes; the Roots table locates both commits' tries.
    let roots = |commit| golemdb_branch::read_roots(&tx, golemdb_branch::CommitId::new(commit));
    let first = roots(0).unwrap().unwrap();
    assert_eq!(first.state.hash(&hasher), genesis.state_root);
    assert_eq!(first.index, golemdb_merkle::RootRef::Empty);
    assert_eq!(
        roots(1).unwrap().unwrap().state.hash(&hasher),
        reopened.info().state_root
    );
    genesis
}

#[test]
fn memory_genesis_and_committed_reopen_support_both_hash_algorithms() {
    let keccak = lifecycle(MemoryStore::new(), config(), Keccak256Hasher);
    let mut blake = config();
    blake.genesis.hash_function = HashAlgorithm::Blake3;
    let blake = lifecycle(MemoryStore::new(), blake, Blake3Hasher);
    assert_ne!(keccak.genesis_id, blake.genesis_id);
    assert_ne!(keccak.state_root, blake.state_root);
}

fn modes(db: impl Store + Clone + Send + Sync + 'static) {
    let config = config();
    assert!(matches!(
        Database::from_store(
            db.clone(),
            &config.clone().with_mode(OpenMode::ExistingOnly)
        ),
        Err(OpenError::NotInitialized)
    ));
    assert!(db.begin_write().unwrap().is_empty().unwrap());
    let opened =
        Database::from_store(db.clone(), &config.clone().with_mode(OpenMode::CreateNew)).unwrap();
    assert!(opened.info().created);
    let before = snapshot(&db);
    assert!(matches!(
        Database::from_store(db.clone(), &config.clone().with_mode(OpenMode::CreateNew)),
        Err(OpenError::AlreadyInitialized)
    ));
    for change in 0..4 {
        let mut different = config.clone();
        match change {
            0 => different.genesis.hash_function = HashAlgorithm::Blake3,
            1 => different.genesis.cell_limits.max_cell_name_len += 1,
            2 => different.genesis.cell_limits.max_str_len += 1,
            _ => different.genesis.cell_limits.max_bytes_len += 1,
        }
        assert!(matches!(
            Database::from_store(db.clone(), &different),
            Err(OpenError::GenesisMismatch)
        ));
        assert_eq!(snapshot(&db), before);
    }
}

#[test]
fn opening_modes_and_genesis_mismatch_do_not_change_memory_state() {
    modes(MemoryStore::new());
}

#[test]
fn partial_or_foreign_state_is_never_initialized() {
    for table in [
        SUPERBLOCK,
        tables::CELL,
        tables::CELL_TRIE,
        Table("Foreign"),
    ] {
        for delete in [false, true] {
            let db = MemoryStore::new();
            let mut tx = db.begin_write().unwrap();
            tx.put(table, b"key", b"partial").unwrap();
            if delete {
                tx.delete(table, b"key").unwrap();
            }
            tx.commit().unwrap();
            let before = snapshot(&db);
            for mode in [
                OpenMode::CreateIfMissing,
                OpenMode::ExistingOnly,
                OpenMode::CreateNew,
            ] {
                assert!(matches!(
                    Database::from_store(db.clone(), &config().with_mode(mode)),
                    Err(OpenError::CorruptState(_))
                ));
            }
            assert_eq!(snapshot(&db), before);
            assert_eq!(
                db.begin_read().unwrap().get(table, b"key").unwrap(),
                (!delete).then(|| b"partial".to_vec())
            );
        }
    }
}

#[test]
fn missing_or_malformed_metadata_and_reserved_cells_fail_without_repair() {
    for key in [
        b"format".as_slice(),
        b"hash_fn",
        b"roaring",
        b"genesis_id",
        b"head",
    ] {
        for malformed in [false, true] {
            let db = MemoryStore::new();
            Database::from_store(db.clone(), &config()).unwrap();
            let mut tx = db.begin_write().unwrap();
            if malformed {
                tx.put(SUPERBLOCK, key, b"x").unwrap();
            } else {
                tx.delete(SUPERBLOCK, key).unwrap();
            }
            tx.commit().unwrap();
            let before = snapshot(&db);
            assert!(Database::from_store(db.clone(), &config()).is_err());
            assert_eq!(snapshot(&db), before);
        }
    }
    for key in [
        CellKey::new(system::PARAMS.id, reserved::MAX_STR_LEN),
        CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID),
        CellKey::new(system::PARAMS.id, reserved::KEY),
        CellKey::new(
            system::RECORD_KEYS.id,
            CellNameRef::raw(&system::PARAMS.key),
        ),
    ] {
        for malformed in [false, true] {
            let db = MemoryStore::new();
            Database::from_store(db.clone(), &config()).unwrap();
            let mut tx = db.begin_write().unwrap();
            if malformed {
                tx.put(
                    tables::CELL,
                    &key.encode(),
                    CellValue::from_u64(100).encoded_bytes(),
                )
                .unwrap();
            } else {
                tx.delete(tables::CELL, &key.encode()).unwrap();
            }
            tx.commit().unwrap();
            let before = snapshot(&db);
            assert!(Database::from_store(db.clone(), &config()).is_err());
            assert_eq!(snapshot(&db), before);
        }
    }
}

#[test]
fn unsupported_format_ids_and_missing_trie_roots_are_rejected() {
    for (key, bytes) in [
        (b"format".as_slice(), 99u32.to_be_bytes().to_vec()),
        (b"hash_fn", 99u16.to_be_bytes().to_vec()),
        (b"roaring", 99u16.to_be_bytes().to_vec()),
    ] {
        let db = MemoryStore::new();
        Database::from_store(db.clone(), &config()).unwrap();
        let mut tx = db.begin_write().unwrap();
        tx.put(SUPERBLOCK, key, &bytes).unwrap();
        tx.commit().unwrap();
        let result = Database::from_store(db, &config());
        assert!(matches!(
            result,
            Err(OpenError::UnsupportedFormat(99)
                | OpenError::UnsupportedHash(99)
                | OpenError::UnsupportedRoaring(99))
        ));
    }
    let db = MemoryStore::new();
    Database::from_store(db.clone(), &config()).unwrap();
    // The genesis state root is a branch stored at commit 0, at the empty route.
    let root = golemdb_merkle::node_key(0, &[], &[]);
    let mut tx = db.begin_write().unwrap();
    assert!(tx.delete(tables::CELL_TRIE, &root).unwrap());
    tx.commit().unwrap();
    assert!(Database::from_store(db, &config()).is_err());
}

fn concurrent(db: impl Store + Clone + Send + Sync + 'static, mode: OpenMode, different: bool) {
    let mut a = config();
    a.mode = mode;
    let mut b = a.clone();
    if different {
        b.genesis.cell_limits.max_str_len += 1;
    }
    let barrier = Arc::new(Barrier::new(2));
    let (first, second) = std::thread::scope(|scope| {
        let call = |cfg, barrier: Arc<Barrier>| {
            barrier.wait();
            Database::from_store(db.clone(), &cfg).map(|opened| *opened.info())
        };
        let first_barrier = barrier.clone();
        let one = scope.spawn(move || call(a, first_barrier));
        let two = scope.spawn(move || call(b, barrier));
        (one.join().unwrap(), two.join().unwrap())
    });
    match (mode, different) {
        (_, true) => assert!(matches!(
            (first, second),
            (
                Ok(OpenInfo { created: true, .. }),
                Err(OpenError::GenesisMismatch)
            ) | (
                Err(OpenError::GenesisMismatch),
                Ok(OpenInfo { created: true, .. })
            )
        )),
        (OpenMode::CreateNew, false) => assert!(matches!(
            (first, second),
            (
                Ok(OpenInfo { created: true, .. }),
                Err(OpenError::AlreadyInitialized)
            ) | (
                Err(OpenError::AlreadyInitialized),
                Ok(OpenInfo { created: true, .. })
            )
        )),
        _ => {
            let first = first.unwrap();
            let second = second.unwrap();
            assert_ne!(first.created, second.created);
            assert_eq!(first.genesis_id, second.genesis_id);
            assert_eq!(first.state_root, second.state_root);
        }
    }
}

#[test]
fn memory_initializers_serialize_and_compare_genesis() {
    for (mode, different) in [
        (OpenMode::CreateIfMissing, false),
        (OpenMode::CreateNew, false),
        (OpenMode::CreateIfMissing, true),
    ] {
        concurrent(MemoryStore::new(), mode, different);
    }
}

#[test]
fn mdbx_genesis_matches_memory_and_reopens_from_disk_after_commit() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.genesis.hash_function = hash;
        let memory = *Database::open_memory(&cfg.genesis).unwrap().info();
        {
            let db = Database::open(dir.path(), &cfg).unwrap();
            assert_eq!(db.info(), &memory);
        }
        let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
        let branches = match hash {
            HashAlgorithm::Keccak256 => {
                let branches = Branches::new(db.clone(), Keccak256Hasher, None).unwrap();
                let record = Records::new(branches.clone());
                let branch = branches.begin().unwrap();
                record
                    .create(
                        branch,
                        KEY,
                        create_entries(golemdb_record::RecordCells::from([(
                            CellNameRef::raw(b"price").into(),
                            CellValue::from_i32(50),
                        )])),
                        Budget::Unlimited,
                    )
                    .result
                    .unwrap();
                branches.commit(branch).unwrap()
            }
            HashAlgorithm::Blake3 => {
                let branches = Branches::new(db.clone(), Blake3Hasher, None).unwrap();
                let record = Records::new(branches.clone());
                let branch = branches.begin().unwrap();
                record
                    .create(
                        branch,
                        KEY,
                        create_entries(golemdb_record::RecordCells::from([(
                            CellNameRef::raw(b"price").into(),
                            CellValue::from_i32(50),
                        )])),
                        Budget::Unlimited,
                    )
                    .result
                    .unwrap();
                branches.commit(branch).unwrap()
            }
        };
        assert_eq!(branches, golemdb_branch::CommitId::new(1));
        drop(db);
        let opened = Database::open(dir.path(), &cfg.with_mode(OpenMode::ExistingOnly)).unwrap();
        assert_eq!(opened.info().commit_id, golemdb_branch::CommitId::new(1));
        assert_eq!(opened.info().genesis_id, memory.genesis_id);
        drop(opened);
        let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
        let tx = db.begin_read().unwrap();
        assert_eq!(
            CellReader::new(&tx)
                .get(&CellKey::new(64, CellNameRef::raw(b"price")))
                .unwrap(),
            Some(CellValue::from_i32(50))
        );
    }
}

#[test]
fn mdbx_modes_limits_and_shared_environment_concurrent_opening() {
    let dir = tempfile::tempdir().unwrap();
    modes(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
    for (mode, different) in [
        (OpenMode::CreateIfMissing, false),
        (OpenMode::CreateNew, false),
        (OpenMode::CreateIfMissing, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        concurrent(
            golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap(),
            mode,
            different,
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent");
    assert!(matches!(
        Database::open(&missing, &config().with_mode(OpenMode::ExistingOnly)),
        Err(OpenError::NotInitialized)
    ));
    assert!(!missing.exists());
    let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
    let mut cfg = config();
    // Name + separator + tag + value must fit, not just the string by itself.
    cfg.genesis.cell_limits.max_str_len = db.max_key_size() as u32;
    assert!(matches!(
        Database::from_store(db.clone(), &cfg),
        Err(OpenError::InvalidConfig(_))
    ));
    assert!(db.begin_write().unwrap().is_empty().unwrap());
    cfg.genesis.cell_limits.max_str_len =
        db.max_key_size() as u32 - cfg.genesis.cell_limits.max_cell_name_len - 2;
    Database::from_store(db.clone(), &cfg).unwrap();
    let branches = Branches::new(db, Keccak256Hasher, None).unwrap();
    let records = Records::new(branches.clone());
    let branch = branches.begin().unwrap();
    let name = "a".repeat(cfg.genesis.cell_limits.max_cell_name_len as usize);
    let value = CellValue::from_str(&"v".repeat(cfg.genesis.cell_limits.max_str_len as usize))
        .with_kind(CellKind::Attribute)
        .unwrap();
    let cells =
        golemdb_record::RecordCells::from([(CellNameRef::raw(name.as_bytes()).into(), value)]);
    records
        .create(branch, KEY, create_entries(cells), Budget::Unlimited)
        .result
        .unwrap();
    branches.commit(branch).unwrap();
}

#[test]
fn mdbx_existing_empty_tables_prevent_initialization() {
    for delete in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
        let mut tx = db.begin_write().unwrap();
        tx.put(Table("Foreign"), b"key", b"value").unwrap();
        if delete {
            tx.delete(Table("Foreign"), b"key").unwrap();
        }
        assert!(!tx.is_empty().unwrap());
        tx.commit().unwrap();
        assert!(matches!(
            Database::from_store(db.clone(), &config()),
            Err(OpenError::CorruptState(_))
        ));
        assert_eq!(
            db.begin_read()
                .unwrap()
                .get(Table("Foreign"), b"key")
                .unwrap(),
            (!delete).then(|| b"value".to_vec())
        );
    }
}

fn create_entries(cells: golemdb_record::RecordCells) -> Vec<golemdb_record::CreateEntry> {
    cells
        .into_iter()
        .map(|(name, value)| golemdb_record::CreateEntry {
            name,
            kind: value.kind(),
            value,
        })
        .collect()
}
