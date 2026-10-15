use golemdb_cells::{CellKey, CellNameRef, CellType, CellValue, CellValueRef, tables as cells};
use golemdb_index::tables as index;
use golemdb_merkle::Keccak256Hasher;
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};

use crate::{BranchError, Branches, head::read_head_state, tables::NODE_REFS as REFS};

const HASH: Keccak256Hasher = Keccak256Hasher;

fn key(id: u64, name: &str) -> CellKey {
    CellKey::new(id, CellNameRef::raw(name.as_bytes()))
}

fn value(text: &str) -> CellValue {
    CellValueRef::new(CellType::Str, text.as_bytes(), true)
        .unwrap()
        .into()
}

fn database() -> MemoryStore {
    let db = MemoryStore::new();
    crate::genesis::create(&db, &HASH, []).unwrap();
    db
}

fn rows(tx: &impl ReadTransaction, table: Table) -> Vec<golemdb_storage::Entry> {
    scan_prefix(tx, table, Vec::new())
        .unwrap()
        .collect::<golemdb_storage::Result<_>>()
        .unwrap()
}

fn mutate(branches: &Branches<MemoryStore, Keccak256Hasher>, values: &[(u64, &str, &str)]) {
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            for &(id, name, text) in values {
                cells.put(key(id, name), value(text));
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.commit(branch).unwrap();
}

fn assert_bounded(db: &MemoryStore) {
    let tx = db.begin_read().unwrap();
    let cell_rows = rows(&tx, cells::CELL);
    let terms = rows(&tx, index::INDEX);
    assert!(rows(&tx, cells::CELL_TRIE).len() <= cell_rows.len().saturating_sub(1));
    assert!(rows(&tx, index::INDEX_TRIE).len() <= terms.len().saturating_sub(1));
    let physical = [
        cells::CELL_TRIE,
        index::INDEX_TRIE,
        index::BITMAP_TRIE,
        index::BITMAP_CONTAINER,
    ];
    // Only counts above one are stored, each for a live physical row.
    for (key, count) in rows(&tx, REFS) {
        assert!(u64::from_be_bytes(count.as_slice().try_into().unwrap()) > 1);
        let table = physical[key[0] as usize];
        assert!(tx.get(table, &key[1..]).unwrap().is_some());
    }
}

#[test]
fn commits_collect_by_default_and_remain_bounded_under_churn() {
    let db = database();
    let branches = Branches::new(db.clone(), HASH).unwrap();
    for round in 0..80 {
        mutate(
            &branches,
            &[
                (64, "a", if round % 2 == 0 { "x" } else { "y" }),
                (65536, "b", "z"),
            ],
        );
        assert_bounded(&db);
    }
    assert_eq!(
        crate::read_head(&db.begin_read().unwrap()).unwrap(),
        crate::CommitId::new(80)
    );
    let reopened = Branches::new(db.clone(), HASH).unwrap();
    mutate(&reopened, &[(64, "a", "final")]);
    assert_bounded(&db);
}

#[test]
fn shared_bitmap_roots_and_children_survive_until_last_owner_leaves() {
    let db = database();
    let branches = Branches::new(db.clone(), HASH).unwrap();
    mutate(
        &branches,
        &[
            (64, "a", "x"),
            (65536, "a", "x"),
            (64, "b", "x"),
            (65536, "b", "x"),
        ],
    );
    let snapshot = db.begin_read().unwrap();
    let old_bitmap = rows(&snapshot, index::BITMAP_TRIE);
    assert_eq!(old_bitmap.len(), 1);
    mutate(&branches, &[(64, "a", "y")]);
    let tx = db.begin_read().unwrap();
    assert_eq!(rows(&tx, index::BITMAP_TRIE), old_bitmap);
    let ref_key = [vec![2], old_bitmap[0].0.clone()].concat();
    // One remaining owner: the count is implicit.
    assert_eq!(tx.get(REFS, &ref_key).unwrap(), None);
    drop(tx);
    mutate(&branches, &[(64, "b", "y")]);
    let tx = db.begin_read().unwrap();
    assert!(rows(&tx, index::BITMAP_TRIE).is_empty());
    assert_eq!(rows(&tx, index::BITMAP_CONTAINER).len(), 2);
    for (hash, _) in rows(&tx, index::BITMAP_CONTAINER) {
        let ref_key = [vec![3], hash].concat();
        assert_eq!(
            tx.get(REFS, &ref_key).unwrap(),
            Some(2u64.to_be_bytes().to_vec())
        );
    }
    // The old physical branch and all its containers remain in the old snapshot.
    assert_eq!(rows(&snapshot, index::BITMAP_TRIE), old_bitmap);
    assert_bounded(&db);
}

#[test]
fn corrupt_reference_count_aborts_seal_and_keeps_the_branch_open_for_retry() {
    let db = database();
    let branches = Branches::new(db.clone(), HASH).unwrap();
    mutate(&branches, &[(64, "a", "x"), (65, "b", "y")]);
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(64, "a"), value("z"));
            Ok::<_, ()>(())
        })
        .unwrap();
    let head = read_head_state(&db.begin_read().unwrap()).unwrap();
    let ref_key = [vec![0], head.state_root.to_vec()].concat();
    // The root has one owner, so its count is implicit; a stored count must
    // be eight bytes and above one.
    assert_eq!(db.begin_read().unwrap().get(REFS, &ref_key).unwrap(), None);
    for corrupt in [vec![0; 8], 1u64.to_be_bytes().to_vec(), vec![1]] {
        let mut tx = db.begin_write().unwrap();
        tx.put(REFS, &ref_key, &corrupt).unwrap();
        tx.commit().unwrap();
        let before = db.begin_read().unwrap();
        assert!(matches!(
            branches.seal(branch),
            Err(BranchError::GarbageCollection(_))
        ));
        assert!(matches!(
            branches.commit(branch),
            Err(BranchError::GarbageCollection(_))
        ));
        let after = db.begin_read().unwrap();
        for table in crate::tables::ENGINE {
            assert_eq!(rows(&before, table), rows(&after, table));
        }
        assert!(!branches.branch_info(branch).unwrap().sealed);
        let mut tx = db.begin_write().unwrap();
        tx.delete(REFS, &ref_key).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        branches.commit(branch).unwrap(),
        crate::CommitId::new(head.commit_id.get() + 1)
    );
    assert_bounded(&db);
}

// Counts reads of stored reference counts through the seal buffer.
struct RefReads<R> {
    tx: R,
    reads: std::cell::Cell<usize>,
}

impl<R: ReadTransaction> ReadTransaction for RefReads<R> {
    type Cursor<'tx>
        = R::Cursor<'tx>
    where
        Self: 'tx;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if table == REFS {
            self.reads.set(self.reads.get() + 1);
        }
        self.tx.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.tx.cursor(table, key)
    }
}

#[test]
fn copied_siblings_are_netted_without_reading_their_counts() {
    let db = database();
    let branches = Branches::new(db.clone(), HASH).unwrap();
    let plain = |text: &str| -> CellValue {
        CellValueRef::new(CellType::Str, text.as_bytes(), false)
            .unwrap()
            .into()
    };
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            for id in 64..64 + 4096 {
                cells.put(key(id, "a"), plain("x"));
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    branches.commit(branch).unwrap();

    let origin = RefReads {
        tx: db.begin_read().unwrap(),
        reads: std::cell::Cell::new(0),
    };
    let head = read_head_state(&origin).unwrap();
    let (cells, index) = (
        golemdb_cells::Cells::new(&HASH),
        golemdb_index::Index::new(&HASH),
    );
    let cell_root = cells.reopen(&origin, head.state_root).unwrap();
    let index_root = index.reopen(&origin, head.index_root).unwrap();
    let mut buffer = crate::buffer::Buffered::new(&origin);
    let change = golemdb_cells::CellChange::Put {
        key: key(64 + 7, "a"),
        value: plain("y"),
    };
    let (cells_update, index_update) =
        crate::state::apply(&mut buffer, &HASH, cell_root, index_root, [change]).unwrap();
    crate::gc::update(
        &mut buffer,
        crate::gc::RETENTION,
        cell_root,
        index_root,
        &cells_update,
        &index_update,
    )
    .unwrap();
    // The edited path crosses four branches. Each has up to 15 unchanged
    // siblings whose +1 and -1 cancel; counting them read 47 counts here.
    assert_eq!(origin.reads.get(), 4);
}
