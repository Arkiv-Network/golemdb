use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Included, Unbounded},
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{
    Entry, ReadCursor, ReadTransaction, Result, StorageError, Store, Table, WriteTransaction,
};

type Rows = BTreeMap<Vec<u8>, Vec<u8>>;
type State = BTreeMap<Table, Rows>;

struct Inner {
    committed: Mutex<Arc<State>>,
    writer: Mutex<()>,
}

/// Cloning the store shares its state. Write transactions copy the state;
/// read transactions cheaply retain its previous committed snapshot.
#[derive(Clone)]
pub struct MemoryStore {
    inner: Arc<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                committed: Mutex::new(Arc::new(State::new())),
                writer: Mutex::new(()),
            }),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

pub struct MemoryReadTransaction {
    state: Arc<State>,
}

pub struct MemoryWriteTransaction<'db> {
    inner: &'db Inner,
    state: State,
    _writer: MutexGuard<'db, ()>,
}

impl Store for MemoryStore {
    type Read<'db> = MemoryReadTransaction;
    type Write<'db> = MemoryWriteTransaction<'db>;

    fn begin_read(&self) -> Result<Self::Read<'_>> {
        let state = self
            .inner
            .committed
            .lock()
            .map_err(|_| StorageError::Poisoned("publication"))?
            .clone();
        Ok(MemoryReadTransaction { state })
    }

    fn begin_write(&self) -> Result<Self::Write<'_>> {
        let writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| StorageError::Poisoned("writer"))?;
        // Acquire writer ownership before reading the latest committed snapshot.
        let snapshot = self.begin_read()?;
        Ok(MemoryWriteTransaction {
            inner: &self.inner,
            state: (*snapshot.state).clone(),
            _writer: writer,
        })
    }
}

fn rows(state: &State, table: Table) -> Result<Option<&Rows>> {
    table.validate()?;
    Ok(state.get(&table))
}

impl ReadTransaction for MemoryReadTransaction {
    type Cursor<'tx> = MemoryCursor<'tx>;

    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let rows = rows(&self.state, table)?;
        Ok(rows.and_then(|rows| rows.get(key)).cloned())
    }

    fn cursor(&self, table: Table, key: &[u8]) -> Result<Self::Cursor<'_>> {
        Ok(MemoryCursor::new(rows(&self.state, table)?, key))
    }
}

impl ReadTransaction for MemoryWriteTransaction<'_> {
    type Cursor<'tx>
        = MemoryCursor<'tx>
    where
        Self: 'tx;

    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let rows = rows(&self.state, table)?;
        Ok(rows.and_then(|rows| rows.get(key)).cloned())
    }

    fn cursor(&self, table: Table, key: &[u8]) -> Result<Self::Cursor<'_>> {
        Ok(MemoryCursor::new(rows(&self.state, table)?, key))
    }
}

impl WriteTransaction for MemoryWriteTransaction<'_> {
    fn is_pristine(&self) -> Result<bool> {
        Ok(self.state.is_empty())
    }

    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        table.validate()?;
        self.state
            .entry(table)
            .or_default()
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn append(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        table.validate()?;
        let rows = self.state.entry(table).or_default();
        match rows.last_key_value() {
            Some((last, _)) if last.as_slice() == key => Err(StorageError::AlreadyExists),
            Some((last, _)) if last.as_slice() > key => Err(StorageError::OutOfOrder),
            _ => {
                rows.insert(key.to_vec(), value.to_vec());
                Ok(())
            }
        }
    }

    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        table.validate()?;
        match self.state.entry(table).or_default().entry(key.to_vec()) {
            std::collections::btree_map::Entry::Occupied(_) => Err(StorageError::AlreadyExists),
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(value.to_vec());
                Ok(())
            }
        }
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<bool> {
        table.validate()?;
        Ok(self
            .state
            .get_mut(&table)
            .is_some_and(|rows| rows.remove(key).is_some()))
    }

    fn commit(self) -> Result<()> {
        let mut committed = self
            .inner
            .committed
            .lock()
            .map_err(|_| StorageError::Poisoned("publication"))?;
        *committed = Arc::new(self.state);
        Ok(())
    }
}

enum Position {
    Key(Vec<u8>),
    Before,
    At(Vec<u8>),
    After,
}

pub struct MemoryCursor<'tx> {
    rows: Option<&'tx Rows>,
    position: Position,
}

impl<'tx> MemoryCursor<'tx> {
    fn new(rows: Option<&'tx Rows>, key: &[u8]) -> Self {
        Self {
            rows,
            position: Position::Key(key.to_vec()),
        }
    }

    fn position(&mut self, row: Option<(&Vec<u8>, &Vec<u8>)>, end: Position) -> Option<Entry> {
        match row {
            Some((key, value)) => {
                self.position = Position::At(key.clone());
                Some((key.clone(), value.clone()))
            }
            None => {
                self.position = end;
                None
            }
        }
    }
}

impl ReadCursor for MemoryCursor<'_> {
    fn next(&mut self) -> Result<Option<Entry>> {
        let Some(rows) = self.rows else {
            return Ok(None);
        };
        let row = match &self.position {
            Position::Before => rows.first_key_value(),
            Position::Key(key) => rows
                .range::<[u8], _>((Included(key.as_slice()), Unbounded))
                .next(),
            Position::At(key) => rows
                .range::<[u8], _>((Excluded(key.as_slice()), Unbounded))
                .next(),
            Position::After => None,
        };
        Ok(self.position(row, Position::After))
    }

    fn prev(&mut self) -> Result<Option<Entry>> {
        let Some(rows) = self.rows else {
            return Ok(None);
        };
        let row = match &self.position {
            Position::After => rows.last_key_value(),
            Position::Key(key) => rows
                .range::<[u8], _>((Unbounded, Included(key.as_slice())))
                .next_back(),
            Position::At(key) => rows
                .range::<[u8], _>((Unbounded, Excluded(key.as_slice())))
                .next_back(),
            Position::Before => None,
        };
        Ok(self.position(row, Position::Before))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_successful_writes_create_tables_and_creation_is_transactional() {
        let db = MemoryStore::new();
        let table = Table("created-on-write");
        let read = db.begin_read().unwrap();
        assert_eq!(read.get(table, b"k").unwrap(), None);
        assert_eq!(read.cursor(table, b"k").unwrap().next().unwrap(), None);
        assert!(read.state.is_empty());
        let mut tx = db.begin_write().unwrap();
        assert!(!tx.delete(table, b"k").unwrap());
        assert_eq!(tx.cursor(table, b"k").unwrap().prev().unwrap(), None);
        assert!(tx.state.is_empty());
        tx.commit().unwrap();
        assert!(db.begin_read().unwrap().state.is_empty());
        let mut tx = db.begin_write().unwrap();
        tx.put(table, b"k", b"v").unwrap();
        assert!(tx.state.contains_key(&table));
        tx.abort();
        assert!(db.begin_read().unwrap().state.is_empty());
        {
            let mut tx = db.begin_write().unwrap();
            tx.insert(table, b"k", b"v").unwrap();
        }
        assert!(db.begin_read().unwrap().state.is_empty());
        let mut tx = db.begin_write().unwrap();
        tx.insert(table, b"k", b"v").unwrap();
        tx.commit().unwrap();
        assert!(db.begin_read().unwrap().state.contains_key(&table));
        assert!(read.state.is_empty());
    }
}
