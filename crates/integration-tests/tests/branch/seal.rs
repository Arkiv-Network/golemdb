use golemdb_branch::{BranchError, Branches, OperationError, SealedCommit, read_head_state};
use golemdb_cells::{
    CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, tables,
};
use golemdb_index::{Index, IndexTerm, PostingChange};
use golemdb_merkle::{
    BRANCH_ROOT_BYTES, Blake3Hasher, HASH_BYTES, HashProvider, Keccak256Hasher, RootRef,
};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const SUPERBLOCK: Table = Table("Superblock");
const TABLES: [Table; 8] = [
    tables::CELL,
    tables::CELL_TRIE.table(),
    golemdb_index::tables::INDEX,
    golemdb_index::tables::INDEX_TRIE.table(),
    golemdb_index::tables::BITMAP_TRIE.table(),
    golemdb_index::tables::BITMAP_CONTAINER.table(),
    SUPERBLOCK,
    Table("Roots"),
];
fn key(id: u64, name: &[u8]) -> CellKey {
    CellKey::new(id, CellNameRef::raw(name))
}
fn value(text: &str, indexed: bool) -> CellValue {
    CellValueRef::new(CellType::Str, text.as_bytes(), indexed)
        .unwrap()
        .into()
}
/// Head row plus the `Roots` row that locates its tries.
fn head(
    tx: &mut impl WriteTransaction,
    hash: &impl HashProvider,
    id: u64,
    state: RootRef<32>,
    index: RootRef<32>,
) {
    let roots = golemdb_branch::Roots { state, index };
    golemdb_branch::write_head(tx, golemdb_branch::CommitId::new(id), &roots, hash).unwrap();
}
fn seed(db: &impl Store, hash: &impl HashProvider, id: u64, rows: &[(CellKey, CellValue)]) {
    let mut tx = db.begin_write().unwrap();
    let cells = Cells::new(hash)
        .apply(
            &mut tx,
            id,
            RootRef::Empty,
            rows.iter().map(|(key, value)| CellChange::Put {
                key: key.clone(),
                value: value.clone(),
            }),
        )
        .unwrap();
    let postings = rows
        .iter()
        .filter(|(_, value)| value.is_indexable())
        .map(|(key, value)| PostingChange::Add {
            term: IndexTerm::from_cell(
                std::str::from_utf8(key.name().as_bytes()).unwrap(),
                value.as_view(),
            )
            .unwrap()
            .unwrap(),
            record_id: key.record_id(),
        });
    let index = Index::new(hash)
        .apply(&mut tx, id, RootRef::Empty, postings)
        .unwrap();
    head(&mut tx, hash, id, cells.root, index.root);
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
// Seal must never acquire a backend writer, even temporarily.
#[derive(Clone)]
struct ReadOnly<S> {
    inner: S,
    allow_writes: Arc<AtomicBool>,
}
impl<S> ReadOnly<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            allow_writes: Arc::new(AtomicBool::new(false)),
        }
    }
    fn enable_commit(&self) {
        self.allow_writes.store(true, Ordering::Relaxed);
    }
}
impl<S: Store> Store for ReadOnly<S> {
    type Read<'a>
        = S::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = S::Write<'a>
    where
        Self: 'a;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        self.inner.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        assert!(
            self.allow_writes.load(Ordering::Relaxed),
            "seal opened a database writer"
        );
        self.inner.begin_write()
    }
}

fn compare_with_rebuild(db: impl Store + Clone + 'static, hash: impl HashProvider + Copy) {
    let initial = vec![
        (key(64, b"name"), value("old", true)),
        (key(65, b"name"), value("old", true)),
        (key(66, b"toggle"), value("same", true)),
        (key(67, b"toggle"), value("same", false)),
        (key(68, b"keep"), value("unchanged", true)),
        (key(69, b"remove"), value("gone", true)),
    ];
    seed(&db, &hash, 7, &initial);
    let before = snapshot(&db);
    let database = ReadOnly::new(db.clone());
    let branches = Branches::new(database.clone(), hash, None, None).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"name"), value("intermediate", true));
            cells.put(key(64, b"name"), value("new", true));
            cells.put(key(66, b"toggle"), value("same", false));
            cells.put(key(67, b"toggle"), value("same", true));
            cells.put(key(68, b"keep"), value("unchanged", true));
            cells.delete(key(69, b"remove"));
            cells.delete(key(70, b"absent"));
            cells.put(key(3, b"\xff\x00"), value("binary field", false));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    branches.checkpoint(branch).unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"name"), value("rolled-back", true));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    branches.rollback(branch).unwrap();
    let version = branches.branch_info(branch).unwrap().version;
    let sealed = branches.seal(branch).unwrap();
    assert_eq!(snapshot(&db), before);
    assert_eq!(branches.head().unwrap(), golemdb_branch::CommitId::new(7));
    assert_eq!(sealed.commit_id, golemdb_branch::CommitId::new(8));
    assert!(branches.branch_info(branch).unwrap().sealed);
    assert_eq!(branches.branch_info(branch).unwrap().version, version);
    assert!(Arc::ptr_eq(&sealed, &branches.seal(branch).unwrap()));
    assert_eq!(sealed.cells.changed_cells.len(), 6); // Five actual user changes + #roots.
    assert_eq!(sealed.index.changed_terms.len(), 4);
    branches
        .read(branch, |cell_reader| {
            assert_eq!(
                cell_reader.get(&key(64, b"name"))?,
                Some(value("new", true))
            );
            assert!(cell_reader.get(&key(69, b"remove"))?.is_none());
            assert!(
                cell_reader
                    .get(&key(golemdb_cells::system::ROOTS.id, &7u64.to_be_bytes()))?
                    .is_none()
            );
            let rows = cell_reader
                .scan_prefix(&64u64.to_be_bytes())?
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(rows.len(), 1);
            Ok::<_, BranchError>(())
        })
        .unwrap();
    assert!(matches!(
        branches.write(branch, |_| -> Result<(), ()> {
            panic!("sealed write admitted")
        }),
        Err(OperationError::Branch(BranchError::Sealed))
    ));
    assert!(matches!(
        branches.checkpoint(branch),
        Err(BranchError::Sealed)
    ));
    assert!(matches!(
        branches.rollback(branch),
        Err(BranchError::Sealed)
    ));
    // Check the final roots against a full rebuild, not the seal algorithm.
    let mut final_rows = initial.into_iter().collect::<BTreeMap<_, _>>();
    for change in &sealed.cells.changed_cells {
        assert_eq!(final_rows.get(&change.key), change.before.as_ref());
        match &change.after {
            Some(value) => {
                final_rows.insert(change.key.clone(), value.clone());
            }
            None => {
                final_rows.remove(&change.key);
            }
        }
    }
    let expected = MemoryStore::new();
    // Exercise discard while the origin is still head, then publish the result
    // through the public API instead of accessing private buffered rows.
    let discarded = branches.begin().unwrap();
    branches.seal(discarded).unwrap();
    branches.discard(discarded).unwrap();
    assert!(matches!(
        branches.seal(discarded),
        Err(BranchError::HandleInvalid)
    ));
    database.enable_commit();
    assert_eq!(branches.commit(branch).unwrap(), sealed.commit_id);
    verify_rebuild(&db, &expected, &sealed, final_rows, &hash);
    assert!(matches!(
        branches.seal(branch),
        Err(BranchError::HandleInvalid)
    ));
}

fn verify_rebuild(
    db: &impl Store,
    expected: &MemoryStore,
    sealed: &SealedCommit,
    rows: BTreeMap<CellKey, CellValue>,
    hash: &impl HashProvider,
) {
    seed(expected, hash, 7, &rows.into_iter().collect::<Vec<_>>());
    let tx = expected.begin_read().unwrap();
    let head = read_head_state(&tx).unwrap();
    assert_eq!(sealed.state_root, head.state_root);
    assert_eq!(sealed.index_root, head.index_root);
    let tx = db.begin_read().unwrap();
    Cells::new(hash)
        .validate_root(&tx, sealed.cells.root)
        .unwrap();
    Index::new(hash)
        .validate_root(&tx, sealed.index.root)
        .unwrap();
    // Index rows hold `hash ‖ commit [‖ singleton path]`. Only the commit that
    // stored each bitmap root differs from the rebuilt store, and it is never
    // later than the sealed commit.
    fn without_commit(
        tx: &impl ReadTransaction,
        table: Table,
        commit: u64,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        scan_prefix(tx, table, vec![])
            .unwrap()
            .map(|row| {
                let (key, mut value) = row.unwrap();
                if table == golemdb_index::tables::INDEX {
                    let field = HASH_BYTES..BRANCH_ROOT_BYTES;
                    let stored = u64::from_be_bytes(value[field.clone()].try_into().unwrap());
                    assert!(stored <= commit);
                    value.drain(field);
                }
                (key, value)
            })
            .collect()
    }
    let commit = sealed.commit_id.get();
    for table in [tables::CELL, golemdb_index::tables::INDEX] {
        let expected_tx = expected.begin_read().unwrap();
        assert_eq!(
            without_commit(&tx, table, commit),
            without_commit(&expected_tx, table, commit)
        );
    }
    let term = IndexTerm::new("name", CellType::Str, b"old").unwrap();
    assert_eq!(
        Index::new(hash)
            .bitmap(&tx, &term)
            .unwrap()
            .unwrap()
            .treemap()
            .iter()
            .collect::<Vec<_>>(),
        vec![65]
    );
}

#[test]
fn memory_seal_matches_full_rebuild_without_storage_writes() {
    compare_with_rebuild(MemoryStore::new(), Keccak256Hasher);
}

#[test]
fn mdbx_seal_matches_full_rebuild_without_storage_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
    compare_with_rebuild(db, Keccak256Hasher);
}

#[test]
fn empty_seal_adds_only_lag_one_roots_and_supports_blake3() {
    let db = MemoryStore::new();
    let hash = Blake3Hasher;
    seed(&db, &hash, 0, &[]);
    let before = snapshot(&db);
    let database = ReadOnly::new(db.clone());
    let branches = Branches::new(database.clone(), hash, None, None).unwrap();
    let branch = branches.begin().unwrap();
    let sealed = branches.seal(branch).unwrap();
    assert_eq!(sealed.commit_id, golemdb_branch::CommitId::new(1));
    assert_eq!(sealed.index_root, hash.hash(&[]));
    assert!(sealed.index.changed_terms.is_empty());
    assert_eq!(sealed.cells.changed_cells.len(), 1);
    let change = &sealed.cells.changed_cells[0];
    assert_eq!(change.key, key(2, &0u64.to_be_bytes()));
    assert_eq!(change.before, None);
    let value = change.after.as_ref().unwrap();
    assert!(!value.is_indexable());
    assert_eq!(value.cell_type(), CellType::Bytes);
    assert_eq!(value.value(), [hash.hash(&[]), hash.hash(&[])].concat());
    assert_eq!(branches.branch_info(branch).unwrap().version, 0);
    assert_eq!(snapshot(&db), before);
    database.enable_commit();
    branches.commit(branch).unwrap();
    Cells::new(&hash)
        .validate_root(&db.begin_read().unwrap(), sealed.cells.root)
        .unwrap();
}

#[derive(Clone)]
struct FaultReads {
    db: MemoryStore,
    fail: Arc<AtomicBool>,
}
struct FaultSnapshot<R> {
    origin: R,
    fail: Arc<AtomicBool>,
}
impl<R: golemdb_storage::ReadTransaction> golemdb_storage::ReadTransaction for FaultSnapshot<R> {
    type Cursor<'a>
        = R::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if table == golemdb_index::tables::BITMAP_CONTAINER.table()
            && self.fail.load(Ordering::Relaxed)
        {
            return Err(golemdb_storage::StorageError::Backend(
                "injected bitmap read failure".into(),
            ));
        }
        self.origin.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.origin.cursor(table, key)
    }
}
impl Store for FaultReads {
    type Read<'a> = FaultSnapshot<<MemoryStore as Store>::Read<'a>>;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        Ok(FaultSnapshot {
            origin: self.db.begin_read()?,
            fail: self.fail.clone(),
        })
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        panic!("seal opened a writer")
    }
}
#[test]
fn index_failure_after_cell_apply_discards_buffer_and_allows_retry() {
    let db = MemoryStore::new();
    seed(
        &db,
        &Keccak256Hasher,
        0,
        &[(key(64, b"name"), value("old", true))],
    );
    let before = snapshot(&db);
    let fail = Arc::new(AtomicBool::new(true));
    let branches = Branches::new(
        FaultReads {
            db: db.clone(),
            fail: fail.clone(),
        },
        Keccak256Hasher,
        None,
        None,
    )
    .unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"name"), value("new", true));
            Ok::<_, ()>(())
        })
        .unwrap();
    let info = branches.branch_info(branch).unwrap();
    assert!(matches!(branches.seal(branch), Err(BranchError::Index(_))));
    assert_eq!(branches.branch_info(branch).unwrap(), info);
    assert_eq!(snapshot(&db), before);
    fail.store(false, Ordering::Relaxed);
    let sealed = branches.seal(branch).unwrap();
    assert_eq!(sealed.cells.changed_cells.len(), 2);
    assert_eq!(sealed.index.changed_terms.len(), 2);
    assert_eq!(snapshot(&db), before);
}

#[test]
fn blake3_seal_matches_full_rebuild() {
    compare_with_rebuild(MemoryStore::new(), Blake3Hasher);
}
