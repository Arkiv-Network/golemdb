//! Storage writes staged over a committed read snapshot, never a store writer.
use golemdb_storage::{
    Entry, ReadCursor, ReadTransaction, Result, StorageError, Table, WriteTransaction,
};
use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Included, Unbounded},
};

type Rows = BTreeMap<Vec<u8>, Option<Vec<u8>>>;
pub(crate) type Writes = BTreeMap<Table, Rows>;

pub(crate) struct Buffered<'a, R> {
    origin: &'a R,
    writes: Writes,
}

impl<'a, R> Buffered<'a, R> {
    pub(crate) fn new(origin: &'a R) -> Self {
        Self {
            origin,
            writes: BTreeMap::new(),
        }
    }
    pub(crate) fn into_writes(self) -> Writes {
        self.writes
    }

    pub(crate) fn writes(&self) -> &Writes {
        &self.writes
    }

    /// Stage deleting a row the caller knows exists, without reading it.
    pub(crate) fn delete_existing(&mut self, table: Table, key: &[u8]) {
        self.writes
            .entry(table)
            .or_default()
            .insert(key.to_vec(), None);
    }
}

fn validate(table: Table) -> Result<()> {
    if table.0.is_empty() || table.0.contains('\0') {
        return Err(StorageError::InvalidTableName(table));
    }
    Ok(())
}

impl<R: ReadTransaction> ReadTransaction for Buffered<'_, R> {
    type Cursor<'tx>
        = BufferedCursor<'tx, R>
    where
        Self: 'tx;
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>> {
        validate(table)?;
        match self.writes.get(&table).and_then(|rows| rows.get(key)) {
            Some(value) => Ok(value.clone()),
            None => self.origin.get(table, key),
        }
    }
    fn cursor(&self, table: Table, key: &[u8]) -> Result<Self::Cursor<'_>> {
        validate(table)?;
        Ok(BufferedCursor {
            origin: self.origin,
            table,
            cursor: self.origin.cursor(table, key)?,
            rows: self.writes.get(&table),
            position: Position::Start(key.to_vec()),
        })
    }
}

impl<R: ReadTransaction> WriteTransaction for Buffered<'_, R> {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        validate(table)?;
        self.writes
            .entry(table)
            .or_default()
            .insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        if self.get(table, key)?.is_some() {
            return Err(StorageError::AlreadyExists);
        }
        self.put(table, key, value)
    }
    fn delete(&mut self, table: Table, key: &[u8]) -> Result<bool> {
        let existed = self.get(table, key)?.is_some();
        if existed {
            self.writes
                .entry(table)
                .or_default()
                .insert(key.to_vec(), None);
        }
        Ok(existed)
    }
    fn commit(self) -> Result<()> {
        Err(StorageError::Backend("a seal buffer cannot commit".into()))
    }
}

enum Position {
    Start(Vec<u8>),
    At(Vec<u8>),
    Before,
    After,
}

pub(crate) struct BufferedCursor<'a, R: ReadTransaction + 'a> {
    origin: &'a R,
    table: Table,
    cursor: R::Cursor<'a>,
    rows: Option<&'a Rows>,
    position: Position,
}

impl<R: ReadTransaction> BufferedCursor<'_, R> {
    fn step(&mut self, forward: bool) -> Result<Option<Entry>> {
        if matches!(
            (&self.position, forward),
            (Position::After, true) | (Position::Before, false)
        ) {
            return Ok(None);
        }
        // The last merged row may come only from the overlay. At an exhausted
        // boundary, the origin cursor is already there and can reverse directly.
        let bound = match &self.position {
            Position::Start(key) => {
                self.cursor = self.origin.cursor(self.table, key)?;
                Included(key.as_slice())
            }
            Position::At(key) => {
                self.cursor = self.origin.cursor(self.table, key)?;
                Excluded(key.as_slice())
            }
            Position::Before | Position::After => Unbounded,
        };
        let overlay = self.rows.and_then(|rows| {
            let mut range = if forward {
                rows.range::<[u8], _>((bound, Unbounded))
            } else {
                rows.range::<[u8], _>((Unbounded, bound))
            };
            if forward {
                range.find_map(|(key, value)| value.as_ref().map(|v| (key.clone(), v.clone())))
            } else {
                range
                    .rev()
                    .find_map(|(key, value)| value.as_ref().map(|v| (key.clone(), v.clone())))
            }
        });
        let origin = loop {
            let row = if forward {
                self.cursor.next()?
            } else {
                self.cursor.prev()?
            };
            let Some((key, value)) = row else {
                break None;
            };
            if matches!(&self.position, Position::At(at) if *at == key)
                || self.rows.is_some_and(|rows| rows.contains_key(&key))
            {
                continue;
            }
            break Some((key, value));
        };
        let row = match (origin, overlay) {
            (Some(a), Some(b)) => Some(if (a.0 < b.0) == forward { a } else { b }),
            (a, b) => a.or(b),
        };
        self.position = match &row {
            Some((key, _)) => Position::At(key.clone()),
            None if forward => Position::After,
            None => Position::Before,
        };
        Ok(row)
    }
}
impl<R: ReadTransaction> ReadCursor for BufferedCursor<'_, R> {
    fn next(&mut self) -> Result<Option<Entry>> {
        self.step(true)
    }
    fn prev(&mut self) -> Result<Option<Entry>> {
        self.step(false)
    }
}
