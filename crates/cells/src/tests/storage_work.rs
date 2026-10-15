use std::cell::Cell;

use crate::{CellChange, CellError, CellKey, CellNameRef, CellValue, Cells, tables};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_storage::{
    MemoryStore, ReadCursor, ReadTransaction, StorageError, Store, Table, WriteTransaction,
};

const HASH: Keccak256Hasher = Keccak256Hasher;
fn key(name: &[u8]) -> CellKey {
    CellKey::new(42, CellNameRef::raw(name))
}
fn put(name: &[u8], value: &[u8]) -> CellChange {
    CellChange::Put {
        key: key(name),
        value: CellValue::parse([&[2][..], value].concat()).unwrap(),
    }
}

struct Observed<T> {
    inner: T,
    reads: Cell<usize>,
    writes: usize,
    fail_cells: bool,
}
impl<T> Observed<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            reads: Cell::new(0),
            writes: 0,
            fail_cells: false,
        }
    }
}
impl<T: ReadTransaction> ReadTransaction for Observed<T> {
    type Cursor<'a>
        = T::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if table == tables::CELL {
            self.reads.set(self.reads.get() + 1);
        }
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.inner.cursor(table, key)
    }
}
impl<T: WriteTransaction> WriteTransaction for Observed<T> {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.writes += 1;
        if self.fail_cells && table == tables::CELL {
            return Err(StorageError::Poisoned("injected write failure"));
        }
        self.inner.put(table, key, value)
    }
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.writes += 1;
        self.inner.insert(table, key, value)
    }
    fn delete(&mut self, table: Table, key: &[u8]) -> golemdb_storage::Result<bool> {
        self.writes += 1;
        self.inner.delete(table, key)
    }
    fn commit(self) -> golemdb_storage::Result<()> {
        self.inner.commit()
    }
    fn abort(self) {
        self.inner.abort();
    }
}

#[test]
fn coalescing_reads_once_per_key_and_noops_do_not_write() {
    let db = MemoryStore::new();
    let cells = Cells::new(&HASH);
    let mut tx = Observed::new(db.begin_write().unwrap());
    let first = cells
        .apply(
            &mut tx,
            RootRef::Empty,
            [put(b"a", b"1"), put(b"a", b"2"), put(b"a", b"3")],
        )
        .unwrap();
    assert_eq!(tx.reads.get(), 1);
    assert_eq!(tx.writes, 1); // Bare singleton: no branch row.
    tx.reads.set(0);
    tx.writes = 0;
    let unchanged = cells
        .apply(
            &mut tx,
            first.root,
            [
                put(b"a", b"intermediate"),
                put(b"a", b"3"),
                put(b"temporary", b"x"),
                CellChange::Delete {
                    key: key(b"temporary"),
                },
            ],
        )
        .unwrap();
    assert_eq!(unchanged.root, first.root);
    assert!(unchanged.changed_cells.is_empty());
    assert_eq!(tx.reads.get(), 2);
    assert_eq!(tx.writes, 0);
}

#[test]
fn abort_after_a_flat_write_failure_discards_intermediate_trie_nodes() {
    let db = MemoryStore::new();
    let cells = Cells::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let root = cells
        .apply(&mut tx, RootRef::Empty, [put(b"a", b"original")])
        .unwrap()
        .root;
    tx.commit().unwrap();
    let mut tx = Observed::new(db.begin_write().unwrap());
    tx.fail_cells = true;
    assert!(matches!(
        cells.apply(&mut tx, root, [put(b"b", b"new")]),
        Err(CellError::Storage(StorageError::Poisoned(
            "injected write failure"
        )))
    ));
    // The trie write preceded the failed flat write. Caller must abort both.
    assert!(
        tx.cursor(tables::CELL_TRIE, b"")
            .unwrap()
            .next()
            .unwrap()
            .is_some()
    );
    tx.abort();
    let read = db.begin_read().unwrap();
    assert!(
        read.cursor(tables::CELL_TRIE, b"")
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    assert_eq!(
        cells.get(&read, &key(b"a")).unwrap().unwrap().as_str(),
        Some("original")
    );
    assert!(cells.get(&read, &key(b"b")).unwrap().is_none());
    assert_eq!(cells.reopen(&read, root.hash(&HASH)).unwrap(), root);
}

struct FailingRead;
struct FailingCursor {
    called: bool,
}
impl ReadCursor for FailingCursor {
    fn next(&mut self) -> golemdb_storage::Result<Option<golemdb_storage::Entry>> {
        assert!(!self.called, "iterator must stop after its first error");
        self.called = true;
        Err(StorageError::Poisoned("injected cursor failure"))
    }
    fn prev(&mut self) -> golemdb_storage::Result<Option<golemdb_storage::Entry>> {
        unreachable!()
    }
}
impl ReadTransaction for FailingRead {
    type Cursor<'a> = FailingCursor;
    fn get(&self, _: Table, _: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        Err(StorageError::Poisoned("injected read failure"))
    }
    fn cursor(&self, _: Table, _: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        Ok(FailingCursor { called: false })
    }
}

#[test]
fn storage_errors_are_not_absence_and_scan_errors_are_terminal() {
    let cells = Cells::new(&HASH);
    assert!(matches!(
        cells.get(&FailingRead, &key(b"x")),
        Err(CellError::Storage(_))
    ));
    // Creating the scan must not advance the cursor.
    let mut scan = cells.scan_record(&FailingRead, 42).unwrap();
    assert!(matches!(scan.next(), Some(Err(CellError::Storage(_)))));
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
}
