//! Committed reads, shared by `get` and by queries: one storage snapshot,
//! checked against its head, and records materialized from it.

use golemdb_branch::{CommitId, read_head};
use golemdb_cells::{CellKey, CellName, CellReader, reserved};
use golemdb_storage::{ReadTransaction, Store};

use crate::{Record, RecordCells, RecordError, RecordKey, Result, state};

/// Open one storage snapshot for a committed read and resolve `commit`
/// against its head: `None` reads the head; `Some(c)` must name it, because
/// historical reads are not supported yet. Returns the snapshot and its head.
pub fn committed_snapshot<S: Store>(
    store: &S,
    commit: Option<CommitId>,
) -> Result<(S::Read<'_>, CommitId)> {
    let tx = store.begin_read()?;
    let head = read_head(&tx)?;
    match commit {
        Some(requested) if requested != head => {
            Err(RecordError::CommitUnavailable { requested, head })
        }
        Some(_) | None => Ok((tx, head)),
    }
}

/// The committed record `id`, as `get` returns it: every cell including
/// `#key` and `#meta`, or only the projected cells, with missing user cells
/// omitted. The key comes from the record's own `#key` cell.
///
/// For engine readers that already know `id` names a live record, such as a
/// query taking IDs from the index. Unlike `get`, it does not check the key's
/// binding back to `id`. A record with no valid `#key` is
/// [`RecordError::CorruptState`].
pub fn read_committed_by_id<R: ReadTransaction>(
    cell_reader: &CellReader<'_, R>,
    id: u64,
    projection: Option<&[CellName]>,
) -> Result<Record> {
    let identity = cell_reader
        .get(&CellKey::new(id, reserved::KEY))?
        .ok_or(RecordError::CorruptState("record has no #key"))?;
    let key = state::identity_key(&identity).ok_or(RecordError::CorruptState("invalid #key"))?;
    let cells = read_cells(cell_reader, id, projection)?;
    Ok(Record { key, cells })
}

/// The committed record bound to `key`, after checking the binding against
/// the record's `#key`.
pub(crate) fn read_committed_by_key<R: ReadTransaction>(
    cell_reader: &CellReader<'_, R>,
    key: RecordKey,
    projection: Option<&[CellName]>,
) -> Result<Record> {
    let id = state::record_id(key, cell_reader.get(&state::binding(key))?)?
        .ok_or(RecordError::NotFound)?;
    state::check_identity(key, cell_reader.get(&CellKey::new(id, reserved::KEY))?)?;
    let cells = read_cells(cell_reader, id, projection)?;
    Ok(Record { key, cells })
}

fn read_cells<R: ReadTransaction>(
    cell_reader: &CellReader<'_, R>,
    id: u64,
    projection: Option<&[CellName]>,
) -> Result<RecordCells> {
    let mut values = RecordCells::new();
    match projection {
        Some(names) => {
            for name in names {
                if let Some(value) = cell_reader.get(&CellKey::new(id, name.clone()))? {
                    values.insert(name.clone(), value);
                }
            }
        }
        None => {
            for row in cell_reader.scan_record(id)? {
                let (address, value) = row?;
                values.insert(address.name().into(), value);
            }
        }
    }
    Ok(values)
}
