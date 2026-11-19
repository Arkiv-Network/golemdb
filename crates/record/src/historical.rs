//! Engine read-pricing adapters. Callers validate target/coverage and supply the
//! current committed schedule before constructing historical cell readers.
//! These helpers do not enable historical targets on the public CRUD API.

use golemdb_cells::{CellKey, CellName, CellReader, CellScan, CellValue, tables};
use golemdb_history::ReadOperation;
use golemdb_metering::{ReadMeter, ReadStep};
use golemdb_storage::ReadTransaction;

use crate::{RecordError, RecordMeta, Result};

/// The selected binding lookup has its own price; history discovery is ordinary
/// cell work. Never charge both `read.key` and `read.cell` for the selected row.
#[derive(Clone, Copy, Debug)]
pub enum LookupRole {
    Binding,
    Cell,
}

fn charge(meter: &mut ReadMeter<'_>, role: LookupRole, work: ReadOperation) -> Result<()> {
    let step = match work {
        ReadOperation::Lookup(table) => {
            if matches!(role, LookupRole::Binding) && table != tables::CELL_HISTORY {
                ReadStep::KeyResolve
            } else {
                ReadStep::CellRead
            }
        }
        ReadOperation::Advance(_) => ReadStep::CellScan,
        ReadOperation::CommitSet { encoded_bytes } => ReadStep::CommitSet { encoded_bytes },
    };
    Ok(meter.charge(step)?)
}

/// Inspect selected bytes with the lookup and bitmap packages already paid.
/// Internal bindings and fixed-size identity/meta reads need no returned-byte
/// charge here. For returned projected cells use `returned` inside `inspect`.
pub fn with_value<R: ReadTransaction, T>(
    reader: &CellReader<'_, R>,
    key: &CellKey,
    role: LookupRole,
    meter: &mut ReadMeter<'_>,
    inspect: impl FnOnce(&mut ReadMeter<'_>, Option<&[u8]>) -> Result<T>,
) -> Result<T> {
    reader.with_encoded_value_with_callbacks(
        key,
        meter,
        |meter, work| charge(meter, role, work),
        inspect,
    )
}

/// Charge D4 bytes before parsing/copying one returned projected/reserved cell.
pub fn returned(name: &CellName, raw: &[u8], meter: &mut ReadMeter<'_>) -> Result<CellValue> {
    super::read::returned(name, raw, meter)
}

/// Full user-record byte prepayment. Scan work remains incremental. Remaining
/// counters prevent corrupt metadata from authorizing more copies than paid for.
#[derive(Debug)]
pub struct RecordPrepayment {
    cells: u64,
    bytes: u64,
}

impl RecordPrepayment {
    pub fn new(meta: &RecordMeta, meter: &mut ReadMeter<'_>) -> Result<Self> {
        let bytes = meta
            .cell_bytes
            .checked_add(super::read::KEY_CELL_BYTES + super::read::META_CELL_BYTES)
            .ok_or(RecordError::AccountingOverflow)?;
        meter.charge(ReadStep::ReturnedBytes(bytes))?;
        Ok(Self {
            cells: meta.cells,
            bytes: meta.cell_bytes,
        })
    }

    fn consume(&mut self, key: &CellKey, raw: &[u8]) -> Result<()> {
        let bytes = 8u64
            .checked_add(key.name().as_bytes().len() as u64)
            .and_then(|n| n.checked_add(raw.len() as u64))
            .ok_or(RecordError::AccountingOverflow)?;
        let cells = self.cells.checked_sub(1);
        let bytes = self.bytes.checked_sub(bytes);
        match (cells, bytes) {
            (Some(cells), Some(bytes)) => {
                self.cells = cells;
                self.bytes = bytes;
                Ok(())
            }
            _ => Err(RecordError::CorruptState(
                "historical cells exceed #meta prepayment",
            )),
        }
    }

    pub fn finish(self) -> Result<()> {
        if self.cells != 0 || self.bytes != 0 {
            return Err(RecordError::CorruptState(
                "#meta does not match historical cells",
            ));
        }
        Ok(())
    }
}

/// Advance a historical scan, charging all work including absent candidates.
/// Full user-record callers pass their byte prepayment; other scans pay bytes
/// per emitted cell. Errors fuse the scan before any later work can occur.
pub fn next_cell<R: ReadTransaction>(
    scan: &mut CellScan<'_, R>,
    meter: &mut ReadMeter<'_>,
    mut prepaid: Option<&mut RecordPrepayment>,
) -> Result<Option<(CellKey, CellValue)>> {
    scan.next_with(
        meter,
        |meter, work| charge(meter, LookupRole::Cell, work),
        |meter, key, raw| {
            let value = if let Some(prepaid) = &mut prepaid {
                prepaid.consume(&key, raw)?;
                CellValue::parse(raw.to_vec()).map_err(golemdb_cells::CellError::from)?
            } else {
                returned(&key.name().into(), raw, meter)?
            };
            Ok((key, value))
        },
    )
}
