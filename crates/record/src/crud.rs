use std::sync::Arc;

use golemdb_branch::{BranchId, Branches, CellRead};
use golemdb_cells::CellName;
use golemdb_merkle::HashProvider;
use golemdb_metering::{Accounting, Budget, ReadMeter, WriteMeter};
use golemdb_storage::{ReadTransaction, Store};

use crate::{
    CreateEntry, Effects, Outcome, PatchEntry, ReadTarget, Record, RecordError, RecordKey, Result,
    WritePricing, admission,
    committed::CommittedRead,
    plan::{self, WritePlan},
    read,
    read_metering::MeteredCellFetcher,
};

/// Record semantics over guarded branch views. Every operation requires an
/// explicit budget and returns an outcome with the selected pricing commit.
/// Failed writes retain no effects. Every write uses the supplied pricing;
/// explicit zero weights preserve full accounting while keeping calls free.
pub struct Records<S, H> {
    branches: Branches<S, H>,
    pricing: Arc<WritePricing>,
}

impl<S, H> Clone for Records<S, H> {
    fn clone(&self) -> Self {
        Self {
            branches: self.branches.clone(),
            pricing: self.pricing.clone(),
        }
    }
}

impl<S: Store, H: HashProvider> Records<S, H> {
    pub fn new(branches: Branches<S, H>, pricing: WritePricing) -> Self {
        Self {
            branches,
            pricing: Arc::new(pricing),
        }
    }

    /// Read a full record or a projection from one validated snapshot.
    pub fn get(
        &self,
        target: ReadTarget,
        key: RecordKey,
        projection: Option<&[CellName]>,
        budget: Budget,
    ) -> Outcome<Record, RecordError> {
        match target {
            ReadTarget::Branch(branch) => {
                match self.branches.read_with_origin(branch, |origin, reader| {
                    Ok::<_, RecordError>(self.read_get(
                        origin,
                        MeteredCellFetcher::Branch(reader),
                        key,
                        projection,
                        budget,
                    ))
                }) {
                    Ok(outcome) => outcome,
                    Err(error) => Outcome::new(Err(error.into()), None, Effects::default()),
                }
            }
            target => {
                let commit = match target {
                    ReadTarget::Commit(c) => Some(c),
                    _ => None,
                };
                let mut context = match self
                    .branches
                    .store()
                    .begin_read()
                    .map_err(RecordError::from)
                    .and_then(CommittedRead::new)
                {
                    Ok(context) => context,
                    Err(error) => return Outcome::new(Err(error), None, Effects::default()),
                };
                let head = context.head();
                if let Err(error) = context.select_target(commit, self.branches.pruning()) {
                    return Outcome::new(Err(error), Some(head), Effects::default());
                }
                self.read_get(
                    head,
                    MeteredCellFetcher::Committed(context.cells()),
                    key,
                    projection,
                    budget,
                )
            }
        }
    }

    fn read_get<R: ReadTransaction>(
        &self,
        pricing_commit: golemdb_branch::CommitId,
        reader: MeteredCellFetcher<'_, '_, R>,
        key: RecordKey,
        projection: Option<&[CellName]>,
        budget: Budget,
    ) -> Outcome<Record, RecordError> {
        let snapshot = match self.pricing.provider.snapshot(pricing_commit.get()) {
            Ok(snapshot) => snapshot,
            Err(error) => return Outcome::new(Err(error.into()), None, Effects::default()),
        };
        let mut meter = ReadMeter::new(&snapshot.model.schedule, budget);
        let result = read::get(&reader, key, projection, &mut meter);
        Outcome::new(result, Some(pricing_commit), Effects::default())
            .with_accounting(meter.into_accounting())
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
                let pricing = &self.pricing;
                // The branch guard has validated the origin against its storage snapshot.
                // Record now captures cached pricing for that commit, before admission.
                let snapshot = pricing.provider.snapshot(origin.get())?;
                priced_at = Some(origin);
                let mut meter = WriteMeter::new(&snapshot.model.schedule, pricing.depths, budget);
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
