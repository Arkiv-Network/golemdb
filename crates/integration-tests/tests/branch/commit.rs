use golemdb_branch::{BranchError, Branches};
use golemdb_cells::{CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, tables};
use golemdb_index::{Index, IndexTerm};
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher};
use golemdb_storage::{
    MemoryStore, ReadTransaction, StorageError, Store, Table, WriteTransaction, scan_prefix,
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const SUPERBLOCK: Table = Table("Superblock");
const TABLES: [Table; 8] = [
    tables::CELL,
    tables::CELL_TRIE,
    golemdb_index::tables::INDEX,
    golemdb_index::tables::INDEX_TRIE,
    golemdb_index::tables::BITMAP_TRIE,
    golemdb_index::tables::BITMAP_CONTAINER,
    SUPERBLOCK,
    Table("Roots"),
];
fn key(name: &[u8]) -> CellKey {
    CellKey::new(64, CellNameRef::raw(name))
}
fn value(text: &str) -> CellValue {
    CellValueRef::new(CellType::Str, text.as_bytes(), true)
        .unwrap()
        .into()
}
fn genesis(db: &impl Store, hash: &impl HashProvider) {
    let empty = hash.hash(&[]);
    let mut tx = db.begin_write().unwrap();
    tx.put(
        SUPERBLOCK,
        b"head",
        &[0u64.to_be_bytes().as_slice(), &empty, &empty].concat(),
    )
    .unwrap();
    tx.commit().unwrap();
}
fn snapshot(db: &impl Store) -> Vec<Vec<golemdb_storage::Entry>> {
    let tx = db.begin_read().unwrap();
    TABLES
        .iter()
        .map(|table| {
            scan_prefix(&tx, *table, vec![])
                .unwrap()
                .collect::<golemdb_storage::Result<Vec<_>>>()
                .unwrap()
        })
        .collect()
}
fn stage<S: Store, H: HashProvider>(
    branches: &Branches<S, H>,
    text: &str,
) -> golemdb_branch::BranchId {
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(b"name"), value(text));
            Ok::<_, ()>(())
        })
        .unwrap();
    branch
}

#[test]
fn stale_commit_conflicts_unless_another_operation_already_invalidated_it() {
    let db = MemoryStore::new();
    genesis(&db, &Keccak256Hasher);
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    let winner = stage(&branches, "winner");
    let stale = stage(&branches, "stale");
    let invalidated = stage(&branches, "invalidated");
    branches.commit(winner).unwrap();
    // Conflict applies even if the caller did not explicitly seal first.
    assert!(matches!(branches.commit(stale), Err(BranchError::Conflict)));
    assert!(matches!(
        branches.commit(stale),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.branch_info(invalidated),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.commit(invalidated),
        Err(BranchError::HandleInvalid)
    ));
}
fn lifecycle(db: impl Store + Clone, hash: impl HashProvider + Copy) {
    genesis(&db, &hash);
    let branches = Branches::new(db.clone(), hash).unwrap();
    let old_reader = db.begin_read().unwrap();
    let old_head = old_reader.get(SUPERBLOCK, b"head").unwrap();
    let winner = stage(&branches, "Alice");
    let stale = stage(&branches, "Bob");
    branches.seal(stale).unwrap();
    let sealed = branches.seal(winner).unwrap();
    assert_eq!(
        branches.commit(winner).unwrap(),
        golemdb_branch::CommitId::new(1)
    );
    assert_eq!(branches.head().unwrap(), golemdb_branch::CommitId::new(1));
    assert!(matches!(
        branches.commit(winner),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.branch_info(winner),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(branches.commit(stale), Err(BranchError::Conflict)));
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
            .reopen(
                &latest,
                sealed.state_root,
                sealed.cells.root.branch_commit()
            )
            .unwrap(),
        sealed.cells.root
    );
    assert_eq!(
        Index::new(&hash)
            .reopen(
                &latest,
                sealed.index_root,
                sealed.index.root.branch_commit()
            )
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
    assert_eq!(
        branches.commit(branch).unwrap(),
        golemdb_branch::CommitId::new(2)
    );
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
    lifecycle(MemoryStore::new(), Keccak256Hasher);
}
#[test]
fn blake3_commit() {
    lifecycle(MemoryStore::new(), Blake3Hasher);
}
#[test]
fn mdbx_commit_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        lifecycle(
            golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap(),
            Keccak256Hasher,
        );
    }
    let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    assert_eq!(branches.head().unwrap(), golemdb_branch::CommitId::new(2));
    let branch = branches.begin().unwrap();
    // Reopens roots persisted by the previous manager, and extends the root chain.
    assert_eq!(
        branches.commit(branch).unwrap(),
        golemdb_branch::CommitId::new(3)
    );
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Open,
    Write(usize),
    HeadRead,
    HeadWrite,
    Commit,
    Panic(usize),
}
#[derive(Clone)]
struct Controlled<S> {
    inner: S,
    fault: Arc<Mutex<Fault>>,
    barrier: Option<Arc<Barrier>>,
    mutations: Arc<AtomicUsize>,
}
impl<S> Controlled<S> {
    fn new(inner: S) -> Self {
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
impl<S: Store> Store for Controlled<S> {
    type Read<'a>
        = S::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = ControlledWrite<S::Write<'a>>
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
        if matches!(self.fault, Fault::HeadRead) && table == SUPERBLOCK && key == b"head" {
            return Err(injected());
        }
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
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
fn failure_recovery(db: impl Store + Clone + 'static) {
    genesis(&db, &Keccak256Hasher);
    let init = Branches::new(db.clone(), Keccak256Hasher).unwrap();
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
        assert_eq!(branches.head().unwrap(), golemdb_branch::CommitId::new(1));
        assert!(branches.branch_info(branch).unwrap().sealed);
        let first = branches.seal(branch).unwrap();
        assert!(Arc::ptr_eq(&first, &branches.seal(branch).unwrap()));
    }
    *controlled.fault.lock().unwrap() = Fault::None;
    assert_eq!(
        branches.commit(branch).unwrap(),
        golemdb_branch::CommitId::new(2)
    );
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.get(tables::CELL, &key(b"name").encode()).unwrap(), None);
    assert_eq!(
        tx.get(tables::CELL, &key(b"other").encode()).unwrap(),
        Some(value("replacement").into_bytes())
    );
}
#[test]
fn memory_failures_abort_all_rows_and_allow_retry() {
    failure_recovery(MemoryStore::new());
}
#[test]
fn mdbx_failures_abort_all_rows_and_allow_retry() {
    let dir = tempfile::tempdir().unwrap();
    failure_recovery(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
}

fn race(db: impl Store + Clone + Send + Sync + 'static) {
    genesis(&db, &Keccak256Hasher);
    let mut controlled = Controlled::new(db.clone());
    controlled.barrier = Some(Arc::new(Barrier::new(2)));
    let a = Branches::new(controlled.clone(), Keccak256Hasher).unwrap();
    let b = Branches::new(controlled.clone(), Keccak256Hasher).unwrap();
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
        (Ok(commit), Err(BranchError::Conflict)) if commit.get() == 1 => &seal_a,
        (Err(BranchError::Conflict), Ok(commit)) if commit.get() == 1 => &seal_b,
        _ => panic!("unexpected race results: {result_a:?}, {result_b:?}"),
    };
    assert_eq!(a.head().unwrap(), golemdb_branch::CommitId::new(1));
    // The baseline contains only head; every winning replay mutation produces
    // one final row (including the head replacement). The loser writes nothing.
    assert_eq!(
        controlled.mutations.load(Ordering::Relaxed),
        snapshot(&db).iter().map(Vec::len).sum::<usize>()
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
    race(MemoryStore::new());
}
#[test]
fn simultaneous_commits_recheck_head_inside_mdbx_writer() {
    let dir = tempfile::tempdir().unwrap();
    race(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn implicit_and_explicit_seal_publish_identical_state() {
    let mut results = Vec::new();
    for explicit in [false, true] {
        let db = MemoryStore::new();
        genesis(&db, &Keccak256Hasher);
        let branches = Branches::new(db.clone(), Keccak256Hasher).unwrap();
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
        assert_eq!(
            branches.commit(branch).unwrap(),
            golemdb_branch::CommitId::new(1)
        );
        results.push(snapshot(&db));
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn mdbx_key_limit_during_implicit_seal_leaves_branch_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
    genesis(&db, &Keccak256Hasher);
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
