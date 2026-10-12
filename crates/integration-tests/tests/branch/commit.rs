use golemdb_branch::{
    BranchError, Branches,
    tables::{ENGINE, SUPERBLOCK},
};
use golemdb_cells::{CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, tables};
use golemdb_index::{Index, IndexTerm};
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher};
use golemdb_storage::{
    Database, MemoryDatabase, ReadTransaction, StorageError, Table, WriteTransaction, scan_prefix,
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

fn key(name: &[u8]) -> CellKey {
    CellKey::new(64, CellNameRef::raw(name))
}
fn value(text: &str) -> CellValue {
    CellValueRef::new(CellType::Str, text.as_bytes(), true)
        .unwrap()
        .into()
}
// Genesis for tests that then open the database through a wrapper.
fn genesis(db: &(impl Database + Clone)) {
    Branches::create(db.clone(), Keccak256Hasher, []).unwrap();
}

fn snapshot(db: &impl Database) -> Vec<Vec<golemdb_storage::Entry>> {
    let tx = db.begin_read().unwrap();
    ENGINE
        .iter()
        .map(|table| {
            scan_prefix(&tx, *table, vec![])
                .unwrap()
                .collect::<golemdb_storage::Result<Vec<_>>>()
                .unwrap()
        })
        .collect()
}
fn stage<D: Database, H: HashProvider>(branches: &Branches<D, H>, text: &str) -> u64 {
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(b"name"), value(text));
            Ok::<_, ()>(())
        })
        .unwrap();
    branch
}
fn lifecycle(db: impl Database + Clone, hash: impl HashProvider + Copy) {
    let branches = Branches::create(db.clone(), hash, []).unwrap();
    let old_reader = db.begin_read().unwrap();
    let old_head = old_reader.get(SUPERBLOCK, b"head").unwrap();
    let winner = stage(&branches, "Alice");
    let stale = stage(&branches, "Bob");
    branches.seal(stale).unwrap();
    let sealed = branches.seal(winner).unwrap();
    assert_eq!(branches.commit(winner).unwrap(), 1);
    assert_eq!(branches.head().unwrap(), 1);
    assert!(matches!(
        branches.commit(winner),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.branch_info(winner),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.commit(stale),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.branch_info(stale),
        Err(BranchError::HandleInvalid)
    ));
    assert_eq!(
        old_reader
            .get(tables::CELL, &key(b"name").encode())
            .unwrap(),
        None
    );
    assert_eq!(old_reader.get(SUPERBLOCK, b"head").unwrap(), old_head);
    let latest = db.begin_read().unwrap();
    assert_eq!(
        latest.get(SUPERBLOCK, b"head").unwrap().unwrap(),
        [
            1u64.to_be_bytes().as_slice(),
            &sealed.state_root,
            &sealed.index_root
        ]
        .concat()
    );
    assert_eq!(
        Cells::new(&hash)
            .reopen(&latest, sealed.state_root)
            .unwrap(),
        sealed.cells.root
    );
    assert_eq!(
        Index::new(&hash)
            .reopen(&latest, sealed.index_root)
            .unwrap(),
        sealed.index.root
    );
    let term = IndexTerm::new("name", CellType::Str, b"Alice").unwrap();
    assert_eq!(
        Index::new(&hash)
            .bitmap(&latest, &term)
            .unwrap()
            .unwrap()
            .treemap()
            .iter()
            .collect::<Vec<_>>(),
        vec![64]
    );
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.delete(key(b"name"));
            Ok::<_, ()>(())
        })
        .unwrap();
    // No explicit seal or final checkpoint is needed.
    assert_eq!(branches.commit(branch).unwrap(), 2);
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.get(tables::CELL, &key(b"name").encode()).unwrap(), None);
    assert_eq!(Index::new(&hash).bitmap(&tx, &term).unwrap(), None);
    let root_key = CellKey::new(2, CellNameRef::raw(&1u64.to_be_bytes()));
    let root_value =
        CellValue::parse(tx.get(tables::CELL, &root_key.encode()).unwrap().unwrap()).unwrap();
    assert_eq!(
        root_value.value(),
        [sealed.state_root, sealed.index_root].concat()
    );
    assert_eq!(root_value.cell_type(), CellType::Bytes);
    for table in [
        Table("CellHistory"),
        Table("IndexHistory"),
        Table("CellChangeSet"),
        Table("IndexChangeSet"),
    ] {
        assert_eq!(scan_prefix(&tx, table, vec![]).unwrap().count(), 0);
    }
}
#[test]
fn memory_commit_publishes_cells_index_roots_and_head_atomically() {
    lifecycle(MemoryDatabase::new(), Keccak256Hasher);
}
#[test]
fn blake3_commit() {
    lifecycle(MemoryDatabase::new(), Blake3Hasher);
}
#[test]
fn mdbx_commit_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        lifecycle(
            golemdb_storage_mdbx::MdbxDatabase::open(dir.path()).unwrap(),
            Keccak256Hasher,
        );
    }
    let db = golemdb_storage_mdbx::MdbxDatabase::open(dir.path()).unwrap();
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    assert_eq!(branches.head().unwrap(), 2);
    let branch = branches.begin().unwrap();
    // Reopens roots persisted by the previous manager, and extends the root chain.
    assert_eq!(branches.commit(branch).unwrap(), 3);
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Open,
    Write(usize),
    HeadRead,
    GraphRead,
    HeadWrite,
    NodeDelete,
    Commit,
    Panic(usize),
}
#[derive(Clone)]
struct Controlled<D> {
    inner: D,
    fault: Arc<Mutex<Fault>>,
    barrier: Option<Arc<Barrier>>,
    mutations: Arc<AtomicUsize>,
}
impl<D> Controlled<D> {
    fn new(inner: D) -> Self {
        Self {
            inner,
            fault: Arc::new(Mutex::new(Fault::None)),
            barrier: None,
            mutations: Arc::new(AtomicUsize::new(0)),
        }
    }
}
fn injected() -> StorageError {
    StorageError::Backend("injected commit failure".into())
}
struct ControlledWrite<W> {
    inner: W,
    fault: Fault,
    step: usize,
    mutations: Arc<AtomicUsize>,
}
impl<D: Database> Database for Controlled<D> {
    type Read<'a>
        = D::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = ControlledWrite<D::Write<'a>>
    where
        Self: 'a;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        self.inner.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        let fault = *self.fault.lock().unwrap();
        if matches!(fault, Fault::Open) {
            return Err(injected());
        }
        // Each branch has already read the old head when it arrives here.
        if let Some(barrier) = &self.barrier {
            barrier.wait();
        }
        Ok(ControlledWrite {
            inner: self.inner.begin_write()?,
            fault,
            step: 0,
            mutations: self.mutations.clone(),
        })
    }
}
impl<W: ReadTransaction> ReadTransaction for ControlledWrite<W> {
    type Cursor<'a>
        = W::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if matches!(self.fault, Fault::GraphRead) && table != SUPERBLOCK {
            return Err(injected());
        }
        if matches!(self.fault, Fault::HeadRead) && table == SUPERBLOCK && key == b"head" {
            return Err(injected());
        }
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        if matches!(self.fault, Fault::GraphRead) {
            return Err(injected());
        }
        self.inner.cursor(table, key)
    }
}
impl<W> ControlledWrite<W> {
    fn mutation(&mut self, table: Table) -> golemdb_storage::Result<()> {
        self.step += 1;
        if matches!(self.fault, Fault::Panic(n) if n == self.step) {
            panic!("injected write panic");
        }
        if matches!(self.fault, Fault::Write(n) if n == self.step)
            || (matches!(self.fault, Fault::HeadWrite) && table == SUPERBLOCK)
        {
            return Err(injected());
        }
        self.mutations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
impl<W: WriteTransaction> WriteTransaction for ControlledWrite<W> {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.mutation(table)?;
        self.inner.put(table, key, value)
    }
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.mutation(table)?;
        self.inner.insert(table, key, value)
    }
    fn delete(&mut self, table: Table, key: &[u8]) -> golemdb_storage::Result<bool> {
        // GC deletes the replaced cell trie root in the measured commit.
        if matches!(self.fault, Fault::NodeDelete) && table == tables::CELL_TRIE {
            return Err(injected());
        }
        self.mutation(table)?;
        self.inner.delete(table, key)
    }
    fn commit(self) -> golemdb_storage::Result<()> {
        if matches!(self.fault, Fault::Commit) {
            return Err(injected());
        }
        self.inner.commit()
    }
}
fn failure_recovery(db: impl Database + Clone + 'static) {
    let init = Branches::create(db.clone(), Keccak256Hasher, []).unwrap();
    init.commit(stage(&init, "original")).unwrap();
    let before = snapshot(&db);
    let controlled = Controlled::new(db.clone());
    let branches = Branches::new(controlled.clone(), Keccak256Hasher).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.delete(key(b"name"));
            cells.put(key(b"other"), value("replacement"));
            Ok::<_, ()>(())
        })
        .unwrap();
    // First failure occurs after implicit seal. Subsequent attempts reuse it.
    for fault in [
        Fault::Open,
        Fault::HeadRead,
        Fault::Write(1),
        Fault::Write(3),
        Fault::HeadWrite,
        Fault::NodeDelete,
        Fault::Commit,
        Fault::Panic(3),
    ] {
        *controlled.fault.lock().unwrap() = fault;
        if matches!(fault, Fault::Panic(_)) {
            assert!(
                std::panic::catch_unwind(AssertUnwindSafe(|| branches.commit(branch))).is_err()
            );
        } else {
            assert!(matches!(
                branches.commit(branch),
                Err(BranchError::Storage(_))
            ));
        }
        assert_eq!(snapshot(&db), before);
        assert_eq!(branches.head().unwrap(), 1);
        assert!(branches.branch_info(branch).unwrap().sealed);
        let first = branches.seal(branch).unwrap();
        assert!(Arc::ptr_eq(&first, &branches.seal(branch).unwrap()));
    }
    *controlled.fault.lock().unwrap() = Fault::None;
    assert_eq!(branches.commit(branch).unwrap(), 2);
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.get(tables::CELL, &key(b"name").encode()).unwrap(), None);
    assert_eq!(
        tx.get(tables::CELL, &key(b"other").encode()).unwrap(),
        Some(value("replacement").into_bytes())
    );
}
#[test]
fn memory_failures_abort_all_rows_and_allow_retry() {
    failure_recovery(MemoryDatabase::new());
}
#[test]
fn mdbx_failures_abort_all_rows_and_allow_retry() {
    let dir = tempfile::tempdir().unwrap();
    failure_recovery(golemdb_storage_mdbx::MdbxDatabase::open(dir.path()).unwrap());
}

fn publication_reads_only_header(db: impl Database + Clone) {
    let branches = Branches::create(db.clone(), Keccak256Hasher, []).unwrap();
    branches.commit(stage(&branches, "original")).unwrap();
    let controlled = Controlled::new(db);
    *controlled.fault.lock().unwrap() = Fault::GraphRead;
    let branches = Branches::new(controlled, Keccak256Hasher).unwrap();
    for explicit in [false, true] {
        let branch = stage(&branches, if explicit { "final" } else { "changed" });
        if explicit {
            branches.seal(branch).unwrap();
        }
        branches.commit(branch).unwrap();
    }
}

#[test]
fn memory_publication_only_reads_head_and_format() {
    publication_reads_only_header(MemoryDatabase::new());
}

#[test]
fn mdbx_publication_only_reads_head_and_format() {
    let dir = tempfile::tempdir().unwrap();
    publication_reads_only_header(golemdb_storage_mdbx::MdbxDatabase::open(dir.path()).unwrap());
}

fn race(db: impl Database + Clone + Send + Sync + 'static) {
    genesis(&db);
    let mut controlled = Controlled::new(db.clone());
    controlled.barrier = Some(Arc::new(Barrier::new(2)));
    let a = Branches::new(controlled.clone(), Keccak256Hasher).unwrap();
    let b = Branches::new(controlled.clone(), Keccak256Hasher).unwrap();
    let initial_rows = snapshot(&db).iter().map(Vec::len).sum::<usize>();
    let id_a = stage(&a, "Alice");
    let id_b = stage(&b, "Bob");
    let seal_a = a.seal(id_a).unwrap();
    let seal_b = b.seal(id_b).unwrap();
    let (result_a, result_b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| a.commit(id_a));
        let second = scope.spawn(|| b.commit(id_b));
        (first.join().unwrap(), second.join().unwrap())
    });
    let winner = match (&result_a, &result_b) {
        (Ok(1), Err(BranchError::HandleInvalid)) => &seal_a,
        (Err(BranchError::HandleInvalid), Ok(1)) => &seal_b,
        _ => panic!("unexpected race results: {result_a:?}, {result_b:?}"),
    };
    assert_eq!(a.head().unwrap(), 1);
    // Every winning replay mutation produces one final row, including the
    // head replacement. The pre-existing format marker is not rewritten.
    assert_eq!(
        controlled.mutations.load(Ordering::Relaxed),
        snapshot(&db).iter().map(Vec::len).sum::<usize>() - initial_rows + 1
    );
    let tx = db.begin_read().unwrap();
    assert_eq!(
        tx.get(SUPERBLOCK, b"head").unwrap().unwrap(),
        [
            1u64.to_be_bytes().as_slice(),
            &winner.state_root,
            &winner.index_root
        ]
        .concat()
    );
    // Exactly the winner's cell changes appear in storage.
    for change in &winner.cells.changed_cells {
        assert_eq!(
            tx.get(tables::CELL, &change.key.encode()).unwrap(),
            change.after.as_ref().map(|v| v.encoded_bytes().to_vec())
        );
    }
    assert!(matches!(
        a.branch_info(id_a),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        b.branch_info(id_b),
        Err(BranchError::HandleInvalid)
    ));
}
#[test]
fn simultaneous_commits_recheck_head_inside_memory_writer() {
    race(MemoryDatabase::new());
}
#[test]
fn simultaneous_commits_recheck_head_inside_mdbx_writer() {
    let dir = tempfile::tempdir().unwrap();
    race(golemdb_storage_mdbx::MdbxDatabase::open(dir.path()).unwrap());
}

#[test]
fn implicit_and_explicit_seal_publish_identical_state() {
    let mut results = Vec::new();
    for explicit in [false, true] {
        let db = MemoryDatabase::new();
        let branches = Branches::create(db.clone(), Keccak256Hasher, []).unwrap();
        let branch = stage(&branches, "final");
        branches.checkpoint(branch).unwrap();
        branches
            .write(branch, |cells| {
                cells.put(key(b"name"), value("rolled back"));
                Ok::<_, ()>(())
            })
            .unwrap();
        branches.rollback(branch).unwrap();
        if explicit {
            branches.seal(branch).unwrap();
        }
        assert_eq!(branches.commit(branch).unwrap(), 1);
        results.push(snapshot(&db));
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn mdbx_key_limit_during_implicit_seal_leaves_branch_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = golemdb_storage_mdbx::MdbxDatabase::open(dir.path()).unwrap();
    genesis(&db);
    let before = snapshot(&db);
    let branches = Branches::new(db.clone(), Keccak256Hasher).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            // Cell codec accepts raw names; deployment admission is the record
            // layer's responsibility. This deliberately exceeds MDBX's key ceiling.
            let field = CellValueRef::new(CellType::Str, b"field", false)
                .unwrap()
                .into();
            cells.put(key(&vec![b'a'; 10000]), field);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(matches!(
        branches.commit(branch),
        Err(BranchError::Cells(golemdb_cells::CellError::Storage(
            StorageError::KeyTooLarge { .. }
        )))
    ));
    assert_eq!(snapshot(&db), before);
    assert!(!branches.branch_info(branch).unwrap().sealed);
    branches.discard(branch).unwrap();
}
