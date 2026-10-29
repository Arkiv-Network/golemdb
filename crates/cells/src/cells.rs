use std::iter::FusedIterator;

use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{ReadCursor, ReadTransaction, Scan, Table, WriteTransaction, scan_prefix};

use crate::{
    CELL_TRIE_PATH_BYTES, CellKey, CellValue, Result,
    cell_trie::{CellPath, CellTrie},
    tables,
};

/// Cell storage and commitment over caller-owned transactions.
///
/// Use one hash provider and a root matching the transaction's `Cell` table.
/// `apply` belongs at seal/commit, after the engine has formed a net batch. Seal
/// requires a staged transaction; this interface does not create an overlay.
/// Abort the transaction after any mutation error and publish roots only after
/// commit. This layer does not update the index, history or head, enforce record
/// lifecycle rules, or apply deployment limits: those belong to the engine.
pub struct Cells<'h, H: HashProvider> {
    trie: CellTrie<'h, H>,
}

/// Final requested cell writes. Repeated keys in a batch use the last operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellChange {
    Put { key: CellKey, value: CellValue },
    Delete { key: CellKey },
}

/// An actual change, retaining complete tagged values for index/history work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellValueChange {
    pub key: CellKey,
    pub before: Option<CellValue>,
    pub after: Option<CellValue>,
}

/// A net change with its encoded key, which addresses the `Cell` table, and its
/// path, which addresses the trie.
struct RoutedChange {
    encoded_key: Vec<u8>,
    path: CellPath,
    change: CellValueChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellsUpdate {
    pub root: RootRef<CELL_TRIE_PATH_BYTES>,
    /// Actual changes only, ordered by complete key, spanning the input batch.
    pub changed_cells: Vec<CellValueChange>,
    /// Stored trie rows this batch replaced, which only older roots reach.
    pub superseded: Vec<(Table, Vec<u8>)>,
}

impl<'h, H: HashProvider> Cells<'h, H> {
    pub fn new(hasher: &'h H) -> Self {
        Self {
            trie: CellTrie::new(hasher),
        }
    }

    /// Read a value from the snapshot's `Cell` table. The owned result outlives the transaction.
    /// This is not a proof or an authenticated read against a supplied root.
    pub fn get(&self, tx: &impl ReadTransaction, key: &CellKey) -> Result<Option<CellValue>> {
        CellReader::new(tx).get(key)
    }

    /// Scan one record lazily in name-byte order, including raw reserved names.
    /// Every returned row is owned; only the iterator borrows the transaction.
    pub fn scan_record<'tx, T: ReadTransaction>(
        &self,
        tx: &'tx T,
        record_id: u64,
    ) -> Result<CellScan<T::Cursor<'tx>>> {
        CellReader::new(tx).scan_record(record_id)
    }

    /// Validate a stored root, such as one read from `Roots`. Empty/singleton
    /// roots are checked against the `Cell` table from the same snapshot; a
    /// branch root's own row is validated, not its entire subtree or
    /// correspondence with all `Cell` rows.
    pub fn validate_root(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
    ) -> Result<()> {
        self.trie.validate_root(tx, root)
    }

    /// Apply each key's final value once, skipping no-ops and absent deletions.
    /// Affected commitments are checked against the original `Cell` values even
    /// for no-ops. This is not a whole-state audit; the caller must supply the
    /// matching root. New trie rows are stored under `commit`, the commit this
    /// batch publishes; a second batch at the same commit replaces rows, so
    /// only its root stays valid. Errors require aborting the enclosing
    /// transaction.
    pub fn apply(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        changes: impl IntoIterator<Item = CellChange>,
    ) -> Result<CellsUpdate> {
        let mut changes_by_key = Vec::new();
        for (key, after) in normalize_changes(changes) {
            if let Some(change) = self.check_change(tx, root, key, after)? {
                changes_by_key.push(change);
            }
        }
        // One trie update for the whole batch writes each touched branch once.
        let edits =
            (changes_by_key.iter()).map(|routed| (routed.path, routed.change.after.as_ref()));
        let update = self.trie.set(tx, commit, root, edits)?;
        write_values(tx, &changes_by_key)?;
        Ok(CellsUpdate {
            root: update.root,
            superseded: update.superseded,
            changed_cells: changes_by_key
                .into_iter()
                .map(|routed| routed.change)
                .collect(),
        })
    }

    /// Check the original commitment for one net change. No-ops are checked
    /// but produce no change. Keys are unique within a batch, so the original
    /// root answers as the evolving root would. Returns the change with its
    /// encoded key and path. Any error requires a tx abort.
    fn check_change(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<CELL_TRIE_PATH_BYTES>,
        key: CellKey,
        after: Option<CellValue>,
    ) -> Result<Option<RoutedChange>> {
        let encoded_key = key.encode();
        let path = self.trie.path(&key);
        let before = read_value(tx, &encoded_key)?;
        self.trie.check(tx, root, path, before.as_ref())?;
        Ok((before != after).then_some(RoutedChange {
            encoded_key,
            path,
            change: CellValueChange { key, before, after },
        }))
    }
}

/// Keep the final operation for each key and return the net batch in key order.
fn normalize_changes(
    changes: impl IntoIterator<Item = CellChange>,
) -> Vec<(CellKey, Option<CellValue>)> {
    let mut changes: Vec<_> = changes
        .into_iter()
        .map(|change| match change {
            CellChange::Put { key, value } => (key, Some(value)),
            CellChange::Delete { key } => (key, None),
        })
        .collect();
    // Stable sorting preserves input order for each key. The value must
    // not affect ordering: the final input operation wins.
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    changes.dedup_by(|later, earlier| {
        if later.0 == earlier.0 {
            // dedup_by keeps the earlier entry; move the last value into it.
            earlier.1 = later.1.take();
            true
        } else {
            false
        }
    });
    changes
}

/// Committed cell reads from a caller-owned snapshot, without a hash provider or
/// branch overlay. Read any associated head metadata from the same transaction.
pub struct CellReader<'tx, R: ReadTransaction> {
    tx: &'tx R,
}

impl<'tx, R: ReadTransaction> CellReader<'tx, R> {
    pub fn new(tx: &'tx R) -> Self {
        Self { tx }
    }

    pub fn get(&self, key: &CellKey) -> Result<Option<CellValue>> {
        read_value(self.tx, &key.encode())
    }

    /// Scan one record in name-byte order. Rows are owned; the iterator borrows
    /// the snapshot and preserves raw reserved-record names.
    pub fn scan_record(&self, record_id: u64) -> Result<CellScan<R::Cursor<'tx>>> {
        Ok(CellScan {
            scan: scan_prefix(self.tx, tables::CELL, record_id.to_be_bytes().to_vec())?,
            done: false,
        })
    }
}

fn read_value(tx: &impl ReadTransaction, key: &[u8]) -> Result<Option<CellValue>> {
    tx.get(tables::CELL, key)?
        .map(CellValue::parse)
        .transpose()
        .map_err(Into::into)
}

/// Write each change's final value under its encoded key, or delete the row.
fn write_values(tx: &mut impl WriteTransaction, changes_by_key: &[RoutedChange]) -> Result<()> {
    for routed in changes_by_key {
        let key = &routed.encoded_key;
        match &routed.change.after {
            Some(value) => tx.put(tables::CELL, key, value.encoded_bytes())?,
            None => {
                tx.delete(tables::CELL, key)?;
            }
        }
    }
    Ok(())
}

/// Lazy record scan. Storage or decoding errors are returned once and terminate
/// iteration. Missing records produce an empty iterator.
pub struct CellScan<C> {
    scan: Scan<C>,
    done: bool,
}

impl<C: ReadCursor> Iterator for CellScan<C> {
    type Item = Result<(CellKey, CellValue)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = self
            .scan
            .next()?
            .map_err(Into::into)
            .and_then(|(key, value)| Ok((CellKey::decode(&key)?, CellValue::parse(value)?)));
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}

impl<C: ReadCursor> FusedIterator for CellScan<C> {}
