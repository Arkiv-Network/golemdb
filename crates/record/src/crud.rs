use golemdb_branch::{BranchId, Branches, CellRead, CommitId};
use golemdb_cells::{CellKey, CellLimits, CellName, CellReader, CellValue, reserved, system};
use golemdb_merkle::HashProvider;
use golemdb_storage::{ReadTransaction, Store};

use crate::{
    CellPatch, ReadTarget, Record, RecordCells, RecordError, RecordKey, RecordPatch, Result,
    committed::{committed_snapshot, read_committed_by_key},
    state,
};

/// Cloneable record facade; clones share the supplied branch manager.
/// Failed mutations restore both the working cells and branch version. Successful
/// mutations remain uncommitted until the branch manager commits them.
pub struct Records<S, H> {
    branches: Branches<S, H>,
}

impl<S, H> Clone for Records<S, H> {
    fn clone(&self) -> Self {
        Self {
            branches: self.branches.clone(),
        }
    }
}

impl<S: Store, H: HashProvider> Records<S, H> {
    pub fn new(branches: Branches<S, H>) -> Self {
        Self { branches }
    }

    /// Read a full record (including `#key`) or an explicit projection. Missing
    /// projected cells are omitted; an empty projection returns only the record's
    /// key. Duplicate names collapse in the result. Names may be raw, including
    /// reserved cells.
    /// Record existence and identity are checked even for an empty projection.
    /// Committed reads resolve the head, binding, identity, and cells within one
    /// storage snapshot, so a concurrent commit cannot mix their states.
    pub fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
    ) -> Result<Record> {
        match target {
            ReadTarget::Branch(branch) => self
                .branches
                .read(branch, |cell_reader| {
                    read_branch_record(cell_reader, key, projection)
                })
                .map_err(Into::into),
            ReadTarget::Head => self.read_committed(None, key, projection),
            ReadTarget::Commit(commit) => self.read_committed(Some(commit), key, projection),
        }
    }

    // Explicit storage path: cell storage owns decoding and scanning; record owns
    // the binding/identity checks. No branch or empty overlay is created for reads.
    fn read_committed(
        &self,
        commit: Option<CommitId>,
        key: RecordKey,
        projection: Option<&[CellName]>,
    ) -> Result<Record> {
        let (tx, _) = committed_snapshot(self.branches.store(), commit)?;
        read_committed_by_key(&CellReader::new(&tx), key, projection)
    }

    /// Create a record with zero or more user cells. Exact reserved keys are
    /// rejected before branch access; all other validation follows handle checks.
    /// Names and values are checked in byte order before identity/allocation reads.
    pub fn create(
        &self,
        branch: BranchId,
        key: RecordKey,
        values: RecordCells,
    ) -> Result<RecordKey> {
        reject_reserved(key)?;
        self.branches
            .write(branch, |cell_writer| {
                let cell_reader = cell_writer.as_read();
                let limits = state::limits(&cell_reader)?;
                for (name, value) in &values {
                    validate(&limits, name, Some(value))?;
                }
                if state::resolve(&cell_reader, key)?.is_some() {
                    return Err(RecordError::AlreadyExists);
                }
                let (id, next) = state::next_id(&cell_reader)?;
                if cell_writer
                    .scan_prefix(&id.to_be_bytes())?
                    .next()
                    .transpose()?
                    .is_some()
                {
                    return Err(RecordError::CorruptState(
                        "allocator points to occupied record",
                    ));
                }
                for (name, value) in values {
                    cell_writer.put(CellKey::new(id, name), value);
                }
                cell_writer.put(
                    CellKey::new(id, reserved::KEY),
                    CellValue::from_bytes32(key.0),
                );
                cell_writer.put(state::binding(key), CellValue::from_u64(id));
                cell_writer.put(state::allocator_key(), CellValue::from_u64(next));
                Ok(key)
            })
            .map_err(Into::into)
    }

    /// Apply a partial mutation. Empty patches and removing absent cells are
    /// no-ops on an existing record. Removing the last user cell preserves the
    /// record's #key and binding; only delete removes the record.
    /// A set may change both type and indexed status. Identical sets and other
    /// no-ops add no journal entries and leave the branch version unchanged.
    pub fn patch(&self, branch: BranchId, key: RecordKey, changes: RecordPatch) -> Result<()> {
        reject_reserved(key)?;
        self.branches
            .write(branch, |cell_writer| {
                let cell_reader = cell_writer.as_read();
                let id = state::resolve(&cell_reader, key)?.ok_or(RecordError::NotFound)?;
                let limits = state::limits(&cell_reader)?;
                for (name, change) in &changes {
                    validate(
                        &limits,
                        name,
                        match change {
                            CellPatch::Set(value) => Some(value),
                            CellPatch::Remove => None,
                        },
                    )?;
                }
                for (name, change) in changes {
                    let address = CellKey::new(id, name);
                    match change {
                        CellPatch::Set(value) => {
                            if cell_writer.get(&address)?.as_ref() != Some(&value) {
                                cell_writer.put(address, value);
                            }
                        }
                        CellPatch::Remove => {
                            if cell_writer.get(&address)?.is_some() {
                                cell_writer.delete(address);
                            }
                        }
                    }
                }
                Ok(())
            })
            .map_err(Into::into)
    }

    /// Remove the binding and every live cell, including #key. Successful
    /// deletion never rewinds the allocator; recreation receives a fresh ID.
    /// Branch rollback can still restore uncommitted records, bindings, and
    /// allocations.
    pub fn delete(&self, branch: BranchId, key: RecordKey) -> Result<()> {
        reject_reserved(key)?;
        self.branches
            .write(branch, |cell_writer| {
                let cell_reader = cell_writer.as_read();
                let id = state::resolve(&cell_reader, key)?.ok_or(RecordError::NotFound)?;
                let addresses = cell_reader
                    .scan_prefix(&id.to_be_bytes())?
                    .map(|row| row.map(|(address, _)| address))
                    .collect::<golemdb_branch::Result<Vec<_>>>()?;
                for address in addresses {
                    cell_writer.delete(address);
                }
                cell_writer.delete(state::binding(key));
                Ok(())
            })
            .map_err(Into::into)
    }
}

fn reject_reserved(key: RecordKey) -> Result<()> {
    if system::by_key(&key.0).is_some() {
        Err(RecordError::Reserved)
    } else {
        Ok(())
    }
}

fn validate(limits: &CellLimits, name: &CellName, value: Option<&CellValue>) -> Result<()> {
    limits
        .parse_user_name(name.as_bytes())
        .map_err(|error| RecordError::InvalidArgument(error.to_string()))?;
    if let Some(value) = value {
        limits
            .validate_value(value.as_view())
            .map_err(|error| RecordError::InvalidArgument(error.to_string()))?;
    }
    Ok(())
}

fn read_branch_record<R: ReadTransaction>(
    cell_reader: &CellRead<'_, R>,
    key: RecordKey,
    projection: Option<&[CellName]>,
) -> Result<Record> {
    let id = state::resolve(cell_reader, key)?.ok_or(RecordError::NotFound)?;
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
            for row in cell_reader.scan_prefix(&id.to_be_bytes())? {
                let (address, value) = row?;
                values.insert(address.name().into(), value);
            }
        }
    }
    Ok(Record { key, cells: values })
}
