use crate::{
    ImmutableDataStorageError as Error, StorageResult,
    backend::{PayloadRow, span_start},
    storage::Segment,
};
use std::{collections::BTreeMap, ops::Range};

/// Rows retain only their file-span coordinate, not per-commit associations.
pub(crate) struct MemoryStorage {
    span: u64,
    rows: BTreeMap<u32, Vec<(u64, PayloadRow)>>,
}
impl MemoryStorage {
    pub(crate) fn recover(
        &mut self,
        s: &Segment,
        ranges: &BTreeMap<u64, Range<u64>>,
    ) -> StorageResult<()> {
        let end = ranges.last_key_value().map_or(0, |(_, range)| range.end);
        if end > self.end_ordinal(s) {
            return Err(Error::Corrupt("missing published immutable rows"));
        }
        if let Some(rows) = self.rows.get(&s.id) {
            for (span, range) in ranges {
                let first = usize::try_from(range.start).map_err(|_| Error::Overflow)?;
                let end = usize::try_from(range.end).map_err(|_| Error::Overflow)?;
                if rows[first..end].iter().any(|(actual, _)| actual != span) {
                    return Err(Error::Corrupt("published rows belong to another file span"));
                }
            }
        }
        self.truncate(s, end)
    }
    pub(crate) fn new(span: u64) -> Self {
        Self {
            span,
            rows: BTreeMap::new(),
        }
    }

    pub(crate) fn append(&mut self, s: &Segment, commit: u64, row: &[&[u8]]) -> StorageResult<u64> {
        let start = span_start(commit, self.span)?;
        let rows = self.rows.entry(s.id).or_default();
        if rows.last().is_some_and(|(previous, _)| *previous > start) {
            return Err(Error::Invalid("cannot append to an older commit span"));
        }
        let ordinal = rows.len() as u64;
        ordinal.checked_add(1).ok_or(Error::Overflow)?;
        rows.push((start, row.iter().map(|v| v.to_vec()).collect()));
        Ok(ordinal)
    }

    pub(crate) fn get(
        &self,
        s: &Segment,
        ordinal: u64,
        columns: &[usize],
    ) -> StorageResult<PayloadRow> {
        let ordinal = usize::try_from(ordinal).map_err(|_| Error::Overflow)?;
        let (_, row) = self
            .rows
            .get(&s.id)
            .and_then(|rows| rows.get(ordinal))
            .ok_or(Error::NotFound)?;
        Ok(columns.iter().map(|&i| row[i].clone()).collect())
    }

    pub(crate) fn read_range(
        &self,
        s: &Segment,
        commit: u64,
        range: Range<u64>,
        columns: &[usize],
    ) -> StorageResult<Vec<PayloadRow>> {
        let start = span_start(commit, self.span)?;
        let rows = self.rows.get(&s.id).ok_or(Error::NotFound)?;
        let bounds = usize::try_from(range.start).map_err(|_| Error::Overflow)?
            ..usize::try_from(range.end).map_err(|_| Error::Overflow)?;
        let rows = rows
            .get(bounds)
            .ok_or(Error::Invalid("range exceeds physical rows"))?;
        if rows.iter().any(|(span, _)| *span != start) {
            return Err(Error::Invalid("range belongs to another commit span"));
        }
        if columns.is_empty() {
            return Ok(vec![]);
        }
        Ok(rows
            .iter()
            .map(|(_, row)| columns.iter().map(|&i| row[i].clone()).collect())
            .collect())
    }

    pub(crate) fn end_ordinal(&self, s: &Segment) -> u64 {
        self.rows.get(&s.id).map_or(0, |v| v.len() as u64)
    }

    pub(crate) fn truncate(&mut self, s: &Segment, from: u64) -> StorageResult<()> {
        let rows = self.rows.entry(s.id).or_default();
        if from > rows.len() as u64 {
            return Err(Error::Invalid("truncate exceeds physical end"));
        }
        rows.truncate(from as usize);
        Ok(())
    }
}
