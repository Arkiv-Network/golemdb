use golemdb_api::{
    Api, CellLimits, Database, Genesis, HashAlgorithm, OpenConfig, OpenError, PricingConfig,
    PruningConfig, RecordLimits,
};
use golemdb_branch::{BranchError, Branches, CommitId};
use golemdb_cells::{
    CellKey, CellKind, CellNameRef, CellReader, CellType, CellValue, reserved, system,
    tables as cells,
};
use golemdb_history::{CommitSet, HistoryError, changeset_key};
use golemdb_index::{ContainerId, IndexKey, IndexReader, IndexTerm, tables as index};
use golemdb_merkle::Keccak256Hasher;
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};
use golemdb_storage_mdbx::MdbxStore;

const SUPERBLOCK: Table = Table("Superblock");
const HISTORY: [Table; 4] = [
    cells::CELL_HISTORY,
    cells::CELL_CHANGE_SET,
    index::INDEX_HISTORY,
    index::INDEX_CHANGE_SET,
];

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

fn key(id: u64) -> CellKey {
    CellKey::new(id, CellNameRef::raw(b"color"))
}
fn value(s: &str) -> CellValue {
    CellValue::from_str(s)
        .with_kind(CellKind::Attribute)
        .unwrap()
}
fn index_key(s: &str, hi: u64) -> IndexKey {
    IndexKey {
        term: IndexTerm::new("color", CellType::Str, s.as_bytes()).unwrap(),
        container_id: ContainerId::new(hi).unwrap(),
    }
}
fn history_snapshot(tx: &impl ReadTransaction) -> Vec<Vec<golemdb_storage::Entry>> {
    HISTORY
        .into_iter()
        .map(|table| {
            scan_prefix(tx, table, vec![])
                .unwrap()
                .collect::<golemdb_storage::Result<Vec<_>>>()
                .unwrap()
        })
        .collect()
}

fn assert_final(tx: &impl ReadTransaction) {
    golemdb_pruning::Pruner::validate(tx, 5, 0).unwrap();
    for (target, first, second) in [
        (0, None, None),
        (1, Some(value("A")), Some(value("A"))),
        (2, Some(value("C")), Some(value("C"))),
        (3, Some(CellValue::from_str("C")), Some(value("C"))),
        (4, Some(CellValue::from_str("C")), None),
        (5, Some(value("A")), None),
    ] {
        let reader = CellReader::at(tx, target);
        assert_eq!(reader.get(&key(64)).unwrap(), first);
        assert_eq!(reader.get(&key(65)).unwrap(), second);
        assert_eq!(
            reader.get(&key(65537)).unwrap(),
            (target > 0).then(|| value("A"))
        );
        for id in 80..84 {
            assert_eq!(reader.get(&key(id)).unwrap(), None);
        }
        // Genesis parameters always existed, but the first later allocator
        // update must preserve its genesis value.
        assert_eq!(
            reader
                .get(&CellKey::new(system::PARAMS.id, reserved::MAX_RECORD_CELLS))
                .unwrap(),
            Some(CellValue::from_u64(100))
        );
        assert_eq!(
            reader
                .get(&CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID))
                .unwrap(),
            Some(CellValue::from_u64(if target == 0 { 64 } else { 65538 }))
        );
    }
    let a0 = index_key("A", 0);
    let a1 = index_key("A", 1);
    let c0 = index_key("C", 0);
    for (target, a, c) in [
        (0, vec![], vec![]),
        (1, vec![64, 65], vec![]),
        (2, vec![], vec![64, 65]),
        (3, vec![], vec![65]),
        (4, vec![], vec![]),
        (5, vec![64], vec![]),
    ] {
        let reader = IndexReader::at(tx, target);
        let ids = |key: &IndexKey| {
            reader
                .container(key)
                .unwrap()
                .map_or_else(Vec::new, |c| c.iter().collect::<Vec<_>>())
        };
        assert_eq!(ids(&a0), a);
        assert_eq!(ids(&c0), c);
        assert_eq!(ids(&a1), if target == 0 { vec![] } else { vec![65537] });
    }
    assert_eq!(
        index::HISTORY.read_commits(tx, &a1.encode()).unwrap(),
        [1].into_iter().collect::<CommitSet>()
    );
    assert_eq!(
        cells::HISTORY.read_commits(tx, &key(64).encode()).unwrap(),
        [1, 2, 3, 5].into_iter().collect::<CommitSet>()
    );
    assert_eq!(
        tx.get(cells::CELL_CHANGE_SET, &changeset_key(2, &key(64).encode()))
            .unwrap(),
        Some(value("A").into_bytes())
    );
    let old = index::HISTORY.resolve(tx, &a0.encode(), 1).unwrap();
    let golemdb_history::ValueSource::Preimage(Some(bytes)) = old else {
        panic!("expected the container's value before deletion")
    };
    assert_eq!(
        golemdb_index::BitmapContainer::decode(a0.container_id, &bytes)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [64, 65]
    );
    for id in 80..84 {
        assert!(
            tx.get(cells::CELL_HISTORY, &key(id).encode())
                .unwrap()
                .is_none()
        );
    }
    for c in 0..5u64 {
        let root_cell = CellKey::new(system::ROOTS.id, CellNameRef::raw(&c.to_be_bytes()));
        assert_eq!(
            tx.get(
                cells::CELL_CHANGE_SET,
                &changeset_key(c + 1, &root_cell.encode())
            )
            .unwrap(),
            Some(vec![])
        );
        assert!(CellReader::at(tx, c).get(&root_cell).unwrap().is_none());
    }
}

fn publication(store: impl Store + Clone + Send + Sync + 'static) {
    let db = Database::from_store(store.clone(), &config()).unwrap();
    let genesis = store.begin_read().unwrap();
    assert!(history_snapshot(&genesis).iter().all(Vec::is_empty));
    golemdb_pruning::Pruner::validate(&genesis, 0, 0).unwrap();
    let branches = Branches::new(store.clone(), Keccak256Hasher, None, None).unwrap();
    let first = branches.begin().unwrap();
    branches
        .write(first, |c| {
            for id in [64, 65, 65537] {
                c.put(key(id), value("A"));
            }
            c.put(
                CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID),
                CellValue::from_u64(65538),
            );
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.commit(first).unwrap();
    let old = store.begin_read().unwrap();
    let before = history_snapshot(&old);
    let winner = branches.begin().unwrap();
    let loser = branches.begin().unwrap();
    branches
        .write(winner, |c| {
            c.put(key(64), value("B"));
            c.put(key(64), value("C"));
            c.put(key(65), value("C"));
            c.put(key(65537), value("temporary"));
            c.put(key(65537), value("A"));
            c.put(key(80), value("temporary"));
            c.delete(key(80));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(
        branches
            .write(winner, |c| {
                c.put(key(81), value("failed"));
                Err::<(), _>("failed operation")
            })
            .is_err()
    );
    branches.checkpoint(winner).unwrap();
    branches
        .write(winner, |c| {
            c.put(key(82), value("rolled back"));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.rollback(winner).unwrap();
    branches
        .write(loser, |c| {
            c.put(key(64), value("loser"));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.seal(loser).unwrap();
    let discarded = branches.begin().unwrap();
    branches
        .write(discarded, |c| {
            c.put(key(83), value("discarded"));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.seal(discarded).unwrap();
    branches.discard(discarded).unwrap();
    let sealed = branches.seal(winner).unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &sealed,
        &branches.seal(winner).unwrap()
    ));
    assert_eq!(history_snapshot(&store.begin_read().unwrap()), before);
    assert_eq!(sealed.index.changed_containers.len(), 2); // A/0 removed, C/0 created; A/1 unchanged.
    branches.commit(winner).unwrap();
    let published = history_snapshot(&store.begin_read().unwrap());
    assert!(matches!(branches.commit(loser), Err(BranchError::Conflict)));
    assert_eq!(history_snapshot(&store.begin_read().unwrap()), published);
    assert_eq!(history_snapshot(&old), before);
    assert!(history_snapshot(&genesis).iter().all(Vec::is_empty));
    let b = branches.begin().unwrap();
    branches
        .write(b, |c| {
            c.put(key(64), CellValue::from_str("C"));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    branches
        .write(b, |c| {
            c.delete(key(65));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    branches
        .write(b, |c| {
            c.put(key(64), value("A"));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.commit(b).unwrap();
    assert_final(&store.begin_read().unwrap());
    // Until record/query wiring is complete, the facade still refuses old targets.
    assert!(db.metering_model(Some(CommitId::GENESIS)).is_err());
}

#[test]
fn memory_commits_record_only_net_changes_including_system_cells() {
    publication(MemoryStore::new());
}

#[test]
fn mdbx_history_reopens_with_its_committed_head() {
    let dir = tempfile::tempdir().unwrap();
    {
        publication(MdbxStore::open(dir.path()).unwrap());
    }
    let store = MdbxStore::open(dir.path()).unwrap();
    let db = Database::from_store(store.clone(), &config()).unwrap();
    assert_eq!(db.info().commit_id, CommitId::new(5));
    assert_final(&store.begin_read().unwrap());
}

fn metadata_checks(store: impl Store + Clone + Send + Sync + 'static) {
    let cfg = config();
    let original = *Database::from_store(store.clone(), &cfg).unwrap().info();
    let mut tx = store.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"format", &1u32.to_be_bytes()).unwrap();
    tx.commit().unwrap();
    let before = scan_prefix(&store.begin_read().unwrap(), SUPERBLOCK, vec![])
        .unwrap()
        .collect::<golemdb_storage::Result<Vec<_>>>()
        .unwrap();
    assert!(matches!(
        Database::from_store(store.clone(), &cfg),
        Err(OpenError::UnsupportedFormat(1))
    ));
    assert_eq!(
        scan_prefix(&store.begin_read().unwrap(), SUPERBLOCK, vec![])
            .unwrap()
            .collect::<golemdb_storage::Result<Vec<_>>>()
            .unwrap(),
        before
    );
    let mut tx = store.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"format", &2u32.to_be_bytes()).unwrap();
    tx.commit().unwrap();
    for bytes in [
        None,
        Some(vec![]),
        Some(vec![0; 7]),
        Some(vec![0; 9]),
        Some(1u64.to_be_bytes().to_vec()),
    ] {
        let mut tx = store.begin_write().unwrap();
        match &bytes {
            Some(b) => tx.put(SUPERBLOCK, b"history_floor", b).unwrap(),
            None => {
                tx.delete(SUPERBLOCK, b"history_floor").unwrap();
            }
        }
        tx.commit().unwrap();
        assert!(matches!(
            Database::from_store(store.clone(), &cfg),
            Err(OpenError::Pruning(
                golemdb_pruning::PruningError::InvalidHistoryMetadata(_)
            ))
        ));
        assert_eq!(
            store
                .begin_read()
                .unwrap()
                .get(SUPERBLOCK, b"history_floor")
                .unwrap(),
            bytes
        );
    }
    let mut tx = store.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"history_floor", &0u64.to_be_bytes())
        .unwrap();
    tx.commit().unwrap();
    // The existing pruning window is local configuration, not genesis identity.
    for window in [0, 17] {
        let other = cfg.clone().with_pruning(PruningConfig::new(window));
        let reopened = Database::from_store(store.clone(), &other).unwrap();
        assert_eq!(reopened.info().genesis_id, original.genesis_id);
        assert_eq!(
            store
                .begin_read()
                .unwrap()
                .get(SUPERBLOCK, b"history_floor")
                .unwrap(),
            Some(0u64.to_be_bytes().to_vec())
        );
    }
    let reopened = Database::from_store(store.clone(), &cfg).unwrap();
    assert_eq!(reopened.info().genesis_id, original.genesis_id);
    let param = CellKey::new(system::PARAMS.id, reserved::MAX_RECORD_CELLS);
    let tx = store.begin_read().unwrap();
    assert_eq!(
        CellReader::at(&tx, 0).get(&param).unwrap(),
        Some(CellValue::from_u64(100))
    );
    assert!(history_snapshot(&tx).iter().all(Vec::is_empty));
    drop(tx);
    let mut tx = store.begin_write().unwrap();
    tx.delete(cells::CELL, &param.encode()).unwrap();
    tx.commit().unwrap();
    assert!(Database::from_store(store.clone(), &cfg).is_err());
    assert!(
        store
            .begin_read()
            .unwrap()
            .get(cells::CELL, &param.encode())
            .unwrap()
            .is_none()
    );
}

#[test]
fn opening_requires_coverage_and_rejects_old_formats_without_repair() {
    metadata_checks(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    metadata_checks(MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn history_errors_during_seal_leave_the_branch_open_and_retryable() {
    let store = MemoryStore::new();
    Database::from_store(store.clone(), &config()).unwrap();
    let branches = Branches::new(store.clone(), Keccak256Hasher, None, None).unwrap();
    let b = branches.begin().unwrap();
    branches
        .write(b, |c| {
            c.put(key(64), value("A"));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    branches
        .write(b, |c| {
            c.put(key(64), value("B"));
            Ok::<_, ()>(())
        })
        .unwrap();
    let original = store
        .begin_read()
        .unwrap()
        .get(cells::CELL_HISTORY, &key(64).encode())
        .unwrap()
        .unwrap();
    let mut tx = store.begin_write().unwrap();
    tx.put(cells::CELL_HISTORY, &key(64).encode(), &[0])
        .unwrap();
    tx.commit().unwrap();
    let before = history_snapshot(&store.begin_read().unwrap());
    assert!(matches!(
        branches.seal(b),
        Err(BranchError::Cells(golemdb_cells::CellError::History(
            HistoryError::InvalidCommits { .. }
        )))
    ));
    assert!(!branches.branch_info(b).unwrap().sealed);
    assert_eq!(history_snapshot(&store.begin_read().unwrap()), before);
    assert_eq!(
        CellReader::new(&store.begin_read().unwrap())
            .get(&key(64))
            .unwrap(),
        Some(value("A"))
    );
    let mut tx = store.begin_write().unwrap();
    tx.put(cells::CELL_HISTORY, &key(64).encode(), &original)
        .unwrap();
    tx.commit().unwrap();
    // Index-history failure happens after cell history has already been staged.
    let k = index_key("A", 0).encode();
    let mut tx = store.begin_write().unwrap();
    let original = tx.get(index::INDEX_HISTORY, &k).unwrap().unwrap();
    tx.put(index::INDEX_HISTORY, &k, &[0]).unwrap();
    tx.commit().unwrap();
    let before = history_snapshot(&store.begin_read().unwrap());
    assert!(matches!(
        branches.seal(b),
        Err(BranchError::Index(golemdb_index::IndexError::History(
            HistoryError::InvalidCommits { .. }
        )))
    ));
    assert!(!branches.branch_info(b).unwrap().sealed);
    assert_eq!(history_snapshot(&store.begin_read().unwrap()), before);
    let mut tx = store.begin_write().unwrap();
    tx.put(index::INDEX_HISTORY, &k, &original).unwrap();
    tx.commit().unwrap();
    branches.commit(b).unwrap();
    assert_eq!(
        CellReader::at(&store.begin_read().unwrap(), 1)
            .get(&key(64))
            .unwrap(),
        Some(value("A"))
    );
}
