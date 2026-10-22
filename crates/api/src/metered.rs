use crate::{ApiError, CommitId, Ledger, Result};

/// Every record/query outcome includes a receipt. Explicitly discard it with
/// `into_result()` only when another layer does not need to account for the call.
#[derive(Debug)]
#[must_use]
pub struct Metered<T> {
    pub result: Result<T>,
    pub receipt: Receipt,
}

impl<T> Metered<T> {
    pub fn new(result: Result<T>, receipt: Receipt) -> Self {
        Self { result, receipt }
    }
    /// Zero-cost receipt for mocks and unmetered implementations.
    pub fn unmetered(result: Result<T>, priced_at: Option<CommitId>) -> Self {
        Self::new(
            result,
            Receipt {
                cost: 0,
                priced_at,
                details: Details::default(),
                ledger: Ledger::default(),
            },
        )
    }
    pub fn into_result(self) -> Result<T> {
        self.result
    }
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Metered<U> {
        Metered {
            result: self.result.map(f),
            receipt: self.receipt,
        }
    }
    pub fn map_err(self, f: impl FnOnce(ApiError) -> ApiError) -> Self {
        Self {
            result: self.result.map_err(f),
            receipt: self.receipt,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Receipt {
    /// Charged work. Database calls remain zero until planner integration.
    pub cost: u64,
    pub priced_at: Option<CommitId>,
    pub details: Details,
    /// Counts per pricing weight, including charged work on failed calls.
    pub ledger: Ledger,
}

/// Actual user-cell effects; system cells, failed writes and reads count zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Details {
    pub cells_created: u64,
    pub cells_updated: u64,
    pub cells_deleted: u64,
    pub index_joins: u64,
    pub index_leaves: u64,
    pub cell_bytes_written: u64,
    pub cell_bytes_deleted: u64,
    pub index_bytes_written: u64,
    pub index_bytes_deleted: u64,
}

impl From<golemdb_record::Effects> for Details {
    fn from(e: golemdb_record::Effects) -> Self {
        Self {
            cells_created: e.cells_created,
            cells_updated: e.cells_updated,
            cells_deleted: e.cells_deleted,
            index_joins: e.index_joins,
            index_leaves: e.index_leaves,
            cell_bytes_written: e.cell_bytes_written,
            cell_bytes_deleted: e.cell_bytes_deleted,
            index_bytes_written: e.index_bytes_written,
            index_bytes_deleted: e.index_bytes_deleted,
        }
    }
}

impl<T, E: Into<ApiError>> From<golemdb_record::Outcome<T, E>> for Metered<T> {
    fn from(outcome: golemdb_record::Outcome<T, E>) -> Self {
        Self {
            result: outcome.result.map_err(Into::into),
            receipt: Receipt {
                cost: outcome.accounting.cost,
                priced_at: outcome.priced_at,
                details: outcome.effects.into(),
                ledger: outcome.accounting.ledger,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use golemdb_metering::{
        Budget, INITIAL_DEPTHS, MutationCharge, PlanningStep, Schedule, WRITE_CODE_VERSION,
        WeightName as W, WriteMeter,
    };
    use golemdb_record::{Effects, Outcome, RecordError};

    fn schedule() -> Schedule {
        Schedule::new(
            WRITE_CODE_VERSION,
            [
                (W::WRITE_ADMISSION_BASE, 2),
                (W::WRITE_CELL_DELETE, 4),
                (W::WRITE_CELL_TRIE_UPDATE, 1),
            ],
        )
        .unwrap()
    }

    #[test]
    fn approval_and_domain_errors_keep_cost_and_ledger_through_api_conversion() {
        let schedule = schedule();
        for result in [Ok(()), Err(RecordError::AlreadyExists)] {
            let success = result.is_ok();
            let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Unlimited);
            meter.run_step(PlanningStep::AdmissionBase, || ()).unwrap();
            let accounting = meter.finish().unwrap();
            let expected = accounting.clone();
            let outcome = Outcome::new(
                result,
                Some(CommitId::new(42)),
                Effects {
                    cells_created: 1,
                    ..Effects::default()
                },
            )
            .with_accounting(accounting);
            let metered: Metered<()> = outcome.into();
            assert_eq!(metered.receipt.cost, 2);
            assert_eq!(metered.receipt.ledger, expected.ledger);
            assert_eq!(metered.receipt.priced_at, Some(CommitId::new(42)));
            assert_eq!(metered.receipt.details.cells_created, u64::from(success));
            assert_eq!(
                schedule.price(&metered.receipt.ledger).unwrap(),
                metered.receipt.cost
            );
            let receipt = metered.receipt.clone();
            let mapped = metered.map(|()| 7).map_err(|error| error);
            assert_eq!(mapped.receipt, receipt);
        }
    }

    #[test]
    fn metering_failure_preserves_required_and_charged_planning_without_effects() {
        let schedule = schedule();
        for budget in [0, 2] {
            let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Limited(budget));
            let failure = match meter.run_step(PlanningStep::AdmissionBase, || ()) {
                Err(failure) => failure,
                Ok(()) => {
                    meter
                        .add_planned_mutation(MutationCharge::CellDelete)
                        .unwrap();
                    meter.finish().unwrap_err()
                }
            };
            let expected = failure.clone();
            let outcome: Outcome<(), RecordError> =
                Outcome::metering_failure(Some(CommitId::new(42)), failure);
            let metered: Metered<()> = outcome.into();
            assert!(
                matches!(metered.result, Err(ApiError::OutOfBudget { required }) if required == expected.required)
            );
            assert_eq!(metered.receipt.cost, budget);
            assert_eq!(metered.receipt.ledger, expected.accounting.ledger);
            assert_eq!(metered.receipt.priced_at, Some(CommitId::new(42)));
            assert_eq!(metered.receipt.details, Details::default());
            assert_eq!(metered.receipt.ledger.count("write.cell.delete"), 0);
        }
    }
}
