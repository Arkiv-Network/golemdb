use golemdb_branch::CellRead;
use golemdb_cells::{CellKey, CellLimits, CellNameRef, CellValue, reserved, system};
use golemdb_storage::ReadTransaction;

use crate::{RecordError, RecordKey, Result};

pub(crate) fn binding(key: RecordKey) -> CellKey {
    CellKey::new(system::RECORD_KEYS.id, CellNameRef::raw(&key.0))
}

pub(crate) fn resolve<R: ReadTransaction>(
    cell_reader: &CellRead<'_, R>,
    key: RecordKey,
) -> Result<Option<u64>> {
    let Some(id) = record_id(key, cell_reader.get(&binding(key))?)? else {
        return Ok(None);
    };
    check_identity(key, cell_reader.get(&CellKey::new(id, reserved::KEY))?)?;
    Ok(Some(id))
}

/// Decode and validate the binding's record-level meaning after cell decoding.
pub(crate) fn record_id(key: RecordKey, binding_value: Option<CellValue>) -> Result<Option<u64>> {
    let Some(value) = binding_value else {
        // Reserved records must have been initialized, including their bindings.
        return if system::by_key(&key.0).is_some() {
            Err(RecordError::CorruptState("reserved record binding missing"))
        } else {
            Ok(None)
        };
    };
    let id = value
        .as_u64()
        .filter(|_| !value.is_indexable())
        .ok_or(RecordError::CorruptState("invalid record binding"))?;
    match system::by_key(&key.0) {
        Some(record) if id != record.id => {
            return Err(RecordError::CorruptState("reserved record ID mismatch"));
        }
        None if id < system::FIRST_USER_RECORD_ID => {
            return Err(RecordError::CorruptState("user key bound to reserved ID"));
        }
        _ => {}
    }
    Ok(Some(id))
}

pub(crate) fn check_identity(key: RecordKey, value: Option<CellValue>) -> Result<()> {
    let identity = value.ok_or(RecordError::CorruptState("bound record has no #key"))?;
    if identity_key(&identity) != Some(key) {
        return Err(RecordError::CorruptState("binding and #key disagree"));
    }
    Ok(())
}

/// The key a record's `#key` cell holds; `None` unless it is a `bytes32` field.
pub(crate) fn identity_key(identity: &CellValue) -> Option<RecordKey> {
    identity
        .as_bytes32()
        .filter(|_| !identity.is_indexable())
        .map(RecordKey)
}

pub(crate) fn limits<R: ReadTransaction>(cell_reader: &CellRead<'_, R>) -> Result<CellLimits> {
    let read = |name| {
        let value = cell_reader
            .get(&CellKey::new(system::PARAMS.id, name))?
            .ok_or(RecordError::CorruptState("required #params cell missing"))?;
        value
            .as_u32()
            .filter(|_| !value.is_indexable())
            .ok_or(RecordError::CorruptState("invalid #params cell"))
    };
    Ok(CellLimits {
        max_cell_name_len: read(reserved::MAX_CELL_NAME_LEN)?,
        max_str_len: read(reserved::MAX_STR_LEN)?,
        max_bytes_len: read(reserved::MAX_BYTES_LEN)?,
    })
}

pub(crate) fn allocator_key() -> CellKey {
    CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID)
}

pub(crate) fn next_id<R: ReadTransaction>(cell_reader: &CellRead<'_, R>) -> Result<(u64, u64)> {
    let value = cell_reader
        .get(&allocator_key())?
        .ok_or(RecordError::CorruptState("allocator missing"))?;
    let id = value
        .as_u64()
        .filter(|_| !value.is_indexable())
        .ok_or(RecordError::CorruptState("invalid allocator"))?;
    if id < system::FIRST_USER_RECORD_ID {
        return Err(RecordError::CorruptState("allocator points to reserved ID"));
    }
    let next = id.checked_add(1).ok_or(RecordError::RecordIdExhausted)?;
    Ok((id, next))
}
