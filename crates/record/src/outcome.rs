use golemdb_branch::CommitId;
use golemdb_cells::{CellName, CellValue};

use crate::{RecordError, Result};

/// Execution context captured by the owning layer, before admission. The API
/// maps this to its public receipt without opening another snapshot.
#[derive(Debug)]
#[must_use]
pub struct Outcome<T, E> {
    pub result: std::result::Result<T, E>,
    pub priced_at: Option<CommitId>,
    pub effects: Effects,
}

impl<T, E> Outcome<T, E> {
    pub fn new(
        result: std::result::Result<T, E>,
        priced_at: Option<CommitId>,
        effects: Effects,
    ) -> Self {
        let effects = if result.is_ok() {
            effects
        } else {
            Effects::default()
        };
        Self {
            result,
            priced_at,
            effects,
        }
    }
}

/// Logical user-cell effects, independent of physical writes and pricing.
/// Shared with metadata maintenance; per-record system cells are excluded.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Effects {
    pub cells_created: u64,
    pub cells_updated: u64,
    pub cells_deleted: u64,
    pub index_joins: u64,
    pub index_leaves: u64,
    pub cell_bytes_written: u64,
    pub cell_bytes_deleted: u64,
    pub index_bytes_written: u64,
    pub index_bytes_deleted: u64,
}

pub fn cell_size(name: &CellName, value: &CellValue) -> Result<u64> {
    size(&[8, name.as_bytes().len(), value.encoded_bytes().len()])
}

pub fn index_size(name: &CellName, value: &CellValue) -> Result<u64> {
    size(&[name.as_bytes().len(), 2, value.value().len()])
}

fn size(parts: &[usize]) -> Result<u64> {
    parts.iter().try_fold(0u64, |total, &part| {
        let part = u64::try_from(part).map_err(|_| RecordError::AccountingOverflow)?;
        total
            .checked_add(part)
            .ok_or(RecordError::AccountingOverflow)
    })
}

fn add(total: &mut u64, amount: u64) -> Result<()> {
    *total = total
        .checked_add(amount)
        .ok_or(RecordError::AccountingOverflow)?;
    Ok(())
}

impl Effects {
    pub(crate) fn change(
        &mut self,
        name: &CellName,
        before: Option<&CellValue>,
        after: Option<&CellValue>,
    ) -> Result<()> {
        if before == after || matches!(name.as_bytes().first(), Some(b'#' | b'@')) {
            return Ok(());
        }
        match (before, after) {
            (None, Some(_)) => add(&mut self.cells_created, 1)?,
            (Some(_), Some(_)) => add(&mut self.cells_updated, 1)?,
            (Some(_), None) => add(&mut self.cells_deleted, 1)?,
            (None, None) => return Ok(()),
        }
        if let Some(value) = before {
            add(&mut self.cell_bytes_deleted, cell_size(name, value)?)?;
            if value.is_indexable() {
                add(&mut self.index_leaves, 1)?;
                add(&mut self.index_bytes_deleted, index_size(name, value)?)?;
            }
        }
        if let Some(value) = after {
            add(&mut self.cell_bytes_written, cell_size(name, value)?)?;
            if value.is_indexable() {
                add(&mut self.index_joins, 1)?;
                add(&mut self.index_bytes_written, index_size(name, value)?)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/outcome.rs"]
mod tests;
