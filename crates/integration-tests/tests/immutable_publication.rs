use golemdb_branch::{BranchError, Branches, CommitId, Head, read_head, write_head};
use golemdb_cells::{CellKey, CellNameRef, CellType, CellValue, Width, tables};
use golemdb_immutable_data::{metadata, *};
use golemdb_merkle::{HashProvider, Keccak256Hasher};
use golemdb_storage::{MemoryStore, ReadTransaction, StorageError, Store, Table, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

#[path = "immutable_publication/safety.rs"]
mod safety;

// A subprocess can briefly inherit other tests' flock descriptors before exec.
// Exclude file-owning tests while spawning the crash child in the threaded runner.
static FILE_TESTS: RwLock<()> = RwLock::new(());

fn config() -> ImmutableDataConfig {
    ImmutableDataConfig::new(
        2,
        ["events", "receipts"]
            .map(|name| ImmutableDataSegment {
                name: name.into(),
                compression: ImmutableDataCompression::Lz4,
                columns: vec![
                    ImmutableDataColumn::new("number").with_type(CellType::Uint(Width::W8)),
                    ImmutableDataColumn::new("data"),
                ],
            })
            .into(),
    )
}
fn row(n: u64) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![CellValue::from_u64(n), CellValue::from_bytes(b"body")])
}
fn key(n: u8) -> ImmutableDataKey {
    ImmutableDataKey([n; 32])
}
fn seed(store: &impl Store) {
    let mut tx = store.begin_write().unwrap();
    let root = Keccak256Hasher.hash(&[]);
    write_head(
        &mut tx,
        &Head {
            commit_id: CommitId::GENESIS,
            state_root: root,
            index_root: root,
        },
    )
    .unwrap();
    tx.commit().unwrap();
}
fn read(
    store: &impl Store,
    reader: &ImmutableDataReader,
    segment: &str,
    address: ImmutableDataAddress,
    options: &ImmutableDataReadOptions,
) -> golemdb_branch::Result<ImmutableDataRow> {
    let tx = store.begin_read()?;
    Ok(reader.get(&tx, read_head(&tx)?.get(), segment, address, options)?)
}
fn rows_of(
    store: &impl Store,
    reader: &ImmutableDataReader,
    segment: &str,
    commit: CommitId,
    options: &ImmutableDataReadOptions,
) -> golemdb_branch::Result<Vec<ImmutableDataRow>> {
    let tx = store.begin_read()?;
    Ok(reader.rows_of(&tx, read_head(&tx)?.get(), segment, commit.get(), options)?)
}
fn number(store: &impl Store, reader: &ImmutableDataReader, n: u64) -> u64 {
    read(
        store,
        reader,
        "events",
        ImmutableDataAddress::Ordinal(n),
        &ImmutableDataReadOptions::columns(["number"]),
    )
    .unwrap()
    .get("number")
    .unwrap()
}
fn commit_row<S: Store>(b: &Branches<S, Keccak256Hasher>, n: u8) -> CommitId {
    let id = b.begin().unwrap();
    b.immutable_data_append(id, "events", Some(key(n)), row(n.into()))
        .unwrap();
    b.commit(id).unwrap()
}

fn successful_publication(store: impl Store + Clone, path: Option<&Path>) {
    seed(&store);
    let storage = match path {
        Some(path) => ImmutableDataStorage::open(config(), [7; 32], path).unwrap(),
        None => ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap(),
    };
    let reader = storage.reader();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    let old = store.begin_read().unwrap();
    let winner = b.begin().unwrap();
    let loser = b.begin().unwrap();
    let cell = CellKey::new(64, CellNameRef::raw(b"value"));
    b.write(winner, |cells| {
        cells.put(cell.clone(), CellValue::from_u64(7));
        Ok::<_, ()>(())
    })
    .unwrap();
    assert_eq!(
        b.immutable_data_append(winner, "events", Some(key(1)), row(10))
            .unwrap(),
        0
    );
    assert_eq!(
        b.immutable_data_append(loser, "events", Some(key(1)), row(99))
            .unwrap(),
        0
    );
    let seal = b.seal(winner).unwrap();
    assert_eq!(
        b.immutable_data_append(winner, "events", None, row(11))
            .unwrap(),
        1
    );
    b.immutable_data_append(winner, "receipts", Some(key(1)), row(12))
        .unwrap();
    assert!(matches!(
        read(
            &store,
            &reader,
            "events",
            ImmutableDataAddress::Ordinal(0),
            &ImmutableDataReadOptions::all()
        ),
        Err(BranchError::ImmutableData(
            ImmutableDataStorageError::NotFound
        ))
    ));
    assert_eq!(b.commit(winner).unwrap(), seal.commit_id);
    assert!(matches!(b.commit(loser), Err(BranchError::Conflict)));
    assert_eq!(number(&store, &reader, 0), 10);
    assert_eq!(number(&store, &reader, 1), 11);
    let projected = read(
        &store,
        &reader,
        "events",
        ImmutableDataAddress::Key(key(1)),
        &ImmutableDataReadOptions::columns(["data", "number"]),
    )
    .unwrap();
    assert_eq!(projected.get::<Vec<u8>>("data").unwrap(), b"body");
    assert_eq!(
        rows_of(
            &store,
            &reader,
            "events",
            CommitId::new(1),
            &ImmutableDataReadOptions::all()
        )
        .unwrap(),
        vec![row(10), row(11)]
    );
    assert!(
        rows_of(
            &store,
            &reader,
            "events",
            CommitId::GENESIS,
            &ImmutableDataReadOptions::all()
        )
        .unwrap()
        .is_empty()
    );
    assert!(matches!(
        rows_of(
            &store,
            &reader,
            "events",
            CommitId::new(2),
            &ImmutableDataReadOptions::all()
        ),
        Err(BranchError::ImmutableData(
            ImmutableDataStorageError::CommitUnavailable { .. }
        ))
    ));
    let tx = store.begin_read().unwrap();
    assert_eq!(
        tx.get(tables::CELL, &cell.encode()).unwrap(),
        Some(CellValue::from_u64(7).encoded_bytes().to_vec())
    );
    assert_eq!(metadata::commit_range(&tx, 0, 1).unwrap(), Some(0..2));
    assert_eq!(metadata::commit_range(&tx, 1, 1).unwrap(), Some(0..1));
    assert_eq!(metadata::ordinal_for_key(&old, 0, key(1)).unwrap(), None);
    assert_eq!(read_head(&old).unwrap(), CommitId::GENESIS);
    let empty = b.begin().unwrap();
    b.commit(empty).unwrap();
    assert!(
        rows_of(
            &store,
            &reader,
            "events",
            CommitId::new(2),
            &ImmutableDataReadOptions::all()
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(commit_row(&b, 3), CommitId::new(3));
    assert_eq!(number(&store, &reader, 2), 3);
}

#[test]
fn memory_publication_includes_cells_keys_ranges_and_head() {
    successful_publication(MemoryStore::new(), None);
}
#[test]
fn mdbx_and_files_publish_and_reopen() {
    let _files = FILE_TESTS.read().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let kv = dir.path().join("kv");
    let path = dir.path().join("files");
    successful_publication(MdbxStore::open(&kv).unwrap(), Some(&path));
    let store = MdbxStore::open(&kv).unwrap();
    let storage = ImmutableDataStorage::open(config(), [7; 32], &path).unwrap();
    let reader = storage.reader();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    assert_eq!(b.head().unwrap(), CommitId::new(3));
    assert_eq!(number(&store, &reader, 0), 10);
    assert_eq!(number(&store, &reader, 2), 3);
    assert_eq!(commit_row(&b, 4), CommitId::new(4));
}

#[derive(Default)]
struct Faults {
    // 1 abort, 2 publish then report failure, 3 abort and make outcome unreadable,
    // 4 panic before publication, 5 panic after publication, 6/7 process exit.
    mode: AtomicU8,
    fail_writer: AtomicBool,
    key_inserts: AtomicUsize,
    after_key_inserts: AtomicUsize,
    on_keys_written: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pause: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    on_failure: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    before_write: Mutex<VecDeque<Box<dyn FnOnce() + Send>>>,
    published_damage: Mutex<Option<safety::PublishedDamage>>,
}
#[derive(Clone)]
struct FaultStore<S> {
    inner: S,
    faults: Arc<Faults>,
}
struct FaultWrite<T> {
    inner: T,
    faults: Arc<Faults>,
}
impl<S: Store> Store for FaultStore<S> {
    type Read<'a>
        = S::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = FaultWrite<S::Write<'a>>
    where
        Self: 'a;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        self.inner.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        if self.faults.fail_writer.swap(false, Ordering::SeqCst) {
            return Err(fault());
        }
        let action = self.faults.before_write.lock().unwrap().pop_front();
        if let Some(action) = action {
            action();
        }
        Ok(FaultWrite {
            inner: self.inner.begin_write()?,
            faults: self.faults.clone(),
        })
    }
}
fn fault() -> StorageError {
    StorageError::Backend("injected publication failure".into())
}
impl<T: ReadTransaction> ReadTransaction for FaultWrite<T> {
    type Cursor<'a>
        = T::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.inner.cursor(table, key)
    }
}
impl<T: WriteTransaction> WriteTransaction for FaultWrite<T> {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.inner.put(table, key, value)
    }
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        if table == metadata::KEYS
            && self
                .faults
                .key_inserts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .ok()
                == Some(1)
        {
            return Err(fault());
        }
        self.inner.insert(table, key, value)?;
        if table == metadata::KEYS
            && self
                .faults
                .after_key_inserts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .ok()
                == Some(1)
            && let Some(action) = self.faults.on_keys_written.lock().unwrap().take()
        {
            action();
        }
        Ok(())
    }
    fn delete(&mut self, table: Table, key: &[u8]) -> golemdb_storage::Result<bool> {
        self.inner.delete(table, key)
    }
    fn commit(mut self) -> golemdb_storage::Result<()> {
        let pause = self.faults.pause.lock().unwrap().take();
        if let Some((entered, release)) = pause {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        let mode = self.faults.mode.swap(0, Ordering::SeqCst);
        if mode != 0
            && let Some(action) = self.faults.on_failure.lock().unwrap().take()
        {
            action();
        }
        match mode {
            1 => {
                self.inner.abort();
                Err(fault())
            }
            2 => {
                if let Some(damage) = self.faults.published_damage.lock().unwrap().take() {
                    damage.apply(&mut self.inner);
                }
                self.inner.commit()?;
                Err(fault())
            }
            3 => {
                self.inner.abort();
                self.faults.fail_writer.store(true, Ordering::SeqCst);
                Err(fault())
            }
            4 => {
                self.inner.abort();
                panic!("before commit")
            }
            5 => {
                self.inner.commit()?;
                panic!("after commit")
            }
            6 => std::process::exit(86),
            7 => {
                self.inner.commit()?;
                std::process::exit(87)
            }
            _ => self.inner.commit(),
        }
    }
}
fn fault_store<S>(inner: S) -> FaultStore<S> {
    FaultStore {
        inner,
        faults: Arc::new(Faults::default()),
    }
}

fn failures(store: impl Store + Clone + 'static, path: &Path) {
    seed(&store);
    let store = fault_store(store);
    let storage = ImmutableDataStorage::open(config(), [7; 32], path).unwrap();
    let reader = storage.reader();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    commit_row(&b, 1);
    let id = b.begin().unwrap();
    b.immutable_data_append(id, "events", Some(key(2)), row(2))
        .unwrap();
    b.immutable_data_append(id, "receipts", Some(key(2)), row(3))
        .unwrap();
    let cell = CellKey::new(64, CellNameRef::raw(b"retry"));
    b.write(id, |cells| {
        cells.put(cell.clone(), CellValue::from_u64(9));
        Ok::<_, ()>(())
    })
    .unwrap();
    // Fail the reverse key insert after the physical append and forward insert.
    store.faults.key_inserts.store(2, Ordering::SeqCst);
    assert!(matches!(
        b.commit(id),
        Err(BranchError::ImmutableData(
            ImmutableDataStorageError::Store(StorageError::Backend(_))
        ))
    ));
    assert!(b.branch_info(id).unwrap().sealed);
    assert_eq!(b.head().unwrap().get(), 1);
    assert!(!path.join("segment-events-2-4.jar").exists());
    let tx = store.begin_read().unwrap();
    assert_eq!(metadata::ordinal_for_key(&tx, 0, key(2)).unwrap(), None);
    assert_eq!(metadata::commit_range(&tx, 0, 2).unwrap(), None);
    assert_eq!(tx.get(tables::CELL, &cell.encode()).unwrap(), None);
    drop(tx);
    // Failure after all file fsyncs must roll back both segment tails.
    store.faults.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        b.commit(id),
        Err(BranchError::Storage(StorageError::Backend(_)))
    ));
    assert!(!path.join("segment-events-2-4.jar").exists());
    assert!(!path.join("segment-receipts-2-4.jar").exists());
    assert_eq!(number(&store, &reader, 0), 1);
    // A reported commit error with a published result is success, not rollback.
    store.faults.mode.store(2, Ordering::SeqCst);
    assert_eq!(b.commit(id).unwrap().get(), 2);
    assert_eq!(number(&store, &reader, 1), 2);
    assert!(matches!(b.branch_info(id), Err(BranchError::HandleInvalid)));
}
#[test]
fn memory_transaction_failures_restore_files_and_resolve_published_errors() {
    let _files = FILE_TESTS.read().unwrap();
    let dir = tempfile::tempdir().unwrap();
    failures(MemoryStore::new(), dir.path());
}
#[test]
fn mdbx_transaction_failures_restore_files_and_resolve_published_errors() {
    let _files = FILE_TESTS.read().unwrap();
    let dir = tempfile::tempdir().unwrap();
    failures(
        MdbxStore::open(dir.path().join("kv")).unwrap(),
        &dir.path().join("files"),
    );
}

#[test]
fn failed_publication_in_active_file_preserves_prefix_and_can_retry() {
    let _files = FILE_TESTS.read().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = fault_store(MemoryStore::new());
    seed(&store);
    let storage = ImmutableDataStorage::open(config(), [7; 32], dir.path()).unwrap();
    let reader = storage.reader();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    commit_row(&b, 1);
    commit_row(&b, 2);
    let id = b.begin().unwrap();
    b.immutable_data_append(id, "events", Some(key(3)), row(3))
        .unwrap();
    store.faults.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        b.commit(id),
        Err(BranchError::Storage(StorageError::Backend(_)))
    ));
    assert_eq!(number(&store, &reader, 1), 2);
    assert!(matches!(
        read(
            &store,
            &reader,
            "events",
            ImmutableDataAddress::Ordinal(2),
            &ImmutableDataReadOptions::all()
        ),
        Err(BranchError::ImmutableData(
            ImmutableDataStorageError::NotFound
        ))
    ));
    assert_eq!(b.commit(id).unwrap().get(), 3);
    assert_eq!(number(&store, &reader, 2), 3);
}

#[test]
fn unresolved_commit_and_panics_block_publication_but_keep_committed_reads() {
    let _files = FILE_TESTS.read().unwrap();
    for mode in [3, 4, 5] {
        let dir = tempfile::tempdir().unwrap();
        let db = MemoryStore::new();
        seed(&db);
        let store = fault_store(db.clone());
        let storage = ImmutableDataStorage::open(config(), [7; 32], dir.path()).unwrap();
        let reader = storage.reader();
        let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
        commit_row(&b, 1);
        let id = b.begin().unwrap();
        b.immutable_data_append(id, "events", Some(key(2)), row(2))
            .unwrap();
        store.faults.mode.store(mode, Ordering::SeqCst);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.commit(id)));
        if mode == 3 {
            assert!(matches!(
                result.unwrap(),
                Err(BranchError::PublicationRecovery { .. })
            ));
        } else {
            assert!(result.is_err());
        }
        assert_eq!(number(&store, &reader, 0), 1);
        // A fresh branch is also prevented from publishing until recovery.
        assert!(matches!(
            b.commit(b.begin().unwrap()),
            Err(BranchError::RecoveryRequired)
        ));
        drop(b);
        drop(reader);
        let store = db;
        let storage = ImmutableDataStorage::open(config(), [7; 32], dir.path()).unwrap();
        let reader = storage.reader();
        let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
        assert_eq!(number(&store, &reader, 0), 1);
        if mode == 5 {
            assert_eq!(number(&store, &reader, 1), 2);
            assert_eq!(b.head().unwrap().get(), 2);
        } else {
            assert_eq!(b.head().unwrap().get(), 1);
            assert_eq!(commit_row(&b, 2).get(), 2);
        }
    }
}

#[test]
fn failed_rollback_blocks_publication_and_retains_files_for_recovery() {
    let _files = FILE_TESTS.read().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = MemoryStore::new();
    seed(&db);
    let store = fault_store(db.clone());
    let storage = ImmutableDataStorage::open(config(), [7; 32], dir.path()).unwrap();
    let reader = storage.reader();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    commit_row(&b, 1);
    let offsets = dir.path().join("segment-events-0-2.off");
    let saved = std::fs::read(&offsets).unwrap();
    let remove = offsets.clone();
    *store.faults.on_failure.lock().unwrap() =
        Some(Box::new(move || std::fs::remove_file(remove).unwrap()));
    let id = b.begin().unwrap();
    b.immutable_data_append(id, "events", Some(key(2)), row(2))
        .unwrap();
    store.faults.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        b.commit(id),
        Err(BranchError::PublicationRecovery { .. })
    ));
    assert!(matches!(b.commit(id), Err(BranchError::RecoveryRequired)));
    assert!(dir.path().join("segment-events-2-4.jar").exists());
    drop(b);
    drop(reader);
    std::fs::write(offsets, saved).unwrap();
    let store = db;
    let storage = ImmutableDataStorage::open(config(), [7; 32], dir.path()).unwrap();
    let reader = storage.reader();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    assert_eq!(b.head().unwrap().get(), 1);
    assert_eq!(number(&store, &reader, 0), 1);
    assert!(!dir.path().join("segment-events-2-4.jar").exists());
}

fn concurrent_visibility(
    db: impl Store + Clone + Send + Sync + 'static,
    directory: Option<&Path>,
    baseline: u8,
    abort: bool,
) {
    let store = fault_store(db);
    seed(&store);
    let storage = if let Some(directory) = directory {
        ImmutableDataStorage::open(config(), [7; 32], directory).unwrap()
    } else {
        ImmutableDataStorage::in_memory(config(), [7; 32]).unwrap()
    };
    let reader = storage.reader();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    for n in 1..=baseline {
        commit_row(&b, n);
    }
    let old = store.begin_read().unwrap();
    let old_commit = read_head(&old).unwrap().get();
    let id = b.begin().unwrap();
    b.immutable_data_append(id, "events", Some(key(baseline + 1)), row(7))
        .unwrap();
    let (entered, waiting) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    *store.faults.pause.lock().unwrap() = Some((entered, proceed));
    store.faults.mode.store(u8::from(abort), Ordering::SeqCst);
    let writer = {
        let b = b.clone();
        std::thread::spawn(move || b.commit(id))
    };
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    // No timing-only assertion: a reader must finish before the writer
    // is released from its paused consuming transaction commit.
    let (done, result) = mpsc::channel();
    let reading = {
        let store = store.clone();
        let reader = reader.clone();
        std::thread::spawn(move || {
            assert_eq!(number(&store, &reader, 0), 1);
            assert!(matches!(
                read(
                    &store,
                    &reader,
                    "events",
                    ImmutableDataAddress::Ordinal(u64::from(baseline)),
                    &ImmutableDataReadOptions::all()
                ),
                Err(BranchError::ImmutableData(
                    ImmutableDataStorageError::NotFound
                ))
            ));
            assert!(matches!(
                read(
                    &store,
                    &reader,
                    "events",
                    ImmutableDataAddress::Key(key(baseline + 1)),
                    &ImmutableDataReadOptions::all()
                ),
                Err(BranchError::ImmutableData(
                    ImmutableDataStorageError::NotFound
                ))
            ));
            done.send(()).unwrap();
        })
    };
    result.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(()).unwrap();
    let outcome = writer.join().unwrap();
    if abort {
        assert!(matches!(
            outcome,
            Err(BranchError::Storage(StorageError::Backend(_)))
        ));
    } else {
        assert_eq!(outcome.unwrap(), CommitId::new(u64::from(baseline) + 1));
    }
    reading.join().unwrap();
    assert_eq!(number(&store, &reader, 0), 1);
    assert!(matches!(
        reader.get(
            &old,
            old_commit,
            "events",
            ImmutableDataAddress::Ordinal(u64::from(baseline)),
            &ImmutableDataReadOptions::all()
        ),
        Err(ImmutableDataStorageError::NotFound)
    ));
    assert!(matches!(
        reader.get(
            &old,
            old_commit,
            "events",
            ImmutableDataAddress::Key(key(baseline + 1)),
            &ImmutableDataReadOptions::all()
        ),
        Err(ImmutableDataStorageError::NotFound)
    ));
    if !abort {
        assert_eq!(number(&store, &reader, baseline.into()), 7);
    }
}
#[test]
fn readers_complete_during_publication_and_keep_snapshot_visibility() {
    let _files = FILE_TESTS.read().unwrap();
    for (baseline, abort) in [(1, false), (1, true), (2, false), (2, true)] {
        concurrent_visibility(MemoryStore::new(), None, baseline, abort);
        let dir = tempfile::tempdir().unwrap();
        concurrent_visibility(
            MdbxStore::open(dir.path().join("kv")).unwrap(),
            Some(&dir.path().join("files")),
            baseline,
            abort,
        );
    }
}

#[test]
fn crash_publication_child() {
    let Ok(root) = std::env::var("GOLEM_IMMUTABLE_CRASH_PATH") else {
        return;
    };
    let mode: u8 = std::env::var("GOLEM_IMMUTABLE_CRASH_MODE")
        .unwrap()
        .parse()
        .unwrap();
    let root = Path::new(&root);
    let store = fault_store(MdbxStore::open(root.join("kv")).unwrap());
    let storage = ImmutableDataStorage::open(config(), [7; 32], root.join("files")).unwrap();
    let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
    let next = b.head().unwrap().get() as u8 + 1;
    let id = b.begin().unwrap();
    b.immutable_data_append(id, "events", Some(key(next)), row(next.into()))
        .unwrap();
    store.faults.mode.store(mode, Ordering::SeqCst);
    let _ = b.commit(id);
    panic!("crash failpoint did not exit");
}

#[test]
fn process_restart_recovers_aborted_tail_and_keeps_published_tail() {
    let _files = FILE_TESTS.write().unwrap();
    for (mode, baseline) in [(6, 1), (7, 1), (6, 2), (7, 2)] {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = MdbxStore::open(dir.path().join("kv")).unwrap();
            seed(&store);
            let storage =
                ImmutableDataStorage::open(config(), [7; 32], dir.path().join("files")).unwrap();
            let b = Branches::new(store, Keccak256Hasher, Some(storage)).unwrap();
            for n in 1..=baseline {
                commit_row(&b, n);
            }
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_publication_child", "--nocapture"])
            .env("GOLEM_IMMUTABLE_CRASH_PATH", dir.path())
            .env("GOLEM_IMMUTABLE_CRASH_MODE", mode.to_string())
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(80 + mode),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let store = MdbxStore::open(dir.path().join("kv")).unwrap();
        let storage =
            ImmutableDataStorage::open(config(), [7; 32], dir.path().join("files")).unwrap();
        let reader = storage.reader();
        let b = Branches::new(store.clone(), Keccak256Hasher, Some(storage)).unwrap();
        for n in 1..=baseline {
            assert_eq!(number(&store, &reader, u64::from(n - 1)), u64::from(n));
        }
        let next = u64::from(baseline + 1);
        if mode == 7 {
            assert_eq!(number(&store, &reader, baseline.into()), next);
            assert_eq!(b.head().unwrap().get(), next);
        } else {
            assert_eq!(b.head().unwrap().get(), u64::from(baseline));
            assert_eq!(commit_row(&b, baseline + 1).get(), next);
        }
    }
}

#[test]
fn constructor_rejects_missing_head_before_recovery() {
    let _files = FILE_TESTS.read().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut storage = ImmutableDataStorage::open(config(), [7; 32], dir.path()).unwrap();
    storage.append("events", 1, &row(1)).unwrap();
    storage.sync().unwrap();
    assert!(matches!(
        Branches::new(MemoryStore::new(), Keccak256Hasher, Some(storage)),
        Err(BranchError::MissingHead)
    ));
    // Failure releases ownership without interpreting missing head as genesis
    // and discarding the existing physical rows during recovery.
    let storage = ImmutableDataStorage::open(config(), [7; 32], dir.path()).unwrap();
    assert_eq!(storage.end_ordinal("events").unwrap(), 1);
}
