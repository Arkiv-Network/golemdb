use crate::{BranchError, Branches};
use golemdb_cells::{
    CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, Cells, tables,
};
use golemdb_index::{Index, IndexTerm, PostingChange};
use golemdb_merkle::{Blake3Hasher, Hash, HashProvider, Keccak256Hasher, RootRef, TrieTable};
use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction, scan_prefix};
use std::{
    panic::AssertUnwindSafe,
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
    crate::ROOTS,
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
    let roots = crate::Roots { state, index };
    crate::write_head(tx, crate::CommitId::new(id), &roots, hash).unwrap();
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
    head(
        &mut tx,
        &Keccak256Hasher,
        u64::MAX,
        sealed.cells.root,
        sealed.index.root,
    );
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
    // The seeded roots were hashed with another algorithm, so `Roots[head]` disagrees.
    assert!(matches!(
        branches.seal(branch),
        Err(BranchError::InvalidRoots(0))
    ));
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

/// Row keys of `table`, whose paths have `N` bytes, reachable from `roots`,
/// each in its namespace.
fn reachable_rows<const N: usize, const NS: usize>(
    tx: &impl golemdb_storage::ReadTransaction,
    table: TrieTable<NS>,
    domain: u8,
    roots: &[([u8; NS], RootRef<N>)],
) -> std::collections::BTreeSet<Vec<u8>> {
    let node_key = |ns: [u8; NS], commit, route: &[u8]| {
        golemdb_merkle::Trie::<_, N, NS>::namespaced(table, ns, domain, &Keccak256Hasher)
            .node_key(commit, route)
    };
    let leaf_storage = if table.table() == golemdb_index::tables::BITMAP_TRIE.table() {
        golemdb_merkle::LeafStorage::Rows
    } else {
        golemdb_merkle::LeafStorage::InNode
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut stack: Vec<_> = roots
        .iter()
        .map(|(ns, root)| (*ns, *root, Vec::new()))
        .collect();
    while let Some((ns, root, route)) = stack.pop() {
        let RootRef::Branch(branch) = root else {
            continue;
        };
        let key = node_key(ns, branch.commit, &route);
        let bytes = tx
            .get(table.table(), &key)
            .unwrap()
            .expect("reachable row is stored");
        if !seen.insert(key) {
            continue;
        }
        let node = golemdb_merkle::BranchNodeCompact::<N>::decode(&bytes, leaf_storage).unwrap();
        let base = [&route[..], &node.unpack_prefix()].concat();
        for slot in 0..golemdb_merkle::BRANCH_SLOTS as u8 {
            if let Some(child) = node.child(slot) {
                stack.push((ns, child, [&base[..], &[slot]].concat()));
            }
        }
    }
    seen
}

/// The bitmap root of every term in the committed index, namespaced by the
/// term's routing path.
fn bitmap_roots(
    db: &impl Store,
) -> Vec<(Hash, RootRef<{ golemdb_index::BITMAP_TRIE_PATH_BYTES }>)> {
    let tx = db.begin_read().unwrap();
    scan_prefix(&tx, golemdb_index::tables::INDEX, vec![])
        .unwrap()
        .map(|row| {
            let (term, value) = row.unwrap();
            let ns = IndexTerm::decode(&term)
                .unwrap()
                .routing_path(&Keccak256Hasher);
            (ns, RootRef::decode(&value).unwrap())
        })
        .collect()
}

#[test]
fn commits_store_only_trie_rows_reachable_from_committed_roots() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let branches = Branches::new(db, Keccak256Hasher, None).unwrap();
    let (mut states, mut indexes) = (Vec::new(), Vec::new());
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
        states.push(([], sealed.cells.root));
        indexes.push(([], sealed.index.root));
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
        rows(tables::CELL_TRIE.table()),
        reachable_rows(
            &tx,
            tables::CELL_TRIE,
            golemdb_cells::CELL_BRANCH_DOMAIN,
            &states
        )
    );
    assert_eq!(
        rows(golemdb_index::tables::INDEX_TRIE.table()),
        reachable_rows(
            &tx,
            golemdb_index::tables::INDEX_TRIE,
            golemdb_index::INDEX_BRANCH_DOMAIN,
            &indexes
        )
    );
    assert_eq!(
        rows(golemdb_index::tables::BITMAP_TRIE.table()),
        reachable_rows(
            &tx,
            golemdb_index::tables::BITMAP_TRIE,
            golemdb_index::BITMAP_BRANCH_DOMAIN,
            &bitmaps
        )
    );
    // Nothing needed was dropped: the head trie reaches every current cell, and
    // every current term's posting list loads.
    let head = crate::head::read_head_state(&tx).unwrap();
    let hasher = Keccak256Hasher;
    let roots = crate::head_roots(&tx, &head, &hasher).unwrap();
    Cells::new(&hasher).validate_root(&tx, roots.state).unwrap();
    let root = roots.state;
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

#[test]
fn every_row_a_commit_writes_is_keyed_by_its_commit_number() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let branches = Branches::new(db, Keccak256Hasher, None).unwrap();
    let versioned: Vec<_> = (tables::VERSIONED.into_iter())
        .chain(golemdb_index::tables::VERSIONED)
        .collect();
    let keys = |table| {
        let tx = branches.store().begin_read().unwrap();
        scan_prefix(&tx, table, vec![])
            .unwrap()
            .map(|row| row.unwrap().0)
            .collect::<std::collections::BTreeSet<_>>()
    };
    for commit in 1..=3u64 {
        let before: Vec<_> = versioned.iter().map(|&table| keys(table)).collect();
        let branch = branches.begin().unwrap();
        branches
            .write(branch, |cells| {
                for i in 64..80u64 {
                    // Three bitmap containers per term, so every table gets rows.
                    let id = golemdb_index::path::join(i % 3, i as u16).unwrap();
                    cells.put(
                        key(id, b"tag"),
                        value(&format!("t{}", (i + commit) % 2), true),
                    );
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(
            branches.commit(branch).unwrap(),
            crate::CommitId::new(commit)
        );
        let tx = branches.store().begin_read().unwrap();
        // The state root is rewritten by every commit; this batch also changes the index.
        let roots = crate::read_roots(&tx, crate::CommitId::new(commit))
            .unwrap()
            .unwrap();
        assert!(matches!(roots.state, RootRef::Branch(b) if b.commit == commit));
        assert!(matches!(roots.index, RootRef::Branch(b) if b.commit == commit));
        drop(tx);
        for (&table, before) in versioned.iter().zip(before) {
            let after = keys(table);
            assert!(before.is_subset(&after));
            let added: Vec<_> = after.difference(&before).cloned().collect();
            assert!(!added.is_empty());
            for key in added {
                assert_eq!(key[..golemdb_merkle::COMMIT_BYTES], commit.to_be_bytes());
            }
        }
    }
}

#[test]
fn every_commit_records_roots_that_keep_its_tries_walkable() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let branches = Branches::new(db, Keccak256Hasher, None).unwrap();
    let mut published = Vec::new();
    for commit in 1..=3 {
        let text = format!("v{commit}");
        let branch = stage(&branches, &text);
        let sealed = branches.seal(branch).unwrap();
        branches.commit(branch).unwrap();
        let roots = crate::Roots {
            state: sealed.cells.root,
            index: sealed.index.root,
        };
        published.push((crate::CommitId::new(commit), roots, value(&text, true)));
    }
    let tx = branches.store().begin_read().unwrap();
    // The seeded empty state has no branch rows to locate.
    let genesis = crate::read_roots(&tx, crate::CommitId::new(0))
        .unwrap()
        .unwrap();
    assert_eq!(
        (genesis.state, genesis.index),
        (RootRef::Empty, RootRef::Empty)
    );
    let cell_trie = golemdb_merkle::Trie::<_, { golemdb_cells::CELL_TRIE_PATH_BYTES }>::new(
        tables::CELL_TRIE,
        golemdb_cells::CELL_BRANCH_DOMAIN,
        &Keccak256Hasher,
    );
    let index_trie = golemdb_merkle::Trie::<_, { golemdb_index::INDEX_TRIE_PATH_BYTES }>::new(
        golemdb_index::tables::INDEX_TRIE,
        golemdb_index::INDEX_BRANCH_DOMAIN,
        &Keccak256Hasher,
    );
    let path = Keccak256Hasher.hash(&key(64, b"name").encode());
    for (commit, roots, value) in published {
        assert_eq!(crate::read_roots(&tx, commit).unwrap(), Some(roots));
        // Each old state trie still holds that commit's value, after later commits.
        let leaf = cell_trie.get(&tx, roots.state, &path).unwrap().unwrap();
        let expected = Keccak256Hasher.hash_parts(&[
            &[golemdb_cells::CELL_LEAF_DOMAIN],
            &path,
            value.encoded_bytes(),
        ]);
        assert_eq!(leaf.hash, expected);
        assert_eq!(index_trie.walk(&tx, roots.index).count(), 1);
    }
}

#[test]
fn head_roots_must_exist_and_match_the_head() {
    let hasher = Keccak256Hasher;
    let empty = hasher.hash(&[]);
    let branch_at = |commit| {
        RootRef::Branch(golemdb_merkle::BranchRef {
            hash: [7; 32],
            commit,
        })
    };
    let check = |head: crate::Head, roots: Option<crate::Roots>| {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        match roots {
            Some(roots) => {
                crate::write_head(&mut tx, head.commit_id, &roots, &hasher).unwrap();
            }
            None => crate::head::put_head(&mut tx, &head).unwrap(),
        }
        crate::head_roots(&tx, &head, &hasher)
    };
    let roots = |state| crate::Roots {
        state,
        index: RootRef::Empty,
    };
    let head = |state_root| crate::Head {
        commit_id: crate::CommitId::new(5),
        state_root,
        index_root: empty,
    };
    // Empty tries need no row; a branch root does.
    assert_eq!(check(head(empty), None).unwrap(), roots(RootRef::Empty));
    assert!(matches!(
        check(head([7; 32]), None),
        Err(BranchError::MissingRoots(5))
    ));
    assert_eq!(
        check(head([7; 32]), Some(roots(branch_at(5)))).unwrap(),
        roots(branch_at(5))
    );
    // A row for other roots.
    assert!(matches!(
        check(head([7; 32]), Some(roots(RootRef::Empty))),
        Err(BranchError::InvalidRoots(5))
    ));
}

#[test]
fn malformed_roots_rows_are_invalid() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    let commit = crate::CommitId::new(3);
    // Empty; a state length past the end; an index root too short; a leaf
    // root whose path has the wrong width.
    let short_path = [&[0][..], &[1; golemdb_merkle::BRANCH_ROOT_BYTES + 3]].concat();
    for bytes in [&[][..], &[5, 1, 2], &[0, 9], &short_path] {
        tx.put(crate::ROOTS, &commit.get().to_be_bytes(), bytes)
            .unwrap();
        assert!(matches!(
            crate::read_roots(&tx, commit),
            Err(BranchError::InvalidRoots(3))
        ));
    }
}

#[test]
fn a_failed_append_keeps_the_sealed_branch_retryable() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let branches = Branches::new(db.clone(), Keccak256Hasher, None).unwrap();
    let branch = stage(&branches, "Alice");
    let sealed = branches.seal(branch).unwrap();
    // Occupy the sealed root's row, so commit's append of it fails.
    let RootRef::Branch(root) = sealed.cells.root else {
        panic!("two cells make a branch root");
    };
    let row = golemdb_merkle::Trie::<_, { golemdb_cells::CELL_TRIE_PATH_BYTES }>::new(
        tables::CELL_TRIE,
        golemdb_cells::CELL_BRANCH_DOMAIN,
        &Keccak256Hasher,
    )
    .node_key(root.commit, &[]);
    let mut tx = db.begin_write().unwrap();
    tx.put(tables::CELL_TRIE.table(), &row, b"taken").unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        branches.commit(branch),
        Err(BranchError::Storage(
            golemdb_storage::StorageError::AlreadyExists
        ))
    ));
    assert!(branches.branch_info(branch).unwrap().sealed);
    assert_eq!(branches.head().unwrap().get(), 0);
    let mut tx = db.begin_write().unwrap();
    tx.delete(tables::CELL_TRIE.table(), &row).unwrap();
    tx.commit().unwrap();
    assert_eq!(branches.commit(branch).unwrap().get(), 1);
}

#[test]
fn heads_are_published_once_and_in_order() {
    let hasher = Keccak256Hasher;
    let empty = crate::Roots {
        state: RootRef::Empty,
        index: RootRef::Empty,
    };
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    assert_eq!(
        crate::init_head(&mut tx, &empty, &hasher)
            .unwrap()
            .commit_id
            .get(),
        0
    );
    assert!(matches!(
        crate::init_head(&mut tx, &empty, &hasher),
        Err(BranchError::HeadExists)
    ));
    let head = crate::advance_head(&mut tx, crate::CommitId::GENESIS, &empty, &hasher).unwrap();
    assert_eq!(head.commit_id.get(), 1);
    // An outdated head, as after a lost race or a retry.
    assert!(matches!(
        crate::advance_head(&mut tx, crate::CommitId::GENESIS, &empty, &hasher),
        Err(BranchError::Conflict)
    ));
    // The low-level writer still refuses a commit that is not newer.
    for commit in [0, 1] {
        assert!(matches!(
            crate::write_head(&mut tx, crate::CommitId::new(commit), &empty, &hasher),
            Err(BranchError::CommitNotNewer { commit: c, latest: 1 }) if c == commit
        ));
    }
    assert_eq!(crate::read_head(&tx).unwrap().get(), 1);
}

#[test]
fn a_stale_roots_row_fails_the_commit_and_keeps_the_branch_retryable() {
    let db = MemoryStore::new();
    seed(&db, &Keccak256Hasher, 0, &[]);
    let branches = Branches::new(db.clone(), Keccak256Hasher, None).unwrap();
    let branch = stage(&branches, "Alice");
    branches.seal(branch).unwrap();
    // A row for the commit about to be published: `Roots` is appended, never
    // overwritten.
    let key = 1u64.to_be_bytes();
    let mut tx = db.begin_write().unwrap();
    tx.put(crate::ROOTS, &key, b"stale").unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        branches.commit(branch),
        Err(BranchError::Storage(
            golemdb_storage::StorageError::AlreadyExists
        ))
    ));
    assert!(branches.branch_info(branch).unwrap().sealed);
    assert_eq!(branches.head().unwrap().get(), 0);
    let mut tx = db.begin_write().unwrap();
    tx.delete(crate::ROOTS, &key).unwrap();
    tx.commit().unwrap();
    assert_eq!(branches.commit(branch).unwrap().get(), 1);
}
