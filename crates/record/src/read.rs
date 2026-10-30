//! Metered record materialization. Branch only supplies a stable cell view;
//! Record owns admission, charge boundaries and the returned values.
use std::collections::BTreeSet;

use golemdb_branch::CellRead;
use golemdb_cells::{CellKey, CellName, CellValue, reserved, system};
use golemdb_metering::{ReadMeter, ReadStep};
use golemdb_storage::ReadTransaction;

use crate::{Record, RecordCells, RecordError, RecordKey, Result, cell_size, meta, state};

/// D4 size: record ID + name + value tag + bytes32 key = 8 + 4 + 1 + 32 = 45.
const KEY_CELL_BYTES: u64 =
    (size_of::<u64>() + reserved::KEY.as_bytes().len() + size_of::<u8>() + size_of::<[u8; 32]>())
        as u64;

/// D4 size: record ID + name + value tag + four u64 counters = 8 + 5 + 1 + 32 = 46.
const META_CELL_BYTES: u64 =
    (size_of::<u64>() + reserved::META.as_bytes().len() + size_of::<u8>() + size_of::<[u64; 4]>())
        as u64;

pub(crate) fn get<R: ReadTransaction>(
    reader: &CellRead<'_, R>,
    key: RecordKey,
    projection: Option<&[CellName]>,
    meter: &mut ReadMeter<'_>,
) -> Result<Record> {
    meter.charge(ReadStep::AdmissionBase)?;
    // Charge every supplied entry before normalization, including duplicates.
    // Read names may be raw/binary/reserved; do not apply write-name grammar.
    let names = projection
        .map(|names| -> Result<_> {
            let mut unique = BTreeSet::new();
            for name in names {
                meter.charge(ReadStep::AdmissionEntry {
                    bytes: name.as_bytes().len() as u64,
                })?;
                unique.insert(name);
            }
            Ok(unique)
        })
        .transpose()?;
    // Target validation has already selected this view and its pricing commit.
    // Admission-before-snapshot opening is deliberately deferred.
    meter.charge(ReadStep::ReadBase)?;
    meter.charge(ReadStep::KeyResolve)?;
    let id =
        state::record_id(key, reader.get(&state::binding(key))?)?.ok_or(RecordError::NotFound)?;
    let mut values = RecordCells::new();
    match names {
        Some(names) => {
            for name in names {
                meter.charge(ReadStep::CellRead)?;
                let value = reader
                    .with_encoded_value(&CellKey::new(id, name.clone()), |raw| {
                        raw.map(|raw| returned(name, raw, meter)).transpose()
                    })??;
                if let Some(value) = value {
                    values.insert(name.clone(), value);
                }
            }
        }
        None if id >= system::FIRST_USER_RECORD_ID => {
            meter.charge(ReadStep::CellRead)?;
            let identity = reader.get(&CellKey::new(id, reserved::KEY))?;
            state::check_identity(key, identity.clone())?;
            let identity = identity.ok_or(RecordError::CorruptState("record has no #key"))?;
            meter.charge(ReadStep::CellRead)?;
            let metadata = reader
                .get(&CellKey::new(id, reserved::META))?
                .ok_or(RecordError::CorruptState("user record has no #meta"))?;
            let meta = meta::required(Some(&metadata))?;
            let returned_bytes = meta
                .cell_bytes
                .checked_add(KEY_CELL_BYTES + META_CELL_BYTES)
                .ok_or(RecordError::AccountingOverflow)?;
            meter.charge(ReadStep::FullRecordScan {
                cells: meta.cells,
                returned_bytes,
            })?;
            let mut scan = reader.scan_user_cells(id)?;
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
            values.insert(reserved::KEY.into(), identity);
            values.insert(reserved::META.into(), metadata);
        }
        None => {
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
    }
    // Projections check identity only if they requested it; existence checks
    // resolve the binding alone and never perform an uncharged #key lookup.
    if let Some(identity) = values.get(reserved::KEY.as_bytes()) {
        state::check_identity(key, Some(identity.clone()))?;
    }
    Ok(Record { key, cells: values })
}

fn returned(name: &CellName, raw: &[u8], meter: &mut ReadMeter<'_>) -> Result<CellValue> {
    let bytes = 8u64
        .checked_add(name.as_bytes().len() as u64)
        .and_then(|n| n.checked_add(raw.len() as u64))
        .ok_or(RecordError::AccountingOverflow)?;
    meter.charge(ReadStep::ReturnedBytes(bytes))?;
    CellValue::parse(raw.to_vec()).map_err(|error| golemdb_cells::CellError::from(error).into())
}
