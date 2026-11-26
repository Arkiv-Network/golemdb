use golemdb_history::{HistoryError, ReadOperation, ValueSource};
use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{
    CELL_TRIE_PATH_BYTES, CellKey, CellScan, CellValue, CellValueRef, Result, USER_CELL_SCAN_START,
    cell_trie::CellTrie, tables,
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
    ) -> Result<CellScan<'tx, T>> {
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
        let mut changes = (normalize_changes(changes).into_iter())
            .map(|(key, after)| {
                let before = read_value(tx, &key.encode())?;
                Ok(CellValueChange { key, before, after })
            })
            .collect::<Result<Vec<_>>>()?;
        let update = self.trie.set(tx, commit, root, &changes)?;
        // No-ops were checked by the trie update but write nothing.
        changes.retain(|change| change.before != change.after);
        write_values(tx, &changes)?;
        Ok(CellsUpdate {
            root: update.root,
            superseded: update.superseded,
            changed_cells: changes,
        })
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
    target: Option<u64>,
}

impl<'tx, R: ReadTransaction> CellReader<'tx, R> {
    pub fn new(tx: &'tx R) -> Self {
        Self { tx, target: None }
    }

    /// Read state after `commit` using this transaction's history and live cells.
    /// The caller must validate the target against head and retention coverage
    /// in the same snapshot. Missing history alone cannot establish coverage.
    /// These are flat reads, not proofs against a historical root.
    pub fn at(tx: &'tx R, commit: u64) -> Self {
        Self {
            tx,
            target: Some(commit),
        }
    }

    pub fn get(&self, key: &CellKey) -> Result<Option<CellValue>> {
        self.with_encoded_value(key, |bytes| {
            bytes
                .map(|bytes| CellValueRef::parse(bytes).map(CellValue::from))
                .transpose()
                .map_err(Into::into)
        })?
    }

    /// Inspect selected borrowed bytes without work callbacks. See
    /// [`Self::with_encoded_value_with_callbacks`] for upfront work authorization.
    pub fn with_encoded_value<T>(
        &self,
        key: &CellKey,
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> Result<T> {
        self.with_encoded_value_with_callbacks(
            key,
            &mut (),
            |_, _| Ok(()),
            |_, bytes| Ok(inspect(bytes)),
        )
    }

    /// Inspect borrowed bytes before domain parsing/copying. `before` authorizes
    /// each lookup and commit-set package; a callback returning `Ok(())` permits
    /// internal reads without charging. Empty pre-images mean absence; missing
    /// selected changesets fail and empty live rows remain visible to validation.
    pub fn with_encoded_value_with_callbacks<C, T, E>(
        &self,
        key: &CellKey,
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        inspect: impl FnOnce(&mut C, Option<&[u8]>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<crate::CellError> + From<HistoryError>,
    {
        let encoded = key.encode();
        let key = encoded.as_slice();
        let Some(target) = self.target else {
            before(context, ReadOperation::Lookup(tables::CELL))?;
            return self
                .tx
                .with_value(tables::CELL, key, |raw| inspect(context, raw))
                .map_err(crate::CellError::from)?;
        };
        tables::HISTORY.with_resolved_with_callbacks(
            self.tx,
            key,
            target,
            &mut (context, &mut before),
            |(context, before), work| before(context, work),
            |(context, before), source| match source {
                ValueSource::Preimage(bytes) => inspect(context, bytes),
                ValueSource::Live => {
                    before(context, ReadOperation::Lookup(tables::CELL))?;
                    self.tx
                        .with_value(tables::CELL, key, |raw| inspect(context, raw))
                        .map_err(crate::CellError::from)?
                }
            },
        )
    }

    /// Scan one record in name-byte order. Rows are owned; the iterator borrows
    /// the snapshot and preserves raw reserved-record names.
    /// At a historical target, merge live and historical names and emit only
    /// cells present at that target. The caller validates target availability.
    pub fn scan_record(&self, record_id: u64) -> Result<CellScan<'tx, R>> {
        CellScan::new(self.tx, record_id, self.target, b"")
    }

    /// Scan the same name range as branch user-cell scans: names at or after
    /// `$`, excluding `#key` and `#meta`, which callers can point-read separately.
    /// This assumes an admitted user record; it does not validate name grammar.
    /// Use `scan_record` for unrestricted reserved/binary names.
    pub fn scan_user_cells(&self, record_id: u64) -> Result<CellScan<'tx, R>> {
        CellScan::new(self.tx, record_id, self.target, USER_CELL_SCAN_START)
    }
}

fn read_value(tx: &impl ReadTransaction, key: &[u8]) -> Result<Option<CellValue>> {
    tx.get(tables::CELL, key)?
        .map(CellValue::parse)
        .transpose()
        .map_err(Into::into)
}

/// Write each change's final value under its encoded key, or delete the row.
fn write_values(tx: &mut impl WriteTransaction, changes: &[CellValueChange]) -> Result<()> {
    for change in changes {
        let key = change.key.encode();
        match &change.after {
            Some(value) => tx.put(tables::CELL, &key, value.encoded_bytes())?,
            None => {
                tx.delete(tables::CELL, &key)?;
            }
        }
    }
    Ok(())
}
