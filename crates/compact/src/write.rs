use std::ops::Bound;

use golemdb_storage::{Store, Table, WriteTransaction, scan};

use crate::Result;

/// Delete the rows of `table` below `upper` that `doomed` picks, `rows` per
/// transaction. Returns how many.
pub(crate) fn delete(
    store: &impl Store,
    table: Table,
    upper: Bound<Vec<u8>>,
    rows: usize,
    doomed: impl Fn(&[u8]) -> Result<bool>,
) -> Result<u64> {
    let (mut lower, mut deleted) = (Bound::Unbounded, 0);
    loop {
        let (mut batch, mut last) = (Vec::new(), None);
        {
            let read = store.begin_read()?;
            for row in scan(&read, table, lower.clone(), upper.clone())?.take(rows) {
                let key = row?.0;
                if doomed(&key)? {
                    batch.push(key.clone());
                }
                last = Some(key);
            }
        }
        let Some(last) = last else {
            return Ok(deleted);
        };
        let mut tx = store.begin_write()?;
        for key in &batch {
            tx.delete(table, key)?;
        }
        tx.commit()?;
        deleted += batch.len() as u64;
        lower = Bound::Excluded(last);
    }
}

pub(crate) struct Writer<'a, S: Store> {
    store: &'a S,
    tx: Option<S::Write<'a>>,
    rows: usize,
    batch: usize,
}

impl<'a, S: Store> Writer<'a, S> {
    pub(crate) fn new(store: &'a S, batch: usize) -> Result<Self> {
        Ok(Self {
            store,
            tx: Some(store.begin_write()?),
            rows: 0,
            batch,
        })
    }

    pub(crate) fn tx(&mut self) -> &mut S::Write<'a> {
        self.tx
            .as_mut()
            .expect("a transaction is open between batches")
    }

    pub(crate) fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        self.tx().put(table, key, value)?;
        self.wrote()
    }

    pub(crate) fn wrote(&mut self) -> Result<()> {
        self.rows += 1;
        if self.rows == self.batch {
            self.rows = 0;
            self.tx.take().expect("a transaction is open").commit()?;
            self.tx = Some(self.store.begin_write()?);
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        Ok(self.tx.take().expect("a transaction is open").commit()?)
    }
}
