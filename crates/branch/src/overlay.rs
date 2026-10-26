use std::collections::BTreeMap;

use golemdb_cells::{CellKey, CellValue, tables};
use golemdb_storage::ReadTransaction;

use crate::{
    CellScan, Result,
    journal::{Journal, WriteMark},
    record_scan::{CellLocation, RecordScan, ScanIndex},
};

pub(crate) type Entries = BTreeMap<CellKey, Option<CellValue>>;

/// Cell-only working state over one committed origin snapshot.
///
/// Missing entries fall through to the origin; `None` entries are tombstones.
/// Mutations are available only inside [`Self::write`], so one record operation
/// can update multiple cells atomically. No storage transaction is retained;
/// the caller must supply the same origin snapshot for every read and write.
#[derive(Default)]
pub(crate) struct CellOverlay {
    entries: Entries,
    journal: Journal,
    scan_index: ScanIndex,
}

impl CellOverlay {
    pub(crate) fn version(&self) -> u64 {
        self.journal.version()
    }

    /// An empty overlay with its first frame already open.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(
        &self,
        origin: &impl ReadTransaction,
        key: &CellKey,
    ) -> Result<Option<CellValue>> {
        match self.entries.get(key) {
            Some(value) => Ok(value.clone()),
            None => origin
                .get(tables::CELL, &key.encode())?
                .map(CellValue::parse)
                .transpose()
                .map_err(Into::into),
        }
    }

    /// Merge origin and overlay cells matching an encoded key prefix.
    /// Tombstones hide origin rows. Raw reserved names are preserved.
    pub(crate) fn scan_prefix<'a, R: ReadTransaction>(
        &'a self,
        origin: &'a R,
        prefix: &[u8],
    ) -> Result<CellScan<'a, R::Cursor<'a>>> {
        CellScan::new(origin, &self.entries, prefix)
    }

    /// Run one atomic group of cell operations. Success retains its writes;
    /// an error or unwinding panic restores the preceding overlay exactly.
    /// The caller's error type is preserved. No checkpoint is consumed.
    ///
    /// After rollback, `put` or `delete` automatically reactivates the retained
    /// frame, even for no-op writes. Failed groups restore both the cells and
    /// that frame's previous rollback status; read-only groups leave it alone.
    pub(crate) fn write<R: ReadTransaction, T, E>(
        &mut self,
        origin: &R,
        operation: impl FnOnce(&mut CellWrite<'_, R>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let mark = self.journal.mark();
        let mut cell_writer = CellWrite {
            overlay: self,
            origin,
            mark,
            completed: false,
        };
        let result = operation(&mut cell_writer)?;
        cell_writer.completed = true;
        Ok(result)
    }

    /// Close the current frame and open an empty one. Retains all undo data.
    /// Repeated checkpoints retain distinct empty frames. Amortized O(1).
    pub(crate) fn checkpoint(&mut self) {
        self.journal.checkpoint();
    }

    /// Restore the current frame's starting state without reading storage,
    /// retaining its marker for subsequent writes. Another rollback without
    /// intervening writes or a checkpoint steps back to the preceding frame.
    /// Fresh empty frames are not skipped. Once exhausted, returns
    /// `NoFrameToRollback`; a write or checkpoint can reactivate the last marker.
    /// Work is proportional to the journal entries undone (ordered-map updates
    /// add a logarithmic factor); surviving operations are never replayed.
    pub(crate) fn rollback(&mut self) -> Result<()> {
        self.journal
            .rollback(&mut self.entries, &mut self.scan_index)
    }

    /// Final staged values in cell-key order, with at most one change per key.
    /// This does not read the origin or discard net-zero changes: `Cells::apply`
    /// performs that comparison at seal and returns the actual before/after diff.
    /// Iteration neither consumes the overlay nor releases its undo journal.
    pub(crate) fn changes(&self) -> impl Iterator<Item = golemdb_cells::CellChange> + '_ {
        self.entries.iter().map(|(key, value)| match value {
            Some(value) => golemdb_cells::CellChange::Put {
                key: key.clone(),
                value: value.clone(),
            },
            None => golemdb_cells::CellChange::Delete { key: key.clone() },
        })
    }
}

/// Read-only cell access within a validated [`crate::Branches::read`] callback.
pub struct CellRead<'a, R: ReadTransaction> {
    overlay: &'a CellOverlay,
    origin: &'a R,
}

impl<'a, R: ReadTransaction> CellRead<'a, R> {
    pub(crate) fn new(overlay: &'a CellOverlay, origin: &'a R) -> Self {
        Self { overlay, origin }
    }

    pub fn get(&self, key: &CellKey) -> Result<Option<CellValue>> {
        self.overlay.get(self.origin, key)
    }

    /// Check one attribute term at this branch's committed origin. Staged cell
    /// changes do not affect this lookup. No posting list is materialized.
    pub fn origin_term_exists(
        &self,
        name: golemdb_cells::CellNameRef<'_>,
        value: &CellValue,
    ) -> Result<bool> {
        let term = golemdb_index::IndexTerm::from_cell_name(name, value.as_view())?;
        Ok(golemdb_index::term_exists(self.origin, &term)?)
    }

    /// Read a key's committed position without changing the overlay or its scan
    /// indexes. Examines at most two origin rows, even for an overlay-only key.
    /// This is structural information, not a mutation decision. If passed to
    /// `CellWrite::delete_at`, use it under this same branch-operation guard.
    pub fn locate_cell(&self, key: &CellKey) -> Result<CellLocation> {
        CellLocation::read(self.origin, key)
    }

    /// Iterate this record's live user cells by merging origin and overlay.
    /// Borrows the scan indexes read-only; all maintenance belongs to the writer.
    pub fn scan_user_cells(&self, record: u64) -> Result<RecordScan<'_, R>> {
        RecordScan::new(
            self.origin,
            &self.overlay.entries,
            &self.overlay.scan_index,
            record,
        )
    }

    /// Scan cells in encoded key order, including staged writes and excluding
    /// tombstones. An empty prefix matches all cells. The prefix is copied and
    /// may end anywhere in the encoded key, including inside its record ID.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<CellScan<'_, R::Cursor<'_>>> {
        self.overlay.scan_prefix(self.origin, prefix)
    }
}

/// Cell access within one [`crate::Branches::write`] callback.
///
/// Reads include earlier writes in this callback. Values and keys are already
/// structurally validated by their types; record admission rules and deployment
/// limits belong to the record layer. This view cannot checkpoint or commit.
/// Writes are retained only when the callback returns `Ok`; `Err` or an escaping
/// unwinding panic restores the pre-call state. See [`crate::Branches::write`]
/// for the complete atomicity and panic contract.
pub struct CellWrite<'a, R: ReadTransaction> {
    overlay: &'a mut CellOverlay,
    origin: &'a R,
    mark: WriteMark,
    completed: bool,
}

impl<R: ReadTransaction> CellWrite<'_, R> {
    /// Borrow the current working state for shared read logic. The borrow
    /// prevents mutations until this read view is no longer used.
    pub fn as_read(&self) -> CellRead<'_, R> {
        CellRead::new(self.overlay, self.origin)
    }

    pub fn get(&self, key: &CellKey) -> Result<Option<CellValue>> {
        self.overlay.get(self.origin, key)
    }

    /// Scan cells in encoded key order, including staged writes and excluding
    /// tombstones. An empty prefix matches all cells. The prefix is copied and
    /// may end anywhere in the encoded key, including inside its record ID.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<CellScan<'_, R::Cursor<'_>>> {
        self.overlay.scan_prefix(self.origin, prefix)
    }

    pub fn put(&mut self, key: CellKey, value: CellValue) {
        self.set(key, Some(value));
    }

    /// Stage an explicit tombstone, including when the cell is already absent.
    /// For user cells that must support `CellRead::scan_user_cells`, obtain a
    /// location and use `Self::delete_at` instead; this method records no origin
    /// position, so that bounded scan rejects its user-cell tombstones.
    pub fn delete(&mut self, key: CellKey) {
        self.set(key, None);
    }

    /// Stage a tombstone at an observed location, updating the scan indexes and
    /// undo journal together. Performs no storage reads: the location supplies
    /// the committed position needed to maintain hidden ranges. It must come
    /// from this branch's reader under the same branch-operation guard.
    pub fn delete_at(&mut self, location: CellLocation) {
        self.overlay.journal.record(
            &mut self.overlay.entries,
            &mut self.overlay.scan_index,
            location.key,
            None,
            Some(location.origin),
        );
    }

    fn set(&mut self, key: CellKey, value: Option<CellValue>) {
        self.overlay.journal.record(
            &mut self.overlay.entries,
            &mut self.overlay.scan_index,
            key,
            value,
            None,
        );
    }
}

impl<R: ReadTransaction> Drop for CellWrite<'_, R> {
    fn drop(&mut self) {
        if !self.completed {
            self.overlay.journal.rewind(
                &mut self.overlay.entries,
                &mut self.overlay.scan_index,
                self.mark,
            );
        }
    }
}
