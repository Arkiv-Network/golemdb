//! Record admission, identity, projections and assembly over metered cells.
use std::collections::BTreeSet;

use golemdb_cells::{CellKey, CellName, reserved, system};
use golemdb_metering::{ReadMeter, ReadStep};
use golemdb_storage::ReadTransaction;

use crate::{
    Record, RecordCells, RecordError, RecordKey, Result, meta,
    read_metering::{LookupRole, MeteredCellFetcher, returned},
    state,
};

pub(crate) fn get<R: ReadTransaction>(
    reader: &MeteredCellFetcher<'_, '_, R>,
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
    let id = state::record_id(
        key,
        reader.get(&state::binding(key), LookupRole::Binding, meter)?,
    )?
    .ok_or(RecordError::NotFound)?;
    let mut values = RecordCells::new();
    match names {
        Some(names) => {
            for name in names {
                let value = reader.with_value(
                    &CellKey::new(id, name.clone()),
                    LookupRole::Cell,
                    meter,
                    |meter, raw| raw.map(|raw| returned(name, raw, meter)).transpose(),
                )?;
                if let Some(value) = value {
                    values.insert(name.clone(), value);
                }
            }
        }
        None if id >= system::FIRST_USER_RECORD_ID => {
            let identity = reader.get(&CellKey::new(id, reserved::KEY), LookupRole::Cell, meter)?;
            state::check_identity(key, identity.clone())?;
            let identity = identity.ok_or(RecordError::CorruptState("record has no #key"))?;
            let metadata = reader
                .get(&CellKey::new(id, reserved::META), LookupRole::Cell, meter)?
                .ok_or(RecordError::CorruptState("user record has no #meta"))?;
            let meta = meta::required(Some(&metadata))?;
            values = reader.user_record_cells(id, &meta, meter)?;
            values.insert(reserved::KEY.into(), identity);
            values.insert(reserved::META.into(), metadata);
        }
        None => {
            values = reader.reserved_record_cells(id, meter)?;
        }
    }
    // Projections check identity only if they requested it; existence checks
    // resolve the binding alone and never perform an uncharged #key lookup.
    if let Some(identity) = values.get(reserved::KEY.as_bytes()) {
        state::check_identity(key, Some(identity.clone()))?;
    }
    Ok(Record { key, cells: values })
}
