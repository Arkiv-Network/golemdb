use std::sync::Arc;

use golemdb_branch::{BranchId, Branches, CellRead};
use golemdb_cells::{CellKey, CellName, CellReader};
use golemdb_merkle::HashProvider;
use golemdb_metering::{Accounting, Budget, WriteMeter};
use golemdb_storage::{ReadTransaction, Store};

use crate::{
    CreateEntry, Effects, Outcome, PatchEntry, ReadTarget, Record, RecordCells, RecordError,
    RecordKey, Result, WritePricing, admission,
    committed::{committed_snapshot, read_committed_by_key},
    plan::{self, WritePlan},
    state,
};

/// Record semantics over guarded branch views. Every operation requires an
/// explicit budget and returns an outcome with the selected pricing commit.
/// Failed writes retain no effects. Every write uses the supplied pricing;
/// explicit zero weights preserve full accounting while keeping calls free.
pub struct Records<S, H> {
    branches: Branches<S, H>,
    write_pricing: Arc<WritePricing>,
}

impl<S, H> Clone for Records<S, H> {
    fn clone(&self) -> Self {
        Self {
            branches: self.branches.clone(),
            write_pricing: self.write_pricing.clone(),
        }
    }
}

impl<S: Store, H: HashProvider> Records<S, H> {
    pub fn new(branches: Branches<S, H>, pricing: WritePricing) -> Self {
        Self {
            branches,
            write_pricing: Arc::new(pricing),
        }
    }

    /// Read a full record or a projection from one validated snapshot.
    pub fn get(
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

    /// Admit and plan without writes, approve the complete cost, then apply.
    /// Zero and nonzero schedules use the same metered execution path.
    pub fn create(
        &self,
        branch: BranchId,
        key: RecordKey,
        entries: Vec<CreateEntry>,
        budget: Budget,
    ) -> Outcome<RecordKey, RecordError> {
        self.execute_write(branch, budget, |reader, pricing, meter| {
            admission::base(key, meter)?;
            let values = admission::create(&pricing.cell_limits, entries, meter)?;
            Ok((
                key,
                plan::create(reader, key, values, pricing.record_limits, meter)?,
            ))
        })
    }

    /// Explicit writes may change kind; set preserves kind and requires a cell.
    /// No-ops pay only for admission and reads, and do not advance the journal.
    pub fn patch(
        &self,
        branch: BranchId,
        key: RecordKey,
        entries: Vec<PatchEntry>,
        budget: Budget,
    ) -> Outcome<(), RecordError> {
        self.execute_write(branch, budget, |reader, pricing, meter| {
            admission::base(key, meter)?;
            let changes = admission::patch(&pricing.cell_limits, entries, meter)?;
            Ok((
                (),
                plan::patch(reader, key, changes, pricing.record_limits, meter)?,
            ))
        })
    }

    /// Delete live cells, identity and binding; never rewind the allocator.
    pub fn delete(
        &self,
        branch: BranchId,
        key: RecordKey,
        budget: Budget,
    ) -> Outcome<(), RecordError> {
        self.execute_write(branch, budget, |reader, _, meter| {
            admission::base(key, meter)?;
            Ok(((), plan::delete(reader, key, meter)?))
        })
    }

    fn execute_write<T>(
        &self,
        branch: BranchId,
        budget: Budget,
        operation: impl FnOnce(
            &CellRead<'_, S::Read<'_>>,
            &WritePricing,
            &mut WriteMeter<'_>,
        ) -> Result<(T, WritePlan)>,
    ) -> Outcome<T, RecordError> {
        let mut priced_at = None;
        let mut effects = Effects::default();
        let mut accounting = Accounting::default();
        let result = self
            .branches
            .write_with_origin(branch, |origin, writer| {
                priced_at = Some(origin);
                let pricing = &self.write_pricing;
                let mut meter = WriteMeter::new(&pricing.schedule, pricing.depths, budget);
                let (value, plan) = match operation(&writer.as_read(), pricing, &mut meter) {
                    Ok(planned) => planned,
                    Err(error) => {
                        accounting = meter.into_accounting();
                        return Err(error);
                    }
                };
                let approved = match meter.finish() {
                    Ok(accounting) => accounting,
                    Err(failure) => {
                        accounting = failure.accounting.clone();
                        return Err(failure.into());
                    }
                };
                // The immutable read view is gone. No other operation can change
                // this branch between planning and application under this guard.
                effects = plan.apply(writer);
                accounting = approved;
                Ok(value)
            })
            .map_err(Into::into);
        Outcome::new(result, priced_at, effects).with_accounting(accounting)
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
