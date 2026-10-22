use golemdb_cells::{CellValue, reserved};
use serde::{Deserialize, Serialize};

use crate::{Effects, Record, RecordError, Result};

/// Exact user-cell counts stored in the record's `#meta` bytes32 field.
/// Byte counts use the same cell and index-term sizes as receipt effects.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecordMeta {
    pub cells: u64,
    pub cell_bytes: u64,
    pub indexed_cells: u64,
    pub index_bytes: u64,
}

impl RecordMeta {
    /// Four big-endian u64 values, in declaration order, as a bytes32 field.
    pub fn encode(self) -> CellValue {
        let mut bytes = [0; 32];
        for (chunk, count) in bytes.chunks_exact_mut(8).zip([
            self.cells,
            self.cell_bytes,
            self.indexed_cells,
            self.index_bytes,
        ]) {
            chunk.copy_from_slice(&count.to_be_bytes());
        }
        CellValue::from_bytes32(bytes)
    }

    pub fn decode(value: &CellValue) -> Result<Self> {
        let bytes = value
            .as_bytes32()
            .filter(|_| !value.is_indexable())
            .ok_or(RecordError::CorruptState("#meta must be a bytes32 field"))?;
        let count = |offset| u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let meta = Self {
            cells: count(0),
            cell_bytes: count(8),
            indexed_cells: count(16),
            index_bytes: count(24),
        };
        meta.validate()?;
        Ok(meta)
    }

    fn validate(self) -> Result<()> {
        if self.indexed_cells > self.cells
            || (self.cells == 0) != (self.cell_bytes == 0)
            || (self.indexed_cells == 0) != (self.index_bytes == 0)
        {
            return Err(RecordError::CorruptState("inconsistent #meta counts"));
        }
        Ok(())
    }

    /// Apply the complete operation's effects, before writing any mutations.
    /// Subtract first so corrupt old counts cannot be masked by additions.
    pub(crate) fn apply(self, effects: &Effects) -> Result<Self> {
        let adjust = |count: u64, removed, added| {
            count
                .checked_sub(removed)
                .ok_or(RecordError::CorruptState("#meta count underflow"))?
                .checked_add(added)
                .ok_or(RecordError::AccountingOverflow)
        };
        let meta = Self {
            cells: adjust(self.cells, effects.cells_deleted, effects.cells_created)?,
            cell_bytes: adjust(
                self.cell_bytes,
                effects.cell_bytes_deleted,
                effects.cell_bytes_written,
            )?,
            indexed_cells: adjust(
                self.indexed_cells,
                effects.index_leaves,
                effects.index_joins,
            )?,
            index_bytes: adjust(
                self.index_bytes,
                effects.index_bytes_deleted,
                effects.index_bytes_written,
            )?,
        };
        meta.validate()?;
        Ok(meta)
    }
}

/// Per-user-record caps fixed at genesis. Zero permits only empty records or,
/// for `max_indexed_cells`, only fields. System and admin records are exempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordLimits {
    pub max_cells: u64,
    pub max_indexed_cells: u64,
}

impl RecordLimits {
    pub(crate) fn check(self, meta: RecordMeta) -> Result<()> {
        for (name, count, cap) in [
            ("#maxRecordCells", meta.cells, self.max_cells),
            (
                "#maxRecordIndexedCells",
                meta.indexed_cells,
                self.max_indexed_cells,
            ),
        ] {
            if count > cap {
                return Err(RecordError::InvalidInput {
                    message: format!("resulting record count {count} exceeds {name} ({cap})"),
                    source: None,
                });
            }
        }
        Ok(())
    }
}

impl Record {
    /// Decode `#meta`, or return None if it is absent or malformed.
    /// Use [`RecordMeta::decode`] to inspect a cell's decoding error.
    pub fn meta(&self) -> Option<RecordMeta> {
        self.cells
            .get(reserved::META.as_bytes())
            .and_then(|value| RecordMeta::decode(value).ok())
    }
}

pub(crate) fn required(value: Option<&CellValue>) -> Result<RecordMeta> {
    RecordMeta::decode(value.ok_or(RecordError::CorruptState("user record has no #meta"))?)
}
