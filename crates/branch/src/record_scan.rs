//! Read-only merging of committed and overlay cells, backed by branch-local
//! indexes. Cell locations describe committed positions; the writer uses them
//! to maintain seekable hidden ranges when it applies deletions. A separate
//! live-key set skips overlay-only tombstones in O(log n).

use crate::{BranchError, Result, overlay::Entries};
use golemdb_cells::{CellKey, CellNameRef, CellValue, tables};
use golemdb_storage::{Entry, ReadCursor, ReadTransaction};
use std::{
    collections::{BTreeMap, BTreeSet},
    iter::{FusedIterator, Peekable},
    ops::Bound,
};

#[derive(Clone)]
pub(crate) enum OriginPosition {
    Absent,
    Present { next: Option<CellKey> },
}

/// Opaque position of a cell key relative to a branch's committed origin.
///
/// Records whether the key exists there and, if present, its immediate committed
/// successor. It does not describe the current overlay value or request a
/// deletion. Reading a location changes no branch state; the caller decides
/// whether to use it with [`crate::CellWrite::delete_at`]. Keep it within the
/// same guarded branch operation that obtained it.
pub struct CellLocation {
    pub(crate) key: CellKey,
    pub(crate) origin: OriginPosition,
}

impl CellLocation {
    pub(crate) fn read(origin: &impl ReadTransaction, key: &CellKey) -> Result<Self> {
        let encoded = key.encode();
        let mut cursor = origin.cursor(tables::CELL, &encoded)?;
        let position = if cursor.next()?.is_some_and(|(found, _)| found == encoded) {
            OriginPosition::Present {
                next: cursor
                    .next()?
                    .map(|(key, _)| CellKey::decode(&key))
                    .transpose()?,
            }
        } else {
            OriginPosition::Absent
        };
        Ok(Self {
            key: key.clone(),
            origin: position,
        })
    }
}

/// Retained overlay bookkeeping, updated by the writer and restored by the undo
/// journal. A `RecordScan` only borrows it; iteration never changes these indexes.
#[derive(Default)]
pub(crate) struct ScanIndex {
    live: BTreeSet<CellKey>,
    deleted: BTreeMap<CellKey, OriginPosition>,
    // Disjoint, nonadjacent [first hidden key, next visible origin key) runs.
    hidden: BTreeMap<CellKey, Option<CellKey>>,
    unlocated: BTreeSet<CellKey>,
}

impl ScanIndex {
    pub(crate) fn position(&self, key: &CellKey) -> Option<OriginPosition> {
        self.deleted.get(key).cloned()
    }

    pub(crate) fn update(
        &mut self,
        key: &CellKey,
        value: Option<&Option<CellValue>>,
        origin: Option<OriginPosition>,
    ) {
        self.live.remove(key);
        self.unlocated.remove(key);
        if let Some(OriginPosition::Present { next }) = self.deleted.remove(key) {
            self.remove_hidden(key, next);
        }
        match value {
            Some(Some(_)) => {
                self.live.insert(key.clone());
            }
            Some(None) => match origin {
                Some(position) => {
                    if let OriginPosition::Present { next } = &position {
                        self.add_hidden(key.clone(), next.clone());
                    }
                    self.deleted.insert(key.clone(), position);
                }
                None => {
                    self.unlocated.insert(key.clone());
                }
            },
            None => {}
        }
    }

    fn add_hidden(&mut self, mut first: CellKey, mut end: Option<CellKey>) {
        if let Some((left, left_end)) = self.hidden.range(..&first).next_back()
            && left_end.as_ref() == Some(&first)
        {
            let left = left.clone();
            self.hidden.remove(&left);
            first = left;
        }
        if let Some(right) = &end
            && let Some(right_end) = self.hidden.remove(right)
        {
            end = right_end;
        }
        self.hidden.insert(first, end);
    }

    fn remove_hidden(&mut self, key: &CellKey, next: Option<CellKey>) {
        let Some((first, end)) = self
            .hidden
            .range(..=key)
            .next_back()
            .map(|(first, end)| (first.clone(), end.clone()))
        else {
            return;
        };
        self.hidden.remove(&first);
        if &first < key {
            self.hidden.insert(first, Some(key.clone()));
        }
        if next != end
            && let Some(next) = next
        {
            self.hidden.insert(next, end);
        }
    }

    fn hidden_end(&self, key: &CellKey) -> Option<Option<&CellKey>> {
        self.hidden.range(..=key).next_back().and_then(|(_, end)| {
            end.as_ref()
                .is_none_or(|end| key < end)
                .then_some(end.as_ref())
        })
    }
}

/// A live-user-cell scan whose `next()` examines at most two origin rows and
/// one live overlay entry. Hidden origin runs use one seek; overlay tombstones
/// are absent from the live-key index. No rows are fetched at construction.
/// User-cell tombstones must have been written with `CellWrite::delete_at` so
/// their committed positions are known. The iterator itself never marks deletes.
pub struct RecordScan<'a, R: ReadTransaction> {
    origin: &'a R,
    cursor: R::Cursor<'a>,
    pending: Option<Entry>,
    origin_done: bool,
    overlay: Peekable<std::collections::btree_set::Range<'a, CellKey>>,
    entries: &'a Entries,
    index: &'a ScanIndex,
    record: u64,
    done: bool,
}

impl<'a, R: ReadTransaction> RecordScan<'a, R> {
    pub(crate) fn new(
        origin: &'a R,
        entries: &'a Entries,
        index: &'a ScanIndex,
        record: u64,
    ) -> Result<Self> {
        // All admitted user names begin with '$' or an ASCII letter. Engine
        // cells #key/#meta are point-read separately and never rescanned here.
        let start = CellKey::new(record, CellNameRef::raw(b"$"));
        let end = record
            .checked_add(1)
            .map(|id| Bound::Excluded(CellKey::new(id, CellNameRef::raw(b""))))
            .unwrap_or(Bound::Unbounded);
        let bounds = (Bound::Included(start.clone()), end);
        if index.unlocated.range(bounds.clone()).next().is_some() {
            return Err(BranchError::InvalidScanState(
                "user-cell tombstone lacks origin location",
            ));
        }
        Ok(Self {
            origin,
            cursor: origin.cursor(tables::CELL, &start.encode())?,
            pending: None,
            origin_done: false,
            overlay: index.live.range(bounds).peekable(),
            entries,
            index,
            record,
            done: false,
        })
    }

    fn fill_origin(&mut self) -> Result<()> {
        if self.pending.is_some() || self.origin_done {
            return Ok(());
        }
        let mut row = self.cursor.next()?;
        if let Some((key, _)) = &row {
            let key = CellKey::decode(key)?;
            if let Some(end) = self.index.hidden_end(&key) {
                row = match end {
                    Some(end) => {
                        self.cursor = self.origin.cursor(tables::CELL, &end.encode())?;
                        self.cursor.next()?
                    }
                    None => None,
                };
            }
        }
        self.pending = row.filter(|(key, _)| key.starts_with(&self.record.to_be_bytes()));
        self.origin_done = self.pending.is_none();
        Ok(())
    }

    fn advance(&mut self) -> Result<Option<(CellKey, CellValue)>> {
        self.fill_origin()?;
        let overlay_first = match (&self.pending, self.overlay.peek()) {
            (Some((origin, _)), Some(overlay)) => overlay.encode() <= *origin,
            (None, Some(_)) => true,
            _ => false,
        };
        if overlay_first {
            let key = self
                .overlay
                .next()
                .ok_or(BranchError::InvalidScanState("live key disappeared"))?;
            if self
                .pending
                .as_ref()
                .is_some_and(|(origin, _)| *origin == key.encode())
            {
                self.pending = None;
            }
            let value = self
                .entries
                .get(key)
                .and_then(Option::as_ref)
                .ok_or(BranchError::InvalidScanState("live key has no value"))?;
            return Ok(Some((key.clone(), value.clone())));
        }
        self.pending
            .take()
            .map(|(key, value)| Ok((CellKey::decode(&key)?, CellValue::parse(value)?)))
            .transpose()
    }
}

impl<R: ReadTransaction> Iterator for RecordScan<'_, R> {
    type Item = Result<(CellKey, CellValue)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.advance() {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}
impl<R: ReadTransaction> FusedIterator for RecordScan<'_, R> {}
