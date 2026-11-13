use super::*;
use golemdb_cells::{CellKey, CellNameRef, CellType, CellValue, Width};
use golemdb_immutable_data::{
    ImmutableDataColumn, ImmutableDataCompression, ImmutableDataConfig, ImmutableDataSegment,
    ImmutableDataStorageError as DataError, metadata,
};
use golemdb_merkle::Keccak256Hasher;
use golemdb_storage::{MemoryStore, Table, WriteTransaction, scan_prefix};
use std::sync::atomic::AtomicBool;

fn config() -> ImmutableDataConfig {
    ImmutableDataConfig::new(
        4,
        ["events", "receipts"]
            .map(|name| ImmutableDataSegment {
                name: name.into(),
                columns: vec![
                    ImmutableDataColumn::new("number").with_type(CellType::Uint(Width::W8)),
                ],
                compression: ImmutableDataCompression::None,
            })
            .into(),
    )
}
fn row(number: u64) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![CellValue::from_u64(number)])
}
fn key(n: u8) -> ImmutableDataKey {
    ImmutableDataKey([n; 32])
}
fn head(tx: &mut impl WriteTransaction, commit: u64) {
    let empty = crate::Roots {
        state: golemdb_merkle::RootRef::Empty,
        index: golemdb_merkle::RootRef::Empty,
    };
    crate::write_head(tx, CommitId::new(commit), &empty, &Keccak256Hasher).unwrap();
}
fn store(commit: u64) -> MemoryStore {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    head(&mut tx, commit);
    tx.commit().unwrap();
    db
}
fn staging<S: Store>(branches: &Branches<S, Keccak256Hasher>, id: BranchId) -> ImmutableStaging {
    branches
        .with_branch(id, |state, _| state.immutable_data.clone())
        .unwrap()
}
fn entry<S: Store>(branches: &Branches<S, Keccak256Hasher>, id: BranchId) -> Entry {
    branches
        .inner
        .branches
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .clone()
}
fn append<S: Store>(branches: &Branches<S, Keccak256Hasher>, id: BranchId, n: u8) -> u64 {
    branches
        .immutable_data_append(id, "events", Some(key(n)), row(n.into()))
        .unwrap()
}
fn snapshot(db: &impl Store) -> Vec<Vec<golemdb_storage::Entry>> {
    let tx = db.begin_read().unwrap();
    [metadata::KEYS, metadata::COMMIT_RANGES, Table("Superblock")]
        .map(|table| {
            scan_prefix(&tx, table, vec![])
                .unwrap()
                .collect::<golemdb_storage::Result<Vec<_>>>()
                .unwrap()
        })
        .into()
}

// Synthetic metadata covers enormous ordinals and corruption without allocating
// physical rows. Only staging admission is exercised by these two fixtures.
fn synthetic_metadata_branches(db: MemoryStore) -> Branches<MemoryStore, Keccak256Hasher> {
    let mut branches = Branches::new(db, Keccak256Hasher, None, None).unwrap();
    Arc::get_mut(&mut branches.inner)
        .unwrap()
        .immutable_segments = ImmutableSegments::new(config()).unwrap();
    branches
}

#[test]
fn publication_checks_staged_end_against_metadata_and_physical_rows() {
    for physical_mismatch in [false, true] {
        let db = store(0);
        let storage = ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap();
        let branches = Branches::new(db.clone(), Keccak256Hasher, Some(storage), None).unwrap();
        let first = branches.begin().unwrap();
        append(&branches, first, 1);
        branches.commit(first).unwrap();
        let pending = branches.begin().unwrap();
        assert_eq!(append(&branches, pending, 2), 1);
        if physical_mismatch {
            // Published metadata and staged start agree; only the physical tail
            // is ahead. The later append-ordinal check must not mask this guard.
            branches
                .inner
                .immutable_writer
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .storage
                .append("events", 2, &row(99))
                .unwrap();
        } else {
            // Physical end and staged start agree. Simulate metadata disappearing
            // after staging without moving head, isolating the metadata check.
            let mut tx = db.begin_write().unwrap();
            let range_key = [0u32.to_be_bytes().as_slice(), &1u64.to_be_bytes()].concat();
            tx.delete(metadata::COMMIT_RANGES, &range_key).unwrap();
            tx.commit().unwrap();
        }
        assert!(matches!(
            branches.commit(pending),
            Err(BranchError::ImmutableData(DataError::Corrupt(
                "staged ordinal disagrees with published or physical end"
            )))
        ));
        assert_eq!(branches.head().unwrap(), CommitId::new(1));
        assert!(branches.branch_info(pending).unwrap().sealed);
        let tx = db.begin_read().unwrap();
        assert_eq!(metadata::commit_range(&tx, 0, 2).unwrap(), None);
        assert_eq!(metadata::ordinal_for_key(&tx, 0, key(2)).unwrap(), None);
        drop(tx);
        if physical_mismatch {
            // Recovery removes the extra physical row, retaining the pending
            // branch's ordinal so retry publishes the originally staged value.
            assert_eq!(branches.commit(pending).unwrap(), CommitId::new(2));
        }
    }
}

#[test]
fn staging_is_independent_of_cell_frames_and_remains_editable_after_seal() {
    let db = store(0);
    let storage = ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap();
    let reader = storage.reader();
    let branches = Branches::new(db.clone(), Keccak256Hasher, Some(storage), None).unwrap();
    let id = branches.begin().unwrap();
    let cell = CellKey::new(64, CellNameRef::raw(b"value"));
    branches
        .write(id, |cells| {
            cells.put(cell.clone(), CellValue::from_u64(1));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.checkpoint(id).unwrap();
    assert_eq!(append(&branches, id, 1), 0);
    let pending = staging(&branches, id);
    branches
        .write(id, |cells| {
            cells.put(cell.clone(), CellValue::from_u64(2));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.rollback(id).unwrap();
    assert_eq!(staging(&branches, id), pending);
    assert_eq!(
        branches.read(id, |cells| cells.get(&cell)).unwrap(),
        Some(CellValue::from_u64(1))
    );
    assert!(matches!(
        branches.immutable_data_append(id, "events", Some(key(1)), row(1)),
        Err(BranchError::ImmutableData(DataError::AlreadyExists))
    ));
    let version = branches.branch_info(id).unwrap().version;
    let sealed = branches.seal(id).unwrap();
    assert_eq!(append(&branches, id, 2), 1);
    branches.immutable_data_truncate(id, "events", 1).unwrap();
    assert_eq!(staging(&branches, id), pending);
    assert_eq!(append(&branches, id, 2), 1);
    assert_eq!(branches.branch_info(id).unwrap().version, version);
    assert!(Arc::ptr_eq(&sealed, &branches.seal(id).unwrap()));
    assert!(matches!(branches.rollback(id), Err(BranchError::Sealed)));
    assert!(matches!(branches.checkpoint(id), Err(BranchError::Sealed)));
    assert!(matches!(
        branches.write(id, |_| Ok::<_, ()>(())),
        Err(OperationError::Branch(BranchError::Sealed))
    ));

    let held = entry(&branches, id);
    assert_eq!(branches.commit(id).unwrap(), sealed.commit_id);
    let tx = db.begin_read().unwrap();
    assert_eq!(
        reader
            .rows_of(
                &tx,
                sealed.commit_id.get(),
                "events",
                sealed.commit_id.get(),
                &golemdb_immutable_data::ImmutableDataReadOptions::all()
            )
            .unwrap()
            .len(),
        2
    );
    assert!(held.lock().unwrap().is_none());
    assert!(matches!(
        branches.branch_info(id),
        Err(BranchError::HandleInvalid)
    ));
}

#[test]
fn branch_local_reservations_and_lifecycle_release() {
    let db = store(0);
    let branches = Branches::new(
        db,
        Keccak256Hasher,
        Some(ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap()),
        None,
    )
    .unwrap();
    let a = branches.begin().unwrap();
    let b = branches.begin().unwrap();
    assert_eq!(append(&branches, a, 1), 0);
    assert_eq!(append(&branches.clone(), b, 1), 0);
    assert_eq!(
        branches
            .immutable_data_append(a, "receipts", Some(key(1)), row(9))
            .unwrap(),
        0
    );
    branches.immutable_data_truncate(a, "events", 0).unwrap();
    assert!(matches!(
        branches.immutable_data_append(b, "events", Some(key(1)), row(2)),
        Err(BranchError::ImmutableData(DataError::AlreadyExists))
    ));
    assert_eq!(append(&branches, a, 1), 0);
    let held = entry(&branches, a);
    branches.discard(a).unwrap();
    assert!(held.lock().unwrap().is_none());
    assert!(matches!(
        branches.immutable_data_truncate(a, "events", 0),
        Err(BranchError::HandleInvalid)
    ));
    let c = branches.begin().unwrap();
    assert_eq!(append(&branches, c, 1), 0);
    let d = branches.begin().unwrap();
    assert_eq!(append(&branches, d, 1), 0);
    let e = branches.begin().unwrap();
    assert_eq!(append(&branches, e, 1), 0);
    let entries = [b, c, d, e].map(|id| entry(&branches, id));
    let winner = branches.begin().unwrap();
    branches.commit(winner).unwrap();
    assert!(matches!(branches.commit(b), Err(BranchError::Conflict)));
    assert!(matches!(
        branches.immutable_data_append(c, "events", None, row(3)),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.immutable_data_truncate(d, "events", 0),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.discard(e),
        Err(BranchError::HandleInvalid)
    ));
    for held in entries {
        assert!(held.lock().unwrap().is_none());
    }
    assert!(branches.inner.branches.lock().unwrap().is_empty());
}

#[test]
fn published_ranges_keys_and_invalid_operations_preserve_pending_rows() {
    let db = store(9);
    let first_ordinal = u32::MAX as u64 + 10;
    let mut tx = db.begin_write().unwrap();
    metadata::record_commit_range(&mut tx, 0, 3, 0..first_ordinal).unwrap();
    metadata::bind_key(&mut tx, 0, key(1), first_ordinal - 1).unwrap();
    metadata::record_commit_range(&mut tx, 1, 8, 0..4).unwrap();
    tx.commit().unwrap();
    let before = snapshot(&db);
    let branches = synthetic_metadata_branches(db.clone());
    let id = branches.begin().unwrap();
    branches
        .immutable_data_truncate(id, "events", first_ordinal)
        .unwrap();
    assert_eq!(
        branches
            .immutable_data_append(id, "receipts", Some(key(1)), row(8))
            .unwrap(),
        4
    );
    assert_eq!(append(&branches, id, 2), first_ordinal);
    assert_eq!(
        branches
            .immutable_data_append(id, "events", None, row(3))
            .unwrap(),
        first_ordinal + 1
    );
    let pending = staging(&branches, id);
    for key in [key(1), key(2)] {
        assert!(matches!(
            branches.immutable_data_append(id, "events", Some(key), row(0)),
            Err(BranchError::ImmutableData(DataError::AlreadyExists))
        ));
    }
    for bad in [
        ImmutableDataRow::default(),
        ImmutableDataRow::new(vec![CellValue::from_str("bad")]),
    ] {
        assert!(matches!(
            branches.immutable_data_append(id, "events", Some(key(4)), bad),
            Err(BranchError::ImmutableData(DataError::Input(_)))
        ));
    }
    assert!(matches!(
        branches.immutable_data_append(id, "missing", None, row(0)),
        Err(BranchError::ImmutableData(DataError::NotFound))
    ));
    assert!(matches!(
        branches.immutable_data_truncate(id, "missing", 0),
        Err(BranchError::ImmutableData(DataError::NotFound))
    ));
    for from in [0, first_ordinal - 1, first_ordinal + 3, u64::MAX] {
        assert!(matches!(
            branches.immutable_data_truncate(id, "events", from),
            Err(BranchError::ImmutableData(DataError::Invalid(_)))
        ));
    }
    assert_eq!(staging(&branches, id), pending);
    branches
        .immutable_data_truncate(id, "events", first_ordinal + 2)
        .unwrap();
    assert_eq!(staging(&branches, id), pending);
    assert_eq!(append(&branches, id, 4), first_ordinal + 2);
    branches
        .immutable_data_truncate(id, "events", first_ordinal)
        .unwrap();
    assert_eq!(append(&branches, id, 2), first_ordinal);
    assert_eq!(snapshot(&db), before);
    assert_eq!(branches.branch_info(id).unwrap().version, 0);
}

/// After initialization, staging and seal must never open a storage writer.
struct ReadOnly(MemoryStore, Arc<AtomicBool>);
impl Store for ReadOnly {
    type Read<'a> = <MemoryStore as Store>::Read<'a>;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        self.0.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        assert!(!self.1.load(Ordering::Relaxed), "unexpected writer");
        self.0.begin_write()
    }
}

#[test]
fn staging_and_seal_do_not_open_a_writer() {
    let read_only = Arc::new(AtomicBool::new(false));
    let branches = Branches::new(
        ReadOnly(store(0), read_only.clone()),
        Keccak256Hasher,
        Some(ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap()),
        None,
    )
    .unwrap();
    read_only.store(true, Ordering::Relaxed);
    let id = branches.begin().unwrap();
    append(&branches, id, 1);
    assert!(!branches.branch_info(id).unwrap().sealed);
    branches.seal(id).unwrap();
    append(&branches, id, 2);
    branches.immutable_data_truncate(id, "events", 1).unwrap();
    branches.discard(id).unwrap();
}

#[test]
fn ordinal_overflow_and_corrupt_metadata_do_not_reserve_keys_or_rows() {
    let db = store(1);
    let mut tx = db.begin_write().unwrap();
    metadata::record_commit_range(&mut tx, 0, 1, 0..u64::MAX).unwrap();
    metadata::bind_key(&mut tx, 1, key(1), 0).unwrap(); // no corresponding published range
    tx.commit().unwrap();
    let branches = synthetic_metadata_branches(db);
    let id = branches.begin().unwrap();
    assert!(matches!(
        branches.immutable_data_append(id, "events", Some(key(1)), row(1)),
        Err(BranchError::ImmutableData(DataError::Overflow))
    ));
    assert!(matches!(
        branches.immutable_data_append(id, "receipts", Some(key(1)), row(1)),
        Err(BranchError::ImmutableData(DataError::Corrupt(_)))
    ));
    assert_eq!(staging(&branches, id), ImmutableStaging::default());
    branches
        .immutable_data_truncate(id, "events", u64::MAX)
        .unwrap();
    assert_eq!(
        branches
            .immutable_data_append(id, "receipts", Some(key(2)), row(2))
            .unwrap(),
        0
    );
}

struct AdvanceOnRead {
    db: MemoryStore,
    advance: Arc<AtomicBool>,
}
impl Store for AdvanceOnRead {
    type Read<'a> = <MemoryStore as Store>::Read<'a>;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        let snapshot = self.db.begin_read()?;
        if self.advance.swap(false, Ordering::SeqCst) {
            let mut tx = self.db.begin_write()?;
            metadata::record_commit_range(&mut tx, 0, 1, 0..1).unwrap();
            metadata::bind_key(&mut tx, 0, key(1), 0).unwrap();
            head(&mut tx, 1);
            tx.commit()?;
        }
        Ok(snapshot)
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        self.db.begin_write()
    }
}

#[test]
fn head_key_and_first_ordinal_use_the_same_snapshot() {
    let advance = Arc::new(AtomicBool::new(false));
    let branches = Branches::new(
        AdvanceOnRead {
            db: store(0),
            advance: advance.clone(),
        },
        Keccak256Hasher,
        Some(ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap()),
        None,
    )
    .unwrap();
    let id = branches.begin().unwrap();
    let held = entry(&branches, id);
    advance.store(true, Ordering::SeqCst);
    assert_eq!(append(&branches, id, 1), 0); // admitted snapshot predates publication
    assert_eq!(branches.head().unwrap().get(), 1);
    assert!(matches!(
        branches.immutable_data_append(id, "events", None, row(2)),
        Err(BranchError::HandleInvalid)
    ));
    assert!(held.lock().unwrap().is_none());
}

#[test]
fn concurrent_appends_share_reservations_and_allocate_distinct_ordinals() {
    let branches = Branches::new(
        store(0),
        Keccak256Hasher,
        Some(ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap()),
        None,
    )
    .unwrap();
    let id = branches.begin().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let branches = branches.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let keyed = branches.immutable_data_append(id, "events", Some(key(1)), row(1));
                let ordinals: Vec<_> = (0..8)
                    .map(|i| {
                        branches
                            .immutable_data_append(id, "events", None, row(i))
                            .unwrap()
                    })
                    .collect();
                (keyed, ordinals)
            })
        })
        .collect();
    let mut ordinals = Vec::new();
    let mut successes = 0;
    for worker in workers {
        let (keyed, unkeyed) = worker.join().unwrap();
        match keyed {
            Ok(ordinal) => {
                ordinals.push(ordinal);
                successes += 1;
            }
            Err(BranchError::ImmutableData(DataError::AlreadyExists)) => (),
            other => panic!("unexpected result: {other:?}"),
        }
        ordinals.extend(unkeyed);
    }
    assert_eq!(successes, 1);
    ordinals.sort_unstable();
    assert_eq!(ordinals, (0..17).collect::<Vec<_>>());
}

struct FailMetadata {
    db: MemoryStore,
    fail: Arc<AtomicBool>,
}
struct MetadataRead<T> {
    inner: T,
    fail: Arc<AtomicBool>,
}
impl<T: ReadTransaction> ReadTransaction for MetadataRead<T> {
    type Cursor<'a>
        = T::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if table == metadata::KEYS && self.fail.load(Ordering::SeqCst) {
            return Err(golemdb_storage::StorageError::Backend(
                "injected key read failure".into(),
            ));
        }
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        if table == metadata::COMMIT_RANGES && self.fail.load(Ordering::SeqCst) {
            return Err(golemdb_storage::StorageError::Backend(
                "injected range read failure".into(),
            ));
        }
        self.inner.cursor(table, key)
    }

    fn with_value<Output>(
        &self,
        table: golemdb_storage::Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> Output,
    ) -> golemdb_storage::Result<Output> {
        if table == metadata::KEYS && self.fail.load(Ordering::SeqCst) {
            return Err(golemdb_storage::StorageError::Backend(
                "injected key read failure".into(),
            ));
        }
        self.inner.with_value(table, key, inspect)
    }
}
impl Store for FailMetadata {
    type Read<'a> = MetadataRead<<MemoryStore as Store>::Read<'a>>;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        Ok(MetadataRead {
            inner: self.db.begin_read()?,
            fail: self.fail.clone(),
        })
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        self.db.begin_write()
    }
}

#[test]
fn metadata_read_errors_leave_rows_keys_and_ordinals_retryable() {
    let fail = Arc::new(AtomicBool::new(false));
    let branches = Branches::new(
        FailMetadata {
            db: store(0),
            fail: fail.clone(),
        },
        Keccak256Hasher,
        Some(ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap()),
        None,
    )
    .unwrap();
    let id = branches.begin().unwrap();
    assert_eq!(append(&branches, id, 1), 0);
    let before = staging(&branches, id);
    fail.store(true, Ordering::SeqCst);
    for segment in ["events", "receipts"] {
        assert!(matches!(
            branches.immutable_data_append(id, segment, Some(key(2)), row(2)),
            Err(BranchError::ImmutableData(DataError::Store(_)))
        ));
    }
    assert!(matches!(
        branches.immutable_data_truncate(id, "receipts", 0),
        Err(BranchError::ImmutableData(DataError::Store(_)))
    ));
    assert_eq!(staging(&branches, id), before);
    fail.store(false, Ordering::SeqCst);
    assert_eq!(append(&branches, id, 2), 1);
    assert_eq!(
        branches
            .immutable_data_append(id, "receipts", Some(key(2)), row(2))
            .unwrap(),
        0
    );
}
