use golemdb_branch::{BranchId, Branches, CellRead, CellWrite};
use golemdb_cells::{CellKey, CellName, CellReader, CellValue, reserved};
use golemdb_merkle::HashProvider;
use golemdb_metering::Budget;
use golemdb_storage::{ReadTransaction, Store};

use crate::{
    CellPatch, CreateEntry, Effects, Outcome, PatchChange, PatchEntry, ReadTarget, Record,
    RecordCells, RecordError, RecordKey, RecordPatch, Result, admission,
    committed::{committed_snapshot, read_committed_by_key},
    state,
};

/// Record semantics over guarded branch views. Reports retain the selected
/// pricing commit and actual user effects, including on failed calls.
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

    /// Read a full record or a projection from one validated snapshot.
    pub fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
    ) -> Result<Record> {
        self.get_report(target, key, projection, Budget::Unlimited)
            .result
    }

    pub fn get_report(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        _budget: Budget,
    ) -> Outcome<Record, RecordError> {
        let mut priced_at = None;
        let result = match target {
            ReadTarget::Branch(branch) => self
                .branches
                .read_with_origin(branch, |origin, reader| {
                    priced_at = Some(origin);
                    let projection = admission::projection(projection);
                    read_branch_record(reader, key, projection.as_deref())
                })
                .map_err(Into::into),
            target => {
                let commit = match target {
                    ReadTarget::Commit(c) => Some(c),
                    _ => None,
                };
                match committed_snapshot(self.branches.store(), commit) {
                    Ok((tx, head)) => {
                        priced_at = Some(head);
                        let projection = admission::projection(projection);
                        read_committed_by_key(&CellReader::new(&tx), key, projection.as_deref())
                    }
                    Err(error) => {
                        if let RecordError::CommitUnavailable { head, .. } = &error {
                            priced_at = Some(*head);
                        }
                        Err(error)
                    }
                }
            }
        };
        Outcome::new(result, priced_at, Effects::default())
    }

    /// Trusted convenience entrypoint. Uses the same admission path with an
    /// unlimited budget and drops the report; hosts expose the budgeted API.
    pub fn create(
        &self,
        branch: BranchId,
        key: RecordKey,
        values: RecordCells,
    ) -> Result<RecordKey> {
        let entries = values
            .into_iter()
            .map(|(name, value)| CreateEntry {
                name,
                kind: value.kind(),
                value,
            })
            .collect();
        self.create_report(branch, key, entries, Budget::Unlimited)
            .result
    }

    /// Admit original entries only after handle validation. Budgets are passed
    /// into execution but charging and enforcement remain unimplemented.
    pub fn create_report(
        &self,
        branch: BranchId,
        key: RecordKey,
        entries: Vec<CreateEntry>,
        budget: Budget,
    ) -> Outcome<RecordKey, RecordError> {
        self.write_report(branch, budget, |writer, effects| {
            admission::key(key)?;
            let reader = writer.as_read();
            let limits = state::limits(&reader)?;
            let values = admission::create(&limits, entries)?;
            if state::resolve(&reader, key)?.is_some() {
                return Err(RecordError::AlreadyExists);
            }
            let (id, next) = state::next_id(&reader)?;
            if reader
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
                effects.change(&name, None, Some(&value))?;
                writer.put(CellKey::new(id, name), value);
            }
            writer.put(
                CellKey::new(id, reserved::KEY),
                CellValue::from_bytes32(key.0),
            );
            writer.put(state::binding(key), CellValue::from_u64(id));
            writer.put(state::allocator_key(), CellValue::from_u64(next));
            Ok(key)
        })
    }

    pub fn patch(&self, branch: BranchId, key: RecordKey, changes: RecordPatch) -> Result<()> {
        let entries = changes
            .into_iter()
            .map(|(name, change)| PatchEntry {
                name,
                change: match change {
                    CellPatch::Write(value) => PatchChange::Write {
                        kind: value.kind(),
                        value,
                    },
                    CellPatch::Set(value) => PatchChange::Set(value),
                    CellPatch::Remove => PatchChange::Remove,
                },
            })
            .collect();
        self.patch_report(branch, key, entries, Budget::Unlimited)
            .result
    }

    /// Explicit writes may change kind; set preserves kind and requires a cell.
    /// No-ops do not write, advance the journal, or contribute effects.
    pub fn patch_report(
        &self,
        branch: BranchId,
        key: RecordKey,
        entries: Vec<PatchEntry>,
        budget: Budget,
    ) -> Outcome<(), RecordError> {
        self.write_report(branch, budget, |writer, effects| {
            admission::key(key)?;
            let reader = writer.as_read();
            let limits = state::limits(&reader)?;
            let changes = admission::patch(&limits, entries)?;
            let id = state::resolve(&reader, key)?.ok_or(RecordError::NotFound)?;
            for (name, change) in changes {
                let address = CellKey::new(id, name.clone());
                let before = writer.get(&address)?;
                let after = match change {
                    CellPatch::Write(value) => Some(value),
                    CellPatch::Set(value) => {
                        let old = before
                            .as_ref()
                            .ok_or_else(|| RecordError::CellNotFound { name: name.clone() })?;
                        Some(value.with_kind(old.kind()).map_err(|source| {
                            RecordError::InvalidInput {
                                message: format!(
                                    "set({name:?}): invalid cell value for stored kind"
                                ),
                                source: Some(Box::new(source)),
                            }
                        })?)
                    }
                    CellPatch::Remove => None,
                };
                if before == after {
                    continue;
                }
                effects.change(&name, before.as_ref(), after.as_ref())?;
                match after {
                    Some(value) => writer.put(address, value),
                    None => writer.delete(address),
                }
            }
            Ok(())
        })
    }

    pub fn delete(&self, branch: BranchId, key: RecordKey) -> Result<()> {
        self.delete_report(branch, key, Budget::Unlimited).result
    }

    /// Delete user cells, identity and binding; never rewind the allocator.
    pub fn delete_report(
        &self,
        branch: BranchId,
        key: RecordKey,
        budget: Budget,
    ) -> Outcome<(), RecordError> {
        self.write_report(branch, budget, |writer, effects| {
            admission::key(key)?;
            let reader = writer.as_read();
            let id = state::resolve(&reader, key)?.ok_or(RecordError::NotFound)?;
            let rows = reader
                .scan_prefix(&id.to_be_bytes())?
                .collect::<golemdb_branch::Result<Vec<_>>>()?;
            for (address, value) in rows {
                effects.change(&address.name().into(), Some(&value), None)?;
                writer.delete(address);
            }
            writer.delete(state::binding(key));
            Ok(())
        })
    }

    fn write_report<T>(
        &self,
        branch: BranchId,
        _budget: Budget,
        operation: impl FnOnce(&mut CellWrite<'_, S::Read<'_>>, &mut Effects) -> Result<T>,
    ) -> Outcome<T, RecordError> {
        let mut priced_at = None;
        let mut effects = Effects::default();
        let result = self
            .branches
            .write_with_origin(branch, |origin, writer| {
                priced_at = Some(origin);
                operation(writer, &mut effects)
            })
            .map_err(Into::into);
        // Branches restores the overlay on error; partial effect counts must
        // likewise never escape as successful changes.
        Outcome::new(result, priced_at, effects)
    }
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
