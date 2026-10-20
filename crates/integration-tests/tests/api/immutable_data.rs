use golemdb_api::*;
use golemdb_storage::{MemoryStore, ReadTransaction, Store};

fn genesis(hash: HashAlgorithm) -> Genesis {
    Genesis::new(
        hash,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 4,
            max_bytes_len: 4,
        },
        Some(ImmutableDataConfig::new(
            2,
            vec![
                ImmutableDataSegment {
                    name: "events".into(),
                    columns: vec![
                        ImmutableDataColumn::new("height").with_type(CellType::Uint(Width::W8)),
                        ImmutableDataColumn::new("hash")
                            .with_type(CellType::FixedBytes(Width::W32)),
                        ImmutableDataColumn::new("payload"),
                    ],
                    compression: ImmutableDataCompression::Lz4,
                },
                ImmutableDataSegment {
                    name: "receipts".into(),
                    columns: vec![ImmutableDataColumn::new("value")],
                    compression: ImmutableDataCompression::None,
                },
            ],
        )),
    )
}
fn row(n: u8) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![
        CellValue::from_u64(u64::from(n)),
        CellValue::from_bytes32([n; 32]),
        CellValue::from_bytes(&[n; 128]),
    ])
}
fn key(n: u8) -> ImmutableDataKey {
    ImmutableDataKey([n; 32])
}
fn ordinal(n: u64) -> ImmutableDataAddress {
    ImmutableDataAddress::Ordinal(n)
}
fn all() -> ImmutableDataReadOptions {
    ImmutableDataReadOptions::all()
}
fn assert_row(actual: ImmutableDataRow, n: u8) {
    assert_eq!(actual, row(n));
    assert_eq!(actual.get::<u64>("height").unwrap(), u64::from(n));
    assert_eq!(actual.get::<[u8; 32]>("hash").unwrap(), [n; 32]);
    assert_eq!(actual.get::<Vec<u8>>("payload").unwrap(), vec![n; 128]);
}

fn contract(db: &Database) {
    let clone = db.clone();
    let api: &dyn Api = &clone;
    let schema = api.immutable_data_schema("events").unwrap();
    assert_eq!(schema.columns().len(), 3);
    assert_eq!(
        schema.columns()[1].value_type,
        CellType::FixedBytes(Width::W32)
    );
    assert!(
        api.immutable_data_rows_of("events", CommitId::GENESIS, &all())
            .unwrap()
            .is_empty()
    );
    let branch = api.begin().unwrap();
    let loser = api.begin().unwrap();
    assert_eq!(
        api.immutable_data_append(branch, "events", Some(key(1)), row(1))
            .unwrap(),
        0
    );
    assert_eq!(
        api.immutable_data_append(loser, "events", Some(key(1)), row(9))
            .unwrap(),
        0
    );
    for address in [ordinal(0), ImmutableDataAddress::Key(key(1))] {
        assert!(matches!(
            api.immutable_data_get("events", address, &all()),
            Err(ApiError::NotFound)
        ));
    }
    api.checkpoint(branch).unwrap();
    assert_eq!(
        api.immutable_data_append(branch, "events", Some(key(2)), row(2))
            .unwrap(),
        1
    );
    api.rollback(branch).unwrap(); // immutable rows are independent of cell frames
    let seal = api.seal(branch).unwrap();
    assert_eq!(
        api.immutable_data_append(branch, "events", None, row(3))
            .unwrap(),
        2
    );
    api.immutable_data_truncate(branch, "events", 1).unwrap();
    assert_eq!(
        api.immutable_data_append(branch, "events", Some(key(2)), row(4))
            .unwrap(),
        1
    );
    assert_eq!(api.seal(branch).unwrap(), seal);
    let first = api.commit(branch).unwrap();
    assert!(matches!(api.commit(loser), Err(ApiError::Conflict)));
    assert!(matches!(
        api.immutable_data_truncate(branch, "events", 0),
        Err(ApiError::HandleInvalid)
    ));
    assert_row(
        api.immutable_data_get("events", ordinal(0), &all())
            .unwrap(),
        1,
    );
    assert_row(
        api.immutable_data_get("events", ImmutableDataAddress::Key(key(2)), &all())
            .unwrap(),
        4,
    );
    let rows = api.immutable_data_rows_of("events", first, &all()).unwrap();
    assert_eq!(rows.len(), 2);
    assert_row(rows[0].clone(), 1);
    assert_row(rows[1].clone(), 4);
    assert!(
        api.immutable_data_rows_of("receipts", first, &all())
            .unwrap()
            .is_empty()
    );

    let projection = ImmutableDataReadOptions::columns(["hash", "height"]);
    let projected = api
        .immutable_data_get("events", ordinal(0), &projection)
        .unwrap();
    assert_eq!(projected.get::<[u8; 32]>("hash").unwrap(), [1; 32]);
    assert_eq!(
        projected
            .iter()
            .unwrap()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["hash", "height"]
    );
    assert!(matches!(
        projected.value("payload"),
        Err(ImmutableDataError::ColumnNotSelected { .. })
    ));
    assert!(projected.get::<u32>("height").is_err());
    let rows = api
        .immutable_data_rows_of("events", first, &projection)
        .unwrap();
    assert_eq!(rows[1].get::<u64>("height").unwrap(), 4);
    assert_eq!(rows[1].values().len(), 2);

    let empty = api.commit(api.begin().unwrap()).unwrap();
    assert!(
        api.immutable_data_rows_of("events", empty, &all())
            .unwrap()
            .is_empty()
    );
    assert!(
        matches!(api.immutable_data_rows_of("events", CommitId::new(empty.get() + 1), &all()),
        Err(ApiError::CommitUnavailable { requested, head }) if requested.get() == empty.get() + 1 && head == empty)
    );
    let branch = api.begin().unwrap();
    assert_eq!(
        api.immutable_data_append(branch, "events", None, row(5))
            .unwrap(),
        2
    );
    api.commit(branch).unwrap(); // rotate files by commit span, with continuous ordinals
    assert_row(
        api.immutable_data_get("events", ordinal(2), &all())
            .unwrap(),
        5,
    );
    assert_eq!(
        api.immutable_data_rows_of("events", first, &all())
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn typed_rows_projection_and_branch_lifecycle_in_memory_and_files() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let genesis = genesis(hash);
        contract(&Database::open_memory(&genesis).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let config = OpenConfig::new(genesis);
        {
            let db = Database::open(dir.path(), &config).unwrap();
            contract(&db);
        }
        let db = Database::open(
            dir.path(),
            &config.clone().with_mode(OpenMode::ExistingOnly),
        )
        .unwrap();
        assert_row(
            db.immutable_data_get("events", ordinal(0), &all()).unwrap(),
            1,
        );
        assert_row(
            db.immutable_data_get("events", ordinal(2), &all()).unwrap(),
            5,
        );
        assert!(
            dir.path()
                .join("immutable-data/segment-events-2-4.jar")
                .is_file()
        );
    }
}

#[test]
fn invalid_inputs_duplicates_and_disabled_segments_have_public_errors() {
    let db = Database::open_memory(&genesis(HashAlgorithm::Keccak256)).unwrap();
    let id = db.begin().unwrap();
    for invalid in [
        ImmutableDataRow::new(vec![]),
        ImmutableDataRow::new(vec![
            CellValue::from_u32(1),
            CellValue::from_bytes32([1; 32]),
            CellValue::from_bytes(&[]),
        ]),
    ] {
        assert!(matches!(
            db.immutable_data_append(id, "events", None, invalid),
            Err(ApiError::InvalidArgument { .. })
        ));
    }
    db.immutable_data_append(id, "events", Some(key(1)), row(1))
        .unwrap();
    assert!(matches!(
        db.immutable_data_append(id, "events", Some(key(1)), row(2)),
        Err(ApiError::AlreadyExists)
    ));
    assert!(matches!(
        db.immutable_data_truncate(id, "events", 2),
        Err(ApiError::InvalidArgument { .. })
    ));
    db.commit(id).unwrap();
    let id = db.begin().unwrap();
    assert!(matches!(
        db.immutable_data_append(id, "events", Some(key(1)), row(2)),
        Err(ApiError::AlreadyExists)
    ));
    assert!(matches!(
        db.immutable_data_truncate(id, "events", 0),
        Err(ApiError::InvalidArgument { .. })
    ));
    for options in [
        ImmutableDataReadOptions::columns(["missing"]),
        ImmutableDataReadOptions::columns(["height", "height"]),
        ImmutableDataReadOptions::columns(Vec::<String>::new()),
    ] {
        assert!(matches!(
            db.immutable_data_get("events", ordinal(0), &options),
            Err(ApiError::InvalidArgument { .. })
        ));
        assert!(matches!(
            db.immutable_data_rows_of("events", CommitId::new(1), &options),
            Err(ApiError::InvalidArgument { .. })
        ));
    }
    assert!(matches!(
        db.immutable_data_get("events", ordinal(u64::MAX), &all()),
        Err(ApiError::NotFound)
    ));
    db.immutable_data_append(id, "events", Some(key(2)), row(2))
        .unwrap();
    db.discard(id).unwrap();
    let id = db.begin().unwrap();
    assert_eq!(
        db.immutable_data_append(id, "events", Some(key(2)), row(2))
            .unwrap(),
        1
    );
    db.commit(id).unwrap();

    let mut disabled = genesis(HashAlgorithm::Keccak256);
    disabled.immutable_data = None;
    let dir = tempfile::tempdir().unwrap();
    let disabled = Database::open(dir.path(), &OpenConfig::new(disabled)).unwrap();
    assert!(!dir.path().join("immutable-data").exists());
    for db in [&db, &disabled] {
        assert!(matches!(
            db.immutable_data_schema("unknown"),
            Err(ApiError::NotFound)
        ));
        assert!(matches!(
            db.immutable_data_get("unknown", ordinal(0), &all()),
            Err(ApiError::NotFound)
        ));
        assert!(matches!(
            db.immutable_data_rows_of("unknown", CommitId::GENESIS, &all()),
            Err(ApiError::NotFound)
        ));
        let branch = db.begin().unwrap();
        assert!(matches!(
            db.immutable_data_append(branch, "unknown", None, row(1)),
            Err(ApiError::NotFound)
        ));
        assert!(matches!(
            db.immutable_data_truncate(branch, "unknown", 0),
            Err(ApiError::NotFound)
        ));
        db.discard(branch).unwrap();
    }
}

#[test]
fn supplied_stores_require_a_directory_and_recover_unpublished_file_tails() {
    let dir = tempfile::tempdir().unwrap();
    let config = OpenConfig::new(genesis(HashAlgorithm::Keccak256));
    let store = MemoryStore::new();
    assert!(matches!(
        Database::from_store(store.clone(), &config),
        Err(OpenError::InvalidConfig(_))
    ));
    assert!(
        store
            .begin_read()
            .unwrap()
            .get(golemdb_storage::Table("Superblock"), b"head")
            .unwrap()
            .is_none()
    );
    let config = config.with_immutable_data_directory(dir.path().join("files"));
    let info = {
        let db = Database::from_store(store.clone(), &config).unwrap();
        let id = db.begin().unwrap();
        db.immutable_data_append(id, "events", Some(key(1)), row(1))
            .unwrap();
        db.commit(id).unwrap();
        *db.info()
    };
    {
        let mut files = golemdb_immutable_data::ImmutableDataStorage::open(
            config.genesis.immutable_data.clone().unwrap(),
            info.genesis_id,
            dir.path().join("files"),
        )
        .unwrap();
        files.append("events", 2, &row(9)).unwrap();
        files.sync().unwrap();
    }
    let db = Database::from_store(store.clone(), &config).unwrap();
    assert_row(
        db.immutable_data_get("events", ordinal(0), &all()).unwrap(),
        1,
    );
    assert!(matches!(
        db.immutable_data_get("events", ordinal(1), &all()),
        Err(ApiError::NotFound)
    ));
    let id = db.begin().unwrap();
    assert_eq!(
        db.immutable_data_append(id, "events", None, row(2))
            .unwrap(),
        1
    );
    db.commit(id).unwrap();
    assert!(!dir.path().join("immutable-data").exists());
}

#[test]
fn failed_immutable_initialization_releases_directory_for_retry() {
    use golemdb_immutable_data::ImmutableDataStorageError;

    let dir = tempfile::tempdir().unwrap();
    let config = OpenConfig::new(genesis(HashAlgorithm::Keccak256))
        .with_immutable_data_directory(dir.path());
    let store = MemoryStore::new();
    let db = Database::from_store(store.clone(), &config).unwrap();
    let branch = db.begin().unwrap();
    db.immutable_data_append(branch, "events", Some(key(1)), row(1))
        .unwrap();
    db.commit(branch).unwrap();
    assert!(matches!(
        Database::from_store(store.clone(), &config),
        Err(OpenError::ImmutableData(
            ImmutableDataStorageError::Invalid(_)
        ))
    ));
    drop(db);

    // Recovery fails after the API has obtained a reader from the opened storage.
    // Both reader and writer must be dropped so restoring the file allows retry.
    let offsets = dir.path().join("segment-events-0-2.off");
    let saved = std::fs::read(&offsets).unwrap();
    std::fs::remove_file(&offsets).unwrap();
    assert!(matches!(
        Database::from_store(store.clone(), &config),
        Err(OpenError::Branch(
            golemdb_branch::BranchError::ImmutableData(_)
        ))
    ));
    std::fs::write(offsets, saved).unwrap();
    let db = Database::from_store(store, &config).unwrap();
    assert_row(
        db.immutable_data_get("events", ImmutableDataAddress::Key(key(1)), &all())
            .unwrap(),
        1,
    );
}

// Pause after acquiring the read snapshot, so another clone can commit before
// the API reads head and metadata. Both must still describe the original snapshot.
#[derive(Clone)]
struct PausingStore<S> {
    store: S,
    pause: std::sync::Arc<std::sync::Mutex<Option<ReadPause>>>,
}
type ReadPause = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
impl<S: Store> Store for PausingStore<S> {
    type Read<'a>
        = S::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = S::Write<'a>
    where
        Self: 'a;
    fn max_key_size(&self) -> usize {
        self.store.max_key_size()
    }
    fn max_value_size(&self) -> usize {
        self.store.max_value_size()
    }
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        let snapshot = self.store.begin_read()?;
        let pause = self.pause.lock().unwrap().take();
        if let Some((entered, resume)) = pause {
            entered.send(()).unwrap();
            resume
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        }
        Ok(snapshot)
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        self.store.begin_write()
    }
}

#[test]
fn committed_reads_use_head_and_metadata_from_one_snapshot() {
    for by_commit in [false, true] {
        let pause = std::sync::Arc::new(std::sync::Mutex::new(None));
        let dir = tempfile::tempdir().unwrap();
        let db = Database::from_store(
            PausingStore {
                store: MemoryStore::new(),
                pause: pause.clone(),
            },
            &OpenConfig::new(genesis(HashAlgorithm::Keccak256))
                .with_immutable_data_directory(dir.path()),
        )
        .unwrap();
        let branch = db.begin().unwrap();
        db.immutable_data_append(branch, "events", Some(key(1)), row(1))
            .unwrap();
        let reader = db.clone();
        let (entered, waiting) = std::sync::mpsc::sync_channel(0);
        let (resume, paused) = std::sync::mpsc::sync_channel(0);
        *pause.lock().unwrap() = Some((entered, paused));
        let reading = std::thread::spawn(move || {
            if by_commit {
                reader.immutable_data_rows_of("events", CommitId::new(1), &all())
            } else {
                reader
                    .immutable_data_get("events", ordinal(0), &all())
                    .map(|row| vec![row])
            }
        });
        waiting
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        db.commit(branch).unwrap();
        resume.send(()).unwrap();
        let result = reading.join().unwrap();
        if by_commit {
            assert!(
                matches!(result, Err(ApiError::CommitUnavailable { head, .. }) if head == CommitId::GENESIS)
            );
        } else {
            assert!(matches!(result, Err(ApiError::NotFound)));
        }
        assert_row(
            db.immutable_data_get("events", ordinal(0), &all()).unwrap(),
            1,
        );
    }
}

#[test]
fn range_of_and_ordinal_of_locate_committed_rows() {
    let db = Database::open_memory(&genesis(HashAlgorithm::Keccak256)).unwrap();
    assert_eq!(
        db.immutable_data_range_of("events", CommitId::GENESIS)
            .unwrap(),
        0..0
    );

    let branch = db.begin().unwrap();
    db.immutable_data_append(branch, "events", Some(key(1)), row(1))
        .unwrap();
    db.immutable_data_append(branch, "events", None, row(2))
        .unwrap();
    assert_eq!(
        db.immutable_data_ordinal_of("events", key(1)).unwrap(),
        None
    );
    let first = db.commit(branch).unwrap();

    // A commit that appends nothing still reports where the segment ends.
    let empty = db.commit(db.begin().unwrap()).unwrap();

    let branch = db.begin().unwrap();
    db.immutable_data_append(branch, "events", Some(key(3)), row(3))
        .unwrap();
    let third = db.commit(branch).unwrap();

    assert_eq!(db.immutable_data_range_of("events", first).unwrap(), 0..2);
    assert_eq!(db.immutable_data_range_of("events", empty).unwrap(), 2..2);
    assert_eq!(db.immutable_data_range_of("events", third).unwrap(), 2..3);
    assert_eq!(db.immutable_data_range_of("receipts", third).unwrap(), 0..0);
    assert!(matches!(
        db.immutable_data_range_of("events", CommitId::new(third.get() + 1)),
        Err(ApiError::CommitUnavailable { .. })
    ));
    assert_eq!(
        db.immutable_data_ordinal_of("events", key(1)).unwrap(),
        Some(0)
    );
    assert_eq!(
        db.immutable_data_ordinal_of("events", key(3)).unwrap(),
        Some(2)
    );
    assert_eq!(
        db.immutable_data_ordinal_of("events", key(9)).unwrap(),
        None
    );
    assert!(matches!(
        db.immutable_data_ordinal_of("missing", key(1)),
        Err(ApiError::NotFound)
    ));
}
