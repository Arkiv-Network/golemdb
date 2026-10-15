use crate::support::mdbx as fixture;

use std::{cell::RefCell, collections::BTreeMap};

use golemdb_cells::CellType;
use golemdb_index::{Index, IndexError, IndexTerm, PostingChange, tables};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_storage::{
    ReadTransaction, Result, StorageError, Store, Table, WriteTransaction, scan_prefix,
};

const HASH: Keccak256Hasher = Keccak256Hasher;
fn term(value: u8) -> IndexTerm {
    IndexTerm::new("tag", CellType::Str, &[value]).unwrap()
}
fn add(term: IndexTerm, id: u64) -> PostingChange {
    PostingChange::Add {
        term,
        record_id: id,
    }
}

#[derive(Default)]
struct Counts {
    hits: BTreeMap<Table, usize>,
    writes: BTreeMap<Table, usize>,
}
struct Observed<T> {
    tx: T,
    counts: RefCell<Counts>,
    fail_table: Option<Table>,
}
impl<T> Observed<T> {
    fn new(tx: T) -> Self {
        Self {
            tx,
            counts: RefCell::default(),
            fail_table: None,
        }
    }
    fn writing(&self, table: Table) -> Result<()> {
        *self.counts.borrow_mut().writes.entry(table).or_default() += 1;
        if self.fail_table == Some(table) {
            return Err(StorageError::Backend(
                std::io::Error::other("injected write failure").into(),
            ));
        }
        Ok(())
    }
    fn reset(&self) {
        *self.counts.borrow_mut() = Counts::default();
    }
}
impl<T: ReadTransaction> ReadTransaction for Observed<T> {
    type Cursor<'a>
        = T::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, t: Table, k: &[u8]) -> Result<Option<Vec<u8>>> {
        let result = self.tx.get(t, k)?;
        if result.is_some() {
            *self.counts.borrow_mut().hits.entry(t).or_default() += 1;
        }
        Ok(result)
    }
    fn cursor(&self, t: Table, k: &[u8]) -> Result<Self::Cursor<'_>> {
        self.tx.cursor(t, k)
    }
}
impl<T: WriteTransaction> WriteTransaction for Observed<T> {
    fn put(&mut self, t: Table, k: &[u8], v: &[u8]) -> Result<()> {
        self.writing(t)?;
        self.tx.put(t, k, v)
    }
    fn insert(&mut self, t: Table, k: &[u8], v: &[u8]) -> Result<()> {
        self.writing(t)?;
        self.tx.insert(t, k, v)
    }
    fn delete(&mut self, t: Table, k: &[u8]) -> Result<bool> {
        self.writing(t)?;
        self.tx.delete(t, k)
    }
    fn commit(self) -> Result<()> {
        self.tx.commit()
    }
    fn abort(self) {
        self.tx.abort()
    }
}

#[test]
fn one_chunk_update_does_not_rebuild_256_chunk_postings() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = Observed::new(db.begin_write().unwrap());
    let update = index
        .apply(
            &mut tx,
            RootRef::Empty,
            (0u64..256).flat_map(|hi| [add(term(0), hi << 16), add(term(1), hi << 16)]),
        )
        .unwrap();
    tx.reset();
    let update = index
        .apply(
            &mut tx,
            update.root,
            [add(term(0), 1), add(term(0), 2), add(term(0), 1)],
        )
        .unwrap();
    assert_eq!(
        tx.counts.borrow().hits.get(&tables::BITMAP_CONTAINER),
        Some(&1)
    );
    assert_eq!(
        tx.counts.borrow().writes,
        BTreeMap::from([
            (tables::BITMAP_CONTAINER, 1),
            (tables::BITMAP_TRIE, 2),
            (tables::INDEX_TRIE, 1),
            (tables::INDEX, 1)
        ])
    );
    tx.reset();
    let noop = index
        .apply(&mut tx, update.root, [add(term(0), 1), add(term(0), 2)])
        .unwrap();
    assert!(noop.changed_terms.is_empty());
    assert!(tx.counts.borrow().writes.is_empty());
    tx.reset();
    let bitmap = index.bitmap(&tx, &term(0)).unwrap().unwrap();
    assert!(bitmap.treemap().contains(2));
    assert_eq!(
        tx.counts.borrow().hits.get(&tables::BITMAP_CONTAINER),
        Some(&256)
    );
    assert!(tx.counts.borrow().writes.is_empty());
}

#[test]
fn reopened_singleton_loads_and_finalizes_its_container_once() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = Observed::new(db.begin_write().unwrap());
    let root = index
        .apply(&mut tx, RootRef::Empty, [add(term(0), 1)])
        .unwrap()
        .root;
    tx.reset();
    index
        .apply(&mut tx, root, [add(term(0), 2), add(term(0), 3)])
        .unwrap();
    assert_eq!(
        tx.counts.borrow().hits.get(&tables::BITMAP_CONTAINER),
        Some(&1)
    );
    assert_eq!(
        tx.counts.borrow().writes.get(&tables::BITMAP_CONTAINER),
        Some(&1)
    );
}

#[test]
fn failure_after_both_tries_change_is_rolled_back_with_flat_state() {
    let (_dir, db) = fixture::db();
    let index = Index::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let root = index
        .apply(&mut tx, RootRef::Empty, [add(term(0), 1), add(term(1), 1)])
        .unwrap()
        .root;
    tx.commit().unwrap();
    let snapshot = db.begin_read().unwrap();
    let all_tables = [
        tables::INDEX,
        tables::INDEX_TRIE,
        tables::BITMAP_TRIE,
        tables::BITMAP_CONTAINER,
    ];
    let before = all_tables.map(|table| {
        scan_prefix(&snapshot, table, vec![])
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
    });
    let mut tx = Observed::new(db.begin_write().unwrap());
    tx.fail_table = Some(tables::INDEX);
    assert!(matches!(
        index.apply(&mut tx, root, [add(term(0), 65536)]),
        Err(IndexError::Storage(_))
    ));
    assert_eq!(
        tx.counts.borrow().writes.get(&tables::BITMAP_TRIE),
        Some(&1)
    );
    assert_eq!(tx.counts.borrow().writes.get(&tables::INDEX_TRIE), Some(&1));
    tx.abort();
    let latest = db.begin_read().unwrap();
    for (table, old) in all_tables.into_iter().zip(before) {
        assert_eq!(
            scan_prefix(&latest, table, vec![])
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap(),
            old
        );
    }
    assert_eq!(index.reopen(&latest, root.hash(&HASH)).unwrap(), root);
    let bitmap = index.bitmap(&latest, &term(0)).unwrap().unwrap();
    assert!(!bitmap.treemap().contains(65536));
    assert!(bitmap.treemap().contains(1));
}
