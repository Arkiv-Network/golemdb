use super::*;
use golemdb_storage::scan_prefix;
use std::fs;

// These deliberate store faults exercise defensive outcome resolution. A
// correct atomic store does not publish head with only part of its write set.
#[derive(Clone, Copy, Debug)]
pub(super) enum PublishedDamage {
    MissingRange,
    DifferentRange,
    MissingKey,
    DifferentKeyOrdinal,
    HeadMoved,
}
impl PublishedDamage {
    pub(super) fn apply(self, tx: &mut impl WriteTransaction) {
        let range_key = [0u32.to_be_bytes().as_slice(), &2u64.to_be_bytes()].concat();
        let forward = |n| [vec![0], 0u32.to_be_bytes().to_vec(), key(n).0.to_vec()].concat();
        let reverse = |n: u64| {
            [
                vec![1],
                0u32.to_be_bytes().to_vec(),
                n.to_be_bytes().to_vec(),
            ]
            .concat()
        };
        match self {
            Self::MissingRange => {
                tx.delete(metadata::COMMIT_RANGES, &range_key).unwrap();
            }
            Self::DifferentRange => {
                tx.put(
                    metadata::COMMIT_RANGES,
                    &range_key,
                    &[0u64.to_be_bytes(), 3u64.to_be_bytes()].concat(),
                )
                .unwrap();
            }
            Self::MissingKey => {
                tx.delete(metadata::KEYS, &forward(2)).unwrap();
                tx.delete(metadata::KEYS, &reverse(1)).unwrap();
            }
            Self::DifferentKeyOrdinal => {
                // Both directions remain consistent, but the two new keys
                // identify the opposite rows from those staged by this branch.
                for (key, ordinal) in [(2, 2u64), (3, 1u64)] {
                    tx.put(metadata::KEYS, &forward(key), &ordinal.to_be_bytes())
                        .unwrap();
                    tx.put(metadata::KEYS, &reverse(ordinal), &super::key(key).0)
                        .unwrap();
                }
            }
            Self::HeadMoved => {
                let head = golemdb_branch::read_head_state(tx).unwrap();
                let roots = golemdb_branch::head_roots(tx, &head, &Keccak256Hasher).unwrap();
                write_head(tx, CommitId::new(3), &roots, &Keccak256Hasher).unwrap();
            }
        }
    }
}

fn file_bytes(path: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut files: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn published_metadata(store: &impl Store) -> Vec<Vec<golemdb_storage::Entry>> {
    let tx = store.begin_read().unwrap();
    [
        Table("Superblock"),
        tables::CELL,
        metadata::KEYS,
        metadata::COMMIT_RANGES,
    ]
    .map(|table| {
        scan_prefix(&tx, table, vec![])
            .unwrap()
            .collect::<golemdb_storage::Result<Vec<_>>>()
            .unwrap()
    })
    .into()
}

fn file_failures(store: impl Store + Clone + 'static, path: &Path, during_sync: bool) {
    seed(&store);
    let store = fault_store(store);
    let storage = ImmutableDataStorage::open(config(), [7; 32], path).unwrap();
    let reader = storage.reader();
    let branches = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    commit_row(&branches, 1);
    let before = published_metadata(&store);
    let prefix = file_bytes(path);
    let pending = branches.begin().unwrap();
    branches
        .immutable_data_append(pending, "events", Some(key(2)), row(2))
        .unwrap();
    branches
        .immutable_data_append(pending, "receipts", Some(key(2)), row(3))
        .unwrap();
    let cell = CellKey::new(64, CellNameRef::raw(b"file-failure"));
    branches
        .write(pending, |cells| {
            cells.put(cell, CellValue::from_u64(9));
            Ok::<_, ()>(())
        })
        .unwrap();
    // Fail the second segment after the first has appended (or synced). Remove
    // the obstacle only when publication reacquires a writer to resolve failure.
    let obstacle = path.join(if during_sync {
        "segment-receipts-2-4.tmp"
    } else {
        "segment-receipts-2-4.jar"
    });
    let create = obstacle.clone();
    let event_file = path.join("segment-events-2-4.jar");
    let receipt_offsets = path.join("segment-receipts-2-4.off");
    // Key binding runs after each physical append. Inject only once the first
    // segment (append failure), or both segments (sync failure), have rows.
    store
        .faults
        .after_key_inserts
        .store(if during_sync { 4 } else { 2 }, Ordering::SeqCst);
    *store.faults.on_keys_written.lock().unwrap() =
        Some(Box::new(move || fs::create_dir(create).unwrap()));
    store.faults.before_write.lock().unwrap().extend([
        Box::new(|| ()) as Box<dyn FnOnce() + Send>,
        Box::new(move || {
            assert!(event_file.is_file());
            if during_sync {
                assert!(fs::metadata(&event_file).unwrap().len() > 0);
                assert!(fs::metadata(receipt_offsets).unwrap().len() > 9);
            }
            fs::remove_dir(obstacle).unwrap();
        }),
    ]);
    let result = branches.commit(pending);
    if during_sync {
        assert!(
            matches!(
                result,
                Err(BranchError::ImmutableData(
                    ImmutableDataStorageError::NippyJar(_)
                ))
            ),
            "{result:?}"
        );
    } else {
        assert!(
            matches!(
                result,
                Err(BranchError::ImmutableData(
                    ImmutableDataStorageError::Corrupt("shard path is not a regular file")
                ))
            ),
            "{result:?}"
        );
    }
    assert!(store.faults.before_write.lock().unwrap().is_empty());
    assert_eq!(published_metadata(&store), before);
    assert_eq!(file_bytes(path), prefix);
    assert!(branches.branch_info(pending).unwrap().sealed);
    assert_eq!(
        read(
            &store,
            &reader,
            "events",
            ImmutableDataAddress::Ordinal(0),
            &ImmutableDataReadOptions::all()
        )
        .unwrap(),
        row(1)
    );
    assert!(matches!(
        read(
            &store,
            &reader,
            "events",
            ImmutableDataAddress::Key(key(2)),
            &ImmutableDataReadOptions::all()
        ),
        Err(BranchError::ImmutableData(
            ImmutableDataStorageError::NotFound
        ))
    ));
    assert_eq!(branches.commit(pending).unwrap(), CommitId::new(2));
    let tx = store.begin_read().unwrap();
    assert_eq!(metadata::commit_range(&tx, 0, 2).unwrap(), Some(1..2));
    assert_eq!(metadata::commit_range(&tx, 1, 2).unwrap(), Some(0..1));
    assert_eq!(metadata::ordinal_for_key(&tx, 0, key(2)).unwrap(), Some(1));
    assert_eq!(metadata::ordinal_for_key(&tx, 1, key(2)).unwrap(), Some(0));
    drop(tx);
    assert_eq!(
        read(
            &store,
            &reader,
            "events",
            ImmutableDataAddress::Ordinal(1),
            &ImmutableDataReadOptions::all()
        )
        .unwrap(),
        row(2)
    );
    assert_eq!(
        read(
            &store,
            &reader,
            "receipts",
            ImmutableDataAddress::Ordinal(0),
            &ImmutableDataReadOptions::all()
        )
        .unwrap(),
        row(3)
    );
}

#[test]
fn file_append_and_sync_failures_restore_publication_and_allow_retry() {
    let _files = FILE_TESTS.read().unwrap();
    for during_sync in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        file_failures(
            MemoryStore::new(),
            &dir.path().join("memory-files"),
            during_sync,
        );
        file_failures(
            MdbxStore::open(dir.path().join("kv")).unwrap(),
            &dir.path().join("mdbx-files"),
            during_sync,
        );
    }
}

fn damaged_outcome(store: impl Store + Clone + 'static, path: &Path, damage: PublishedDamage) {
    seed(&store);
    let store = fault_store(store);
    let storage = ImmutableDataStorage::open(config(), [7; 32], path).unwrap();
    let reader = storage.reader();
    let branches = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    commit_row(&branches, 1);
    let pending = branches.begin().unwrap();
    for n in [2, 3] {
        branches
            .immutable_data_append(pending, "events", Some(key(n)), row(n.into()))
            .unwrap();
    }
    let expected_files = Arc::new(Mutex::new(None));
    let captured = expected_files.clone();
    let capture_path = path.to_owned();
    *store.faults.on_failure.lock().unwrap() = Some(Box::new(move || {
        *captured.lock().unwrap() = Some(file_bytes(&capture_path))
    }));
    *store.faults.published_damage.lock().unwrap() = Some(damage);
    store.faults.mode.store(2, Ordering::SeqCst);
    let result = branches.commit(pending);
    match result {
        Err(BranchError::PublicationRecovery {
            publication,
            recovery,
        }) => {
            assert!(
                matches!(*publication, BranchError::Storage(StorageError::Backend(_))),
                "{publication:?}"
            );
            assert!(
                matches!(*recovery, BranchError::RecoveryRequired),
                "{damage:?}: {recovery:?}"
            );
        }
        other => panic!("{damage:?}: {other:?}"),
    }
    // An uncertain outcome must retain every synced file, including new rows.
    assert_eq!(
        file_bytes(path),
        expected_files.lock().unwrap().take().unwrap()
    );
    assert_eq!(
        read(
            &store,
            &reader,
            "events",
            ImmutableDataAddress::Key(key(1)),
            &ImmutableDataReadOptions::all()
        )
        .unwrap(),
        row(1)
    );
    let blocked = branches.commit(branches.begin().unwrap());
    assert!(
        matches!(blocked, Err(BranchError::RecoveryRequired)),
        "{damage:?}: {blocked:?}"
    );
}

#[test]
fn inconsistent_published_outcomes_block_writes_without_truncating_files() {
    let _files = FILE_TESTS.read().unwrap();
    for damage in [
        PublishedDamage::MissingRange,
        PublishedDamage::DifferentRange,
        PublishedDamage::MissingKey,
        PublishedDamage::DifferentKeyOrdinal,
        PublishedDamage::HeadMoved,
    ] {
        let dir = tempfile::tempdir().unwrap();
        damaged_outcome(MemoryStore::new(), &dir.path().join("memory-files"), damage);
        damaged_outcome(
            MdbxStore::open(dir.path().join("kv")).unwrap(),
            &dir.path().join("mdbx-files"),
            damage,
        );
    }
}
