use crate::{BranchError, Branches};
use golemdb_cells::{
    CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, tables,
};
use golemdb_index::{Index, IndexTerm, PostingChange};
use golemdb_merkle::{Blake3Hasher, Hash, HashProvider, Keccak256Hasher, RootRef};
use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction, scan_prefix};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const SUPERBLOCK: Table = Table("Superblock");
const TABLES: [Table; 7] = [
    tables::CELL,
    tables::CELL_TRIE,
    golemdb_index::tables::INDEX,
    golemdb_index::tables::INDEX_TRIE,
    golemdb_index::tables::BITMAP_TRIE,
    golemdb_index::tables::BITMAP_CONTAINER,
    SUPERBLOCK,
];
fn key(id: u64, name: &[u8]) -> CellKey {
    CellKey::new(id, CellNameRef::raw(name))
}
fn value(text: &str, indexed: bool) -> CellValue {
    CellValueRef::new(CellType::Str, text.as_bytes(), indexed)
        .unwrap()
        .into()
}
fn head(tx: &mut impl WriteTransaction, id: u64, state: Hash, index: Hash) {
    tx.put(
        SUPERBLOCK,
        b"head",
        &[id.to_be_bytes().as_slice(), &state, &index].concat(),
    )
    .unwrap();
}
fn seed(db: &impl Store, hash: &impl HashProvider, id: u64, rows: &[(CellKey, CellValue)]) {
    let mut tx = db.begin_write().unwrap();
    let cells = Cells::new(hash)
        .apply(
            &mut tx,
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
        .apply(&mut tx, RootRef::Empty, postings)
        .unwrap();
    head(&mut tx, id, cells.root.hash(hash), index.root.hash(hash));
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
// A guard fixture: these operations must not even try to open a writer.
struct ReadOnly<S>(S);
impl<S> ReadOnly<S> {
    fn new(inner: S) -> Self {
        Self(inner)
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
        self.0.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        panic!("operation opened a database writer");
    }
}

struct PanicHasher(Arc<AtomicBool>);
impl HashProvider for PanicHasher {
    fn hash_parts(&self, parts: &[&[u8]]) -> Hash {
        assert!(!self.0.load(Ordering::Relaxed), "injected hash panic");
        Keccak256Hasher.hash_parts(parts)
    }
}

#[test]
fn invalid_attribute_failure_keeps_overlay_and_frames_retryable() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let before = snapshot(&db);
    let branches = Branches::new(ReadOnly::new(db.clone()), Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"ok"), value("keep", true));
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.checkpoint(branch).unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"\xff"), value("invalid attribute", true));
            Ok::<_, ()>(())
        })
        .unwrap();
    let info = branches.branch_info(branch).unwrap();
    assert!(matches!(branches.seal(branch), Err(BranchError::Term(_))));
    assert_eq!(branches.branch_info(branch).unwrap(), info);
    assert_eq!(snapshot(&db), before);
    branches.rollback(branch).unwrap();
    assert_eq!(
        branches
            .read(branch, |cells| cells.get(&key(64, b"ok")))
            .unwrap(),
        Some(value("keep", true))
    );
    assert!(branches.seal(branch).is_ok());
}

#[test]
fn panicking_seal_does_not_freeze_or_poison_the_branch() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let fail = Arc::new(AtomicBool::new(true));
    let branches = Branches::new(ReadOnly::new(db), PanicHasher(fail.clone()), None).unwrap();
    let branch = branches.begin().unwrap();
    assert!(std::panic::catch_unwind(AssertUnwindSafe(|| branches.seal(branch))).is_err());
    assert!(!branches.branch_info(branch).unwrap().sealed);
    fail.store(false, Ordering::Relaxed);
    assert!(branches.seal(branch).is_ok());
}

#[test]
fn stale_sealed_branches_and_commit_number_overflow_are_rejected() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let branches = Branches::new(ReadOnly::new(db.clone()), Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    let sealed = branches.seal(branch).unwrap();
    let publisher = Branches::new(db.clone(), Keccak256Hasher, None).unwrap();
    publisher.commit(publisher.begin().unwrap()).unwrap();
    assert!(matches!(
        branches.seal(branch),
        Err(BranchError::HandleInvalid)
    ));
    // Reopen over an actual branch root and append the next lag-one cell.
    let next = branches.begin().unwrap();
    let second = branches.seal(next).unwrap();
    assert_eq!(
        second.cells.changed_cells[0].key,
        key(2, &1u64.to_be_bytes())
    );
    let mut tx = db.begin_write().unwrap();
    head(&mut tx, u64::MAX, sealed.state_root, sealed.index_root);
    tx.commit().unwrap();
    let last = branches.begin().unwrap();
    assert!(matches!(
        branches.seal(last),
        Err(BranchError::CommitNumberExhausted)
    ));
    assert!(!branches.branch_info(last).unwrap().sealed);
}

#[test]
fn wrong_roots_and_duplicate_lag_one_cell_fail_without_freezing() {
    let db = MemoryStore::new();
    seed(&db, &Blake3Hasher, 0, &[]);
    let branches = Branches::new(ReadOnly::new(db), Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    assert!(matches!(branches.seal(branch), Err(BranchError::Cells(_))));
    assert!(!branches.branch_info(branch).unwrap().sealed);

    let db = MemoryStore::new();
    seed(
        &db,
        &Keccak256Hasher,
        0,
        &[(key(2, &0u64.to_be_bytes()), value("already exists", false))],
    );
    let branches = Branches::new(ReadOnly::new(db), Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    assert!(matches!(
        branches.seal(branch),
        Err(BranchError::RootsCellExists)
    ));
    assert!(!branches.branch_info(branch).unwrap().sealed);
}

fn stage<S: Store, H: HashProvider>(branches: &Branches<S, H>, text: &str) -> crate::BranchId {
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"name"), value(text, true));
            Ok::<_, ()>(())
        })
        .unwrap();
    branch
}

struct SwitchHash(Arc<AtomicBool>);
impl HashProvider for SwitchHash {
    fn hash_parts(&self, parts: &[&[u8]]) -> Hash {
        assert!(
            !self.0.load(Ordering::Relaxed),
            "commit recalculated a sealed hash"
        );
        Keccak256Hasher.hash_parts(parts)
    }
}

#[test]
fn explicit_seal_is_not_recomputed_by_commit() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let fail = Arc::new(AtomicBool::new(false));
    let branches = Branches::new(db, SwitchHash(fail.clone()), None).unwrap();
    let branch = stage(&branches, "Alice");
    branches.seal(branch).unwrap();
    fail.store(true, Ordering::Relaxed);
    assert_eq!(branches.commit(branch).unwrap(), crate::CommitId::new(1));
}

#[test]
fn implicit_seal_failure_keeps_branch_open_and_never_opens_writer() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let controlled = ReadOnly::new(db);
    let branches = Branches::new(controlled, Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, b"\xff"), value("invalid attribute name", true));
            Ok::<_, ()>(())
        })
        .unwrap();
    let info = branches.branch_info(branch).unwrap();
    assert!(matches!(branches.commit(branch), Err(BranchError::Term(_))));
    assert_eq!(branches.branch_info(branch).unwrap(), info);
    branches.rollback(branch).unwrap();
    branches.discard(branch).unwrap();
}

/// Branch rows of `table`, whose paths have `N` bytes, reachable from `roots`;
/// leaf and empty roots have no row.
fn reachable_rows<const N: usize>(
    tx: &impl golemdb_storage::ReadTransaction,
    table: Table,
    roots: &[Hash],
) -> std::collections::BTreeSet<Vec<u8>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = roots.to_vec();
    while let Some(hash) = stack.pop() {
        let Some(bytes) = tx.get(table, &hash).unwrap() else {
            continue;
        };
        if !seen.insert(hash.to_vec()) {
            continue;
        }
        let node = golemdb_merkle::BranchNodeCompact::<N>::decode(&bytes).unwrap();
        for slot in 0..16 {
            if let Some(RootRef::Branch(child)) = node.child(slot) {
                stack.push(child);
            }
        }
    }
    seen
}

/// The bitmap root of every term in the committed index.
fn bitmap_roots(db: &impl Store) -> Vec<Hash> {
    let tx = db.begin_read().unwrap();
    scan_prefix(&tx, golemdb_index::tables::INDEX, vec![])
        .unwrap()
        .map(|row| row.unwrap().1.try_into().unwrap())
        .collect()
}

#[test]
fn commits_store_only_trie_rows_reachable_from_committed_roots() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let branches = Branches::new(db, Keccak256Hasher, None).unwrap();
    let genesis = crate::head::read_head_state(&branches.store().begin_read().unwrap()).unwrap();
    let (mut states, mut indexes) = (vec![genesis.state_root], vec![genesis.index_root]);
    let mut bitmaps = bitmap_roots(branches.store());
    // Commit 1 creates 40 records; commit 2 rewrites most of them and removes some cells.
    for round in 0..2u64 {
        let branch = branches.begin().unwrap();
        branches
            .write(branch, |cells| {
                for i in 64..104u64 {
                    // Spread the records over three bitmap containers, so one
                    // tag term changes several containers in one commit.
                    let id = golemdb_index::path::join(i % 3, i as u16).unwrap();
                    if round == 1 && i % 3 == 0 {
                        cells.delete(key(id, b"tag"));
                        continue;
                    }
                    cells.put(
                        key(id, b"tag"),
                        value(&format!("t{}", (id + round) % 7), true),
                    );
                    cells.put(key(id, b"name"), value(&format!("n{id}-{round}"), true));
                    cells.put(key(id, b"note"), value("plain", false));
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        let sealed = branches.seal(branch).unwrap();
        states.push(sealed.state_root);
        indexes.push(sealed.index_root);
        branches.commit(branch).unwrap();
        bitmaps.extend(bitmap_roots(branches.store()));
    }
    let tx = branches.store().begin_read().unwrap();
    let rows = |table| {
        scan_prefix(&tx, table, vec![])
            .unwrap()
            .map(|row| row.unwrap().0)
            .collect::<std::collections::BTreeSet<_>>()
    };
    // No intermediate versions: every stored trie row belongs to a committed root.
    assert_eq!(
        rows(tables::CELL_TRIE),
        reachable_rows::<{ golemdb_cells::CELL_TRIE_PATH_BYTES }>(&tx, tables::CELL_TRIE, &states)
    );
    assert_eq!(
        rows(golemdb_index::tables::INDEX_TRIE),
        reachable_rows::<{ golemdb_index::INDEX_TRIE_PATH_BYTES }>(
            &tx,
            golemdb_index::tables::INDEX_TRIE,
            &indexes
        )
    );
    assert_eq!(
        rows(golemdb_index::tables::BITMAP_TRIE),
        reachable_rows::<{ golemdb_index::BITMAP_TRIE_PATH_BYTES }>(
            &tx,
            golemdb_index::tables::BITMAP_TRIE,
            &bitmaps
        )
    );
    // Nothing needed was dropped: the head trie reaches every current cell, and
    // every current term's posting list loads.
    let head = crate::head::read_head_state(&tx).unwrap();
    let hasher = Keccak256Hasher;
    let root = Cells::new(&hasher).reopen(&tx, head.state_root).unwrap();
    let trie = golemdb_merkle::Trie::<_, { golemdb_cells::CELL_TRIE_PATH_BYTES }>::new(
        tables::CELL_TRIE,
        golemdb_cells::CELL_BRANCH_DOMAIN,
        &hasher,
    );
    let leaves = trie
        .walk(&tx, root)
        .collect::<golemdb_merkle::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(leaves.len(), rows(tables::CELL).len());
    let index = Index::new(&hasher);
    for term in rows(golemdb_index::tables::INDEX) {
        let term = IndexTerm::decode(&term).unwrap();
        assert!(
            !index
                .bitmap(&tx, &term)
                .unwrap()
                .unwrap()
                .treemap()
                .is_empty()
        );
    }
}
