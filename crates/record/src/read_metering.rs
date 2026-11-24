//! Metered cell access for branch, live and historical record reads.
//! Callers select and validate the source and supply the appropriate pricing
//! schedule. Record identity, projections and assembly belong to `read`.

use golemdb_branch::CellRead;
use golemdb_cells::{CellKey, CellName, CellReader, CellScan, CellValue, reserved, tables};
use golemdb_history::ReadOperation;
use golemdb_metering::{ReadMeter, ReadStep};
use golemdb_storage::ReadTransaction;

use crate::{RecordCells, RecordError, RecordMeta, Result, cell_size};

/// D4 size: record ID + name + value tag + bytes32 key = 8 + 4 + 1 + 32 = 45.
const KEY_CELL_BYTES: u64 =
    (size_of::<u64>() + reserved::KEY.as_bytes().len() + size_of::<u8>() + size_of::<[u8; 32]>())
        as u64;

/// D4 size: record ID + name + value tag + four u64 counters = 8 + 5 + 1 + 32 = 46.
const META_CELL_BYTES: u64 =
    (size_of::<u64>() + reserved::META.as_bytes().len() + size_of::<u8>() + size_of::<[u64; 4]>())
        as u64;

/// Exactly one selected source, with record-owned charging around its reads.
/// Branch readers stay within their guard; committed readers select live or
/// historical data. One caller-owned meter tracks the complete operation.
pub enum MeteredCellFetcher<'a, 'tx, R: ReadTransaction> {
    Branch(&'a CellRead<'tx, R>),
    Committed(CellReader<'tx, R>),
}

impl<R: ReadTransaction> MeteredCellFetcher<'_, '_, R> {
    /// Inspect selected bytes after paying for the lookup and any history work.
    /// Internal bindings and identity/meta reads need no returned-byte charge
    /// here. Use [`returned`] inside `inspect` for returned projected cells.
    pub fn with_value<T>(
        &self,
        key: &CellKey,
        role: LookupRole,
        meter: &mut ReadMeter<'_>,
        inspect: impl FnOnce(&mut ReadMeter<'_>, Option<&[u8]>) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Branch(reader) => {
                meter.charge(match role {
                    LookupRole::Binding => ReadStep::KeyResolve,
                    LookupRole::Cell => ReadStep::CellRead,
                })?;
                reader.with_encoded_value(key, |raw| inspect(meter, raw))?
            }
            Self::Committed(reader) => reader.with_encoded_value_with_callbacks(
                key,
                meter,
                |meter, work| charge(meter, role, work),
                inspect,
            ),
        }
    }

    /// Read an internal binding or identity/meta cell; returned bytes are charged
    /// separately by projection handling or full-record prepayment.
    pub fn get(
        &self,
        key: &CellKey,
        role: LookupRole,
        meter: &mut ReadMeter<'_>,
    ) -> Result<Option<CellValue>> {
        self.with_value(key, role, meter, |_, raw| {
            raw.map(|raw| CellValue::parse(raw.to_vec()).map_err(golemdb_cells::CellError::from))
                .transpose()
                .map_err(Into::into)
        })
    }

    /// Full user records share byte prepayment, but historical traversal is
    /// charged incrementally because metadata does not count absent candidates.
    pub fn user_record_cells(
        &self,
        id: u64,
        meta: &RecordMeta,
        meter: &mut ReadMeter<'_>,
    ) -> Result<RecordCells> {
        match self {
            Self::Branch(reader) => {
                read_live_user_cells(meta, meter, || reader.scan_user_cells(id))
            }
            Self::Committed(reader) if !reader.uses_history() => {
                read_live_user_cells(meta, meter, || reader.scan_user_cells(id))
            }
            Self::Committed(reader) => {
                let mut prepaid = RecordPrepayment::new(meta, meter)?;
                let mut scan = reader.scan_user_cells(id)?;
                let mut values = RecordCells::new();
                while let Some((key, value)) = next_cell(&mut scan, meter, Some(&mut prepaid))? {
                    values.insert(key.name().into(), value);
                }
                prepaid.finish()?;
                Ok(values)
            }
        }
    }

    /// Reserved records pay for traversal and returned bytes incrementally.
    pub fn reserved_record_cells(&self, id: u64, meter: &mut ReadMeter<'_>) -> Result<RecordCells> {
        let mut values = RecordCells::new();
        match self {
            Self::Branch(reader) => {
                reader.visit_record(
                    id,
                    meter,
                    |meter| meter.charge(ReadStep::CellScan).map_err(RecordError::from),
                    |meter, address, raw| {
                        let name = address.name().into();
                        let value = returned(&name, raw, meter)?;
                        values.insert(name, value);
                        Ok(())
                    },
                )?;
            }
            Self::Committed(reader) => {
                let mut scan = reader.scan_record(id)?;
                while let Some((key, value)) = next_cell(&mut scan, meter, None)? {
                    values.insert(key.name().into(), value);
                }
            }
        }
        Ok(values)
    }
}

/// Prepay the complete live scan before opening it; consume only the known count.
fn read_live_user_cells<I, E>(
    meta: &RecordMeta,
    meter: &mut ReadMeter<'_>,
    open_scan: impl FnOnce() -> std::result::Result<I, E>,
) -> Result<RecordCells>
where
    I: Iterator<Item = std::result::Result<(CellKey, CellValue), E>>,
    RecordError: From<E>,
{
    let returned_bytes = meta
        .cell_bytes
        .checked_add(KEY_CELL_BYTES + META_CELL_BYTES)
        .ok_or(RecordError::AccountingOverflow)?;
    meter.charge(ReadStep::FullRecordScan {
        cells: meta.cells,
        returned_bytes,
    })?;
    let mut scan = open_scan()?;
    let mut values = RecordCells::new();
    let mut bytes = 0u64;
    for _ in 0..meta.cells {
        let (address, value) = scan
            .next()
            .transpose()?
            .ok_or(RecordError::CorruptState("#meta exceeds live cell count"))?;
        let name = address.name().into();
        bytes = bytes
            .checked_add(cell_size(&name, &value)?)
            .ok_or(RecordError::AccountingOverflow)?;
        values.insert(name, value);
    }
    if bytes != meta.cell_bytes {
        return Err(RecordError::CorruptState(
            "#meta does not match returned bytes",
        ));
    }
    Ok(values)
}

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

/// Charge D4 bytes before parsing/copying one returned projected/reserved cell.
pub fn returned(name: &CellName, raw: &[u8], meter: &mut ReadMeter<'_>) -> Result<CellValue> {
    let bytes = 8u64
        .checked_add(name.as_bytes().len() as u64)
        .and_then(|n| n.checked_add(raw.len() as u64))
        .ok_or(RecordError::AccountingOverflow)?;
    meter.charge(ReadStep::ReturnedBytes(bytes))?;
    CellValue::parse(raw.to_vec()).map_err(|error| golemdb_cells::CellError::from(error).into())
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
            .checked_add(KEY_CELL_BYTES + META_CELL_BYTES)
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

/// Advance a committed scan, charging all work including absent historical candidates.
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
