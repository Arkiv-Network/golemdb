use std::collections::{BTreeMap, btree_map::Entry};

use crate::{Accounting, Ledger, WeightName};

/// Initial write-pricing code identifier, not a persisted model version or a
/// declaration that production weights have been calibrated.
pub const WRITE_CODE_VERSION: u64 = 1;

/// Immutable weights supplied by the caller. No database or commit identifiers.
///
/// Missing weights stay absent; only a charge that uses one fails. Extra names,
/// including host weights, are retained for inspection but never selected by the
/// write meter. Declaration/model checks belong to the persisted pricing loader;
/// runtime activation and cache replacement remain deferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    code_version: u64,
    weights: BTreeMap<WeightName, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("unsupported write metering code version {0}")]
    UnsupportedCodeVersion(u64),
    #[error("duplicate weight {0}")]
    DuplicateWeight(WeightName),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PricingError {
    #[error("unpriced weight {0}")]
    Unpriced(WeightName),
    #[error("metering cost or count overflow")]
    Overflow,
}

impl Schedule {
    /// Explicit free prices for every weight used by the current write model,
    /// including its planning reads. Missing/custom weights stay absent.
    pub fn zero_write() -> Self {
        use WeightName as W;

        Self {
            code_version: WRITE_CODE_VERSION,
            weights: [
                W::WRITE_ADMISSION_BASE,
                W::WRITE_ADMISSION_ENTRY,
                W::WRITE_ADMISSION_BYTE,
                W::WRITE_RECORD_CREATE,
                W::WRITE_RECORD_PATCH,
                W::WRITE_RECORD_DELETE,
                W::WRITE_CELL_CREATE,
                W::WRITE_CELL_UPDATE,
                W::WRITE_CELL_DELETE,
                W::WRITE_CELL_TRIE_UPDATE,
                W::WRITE_CELL_BYTE,
                W::WRITE_INDEX_JOIN,
                W::WRITE_INDEX_LEAVE,
                W::WRITE_INDEX_TERM_CREATE,
                W::WRITE_INDEX_TRIE_UPDATE,
                W::WRITE_INDEX_BYTE,
                W::READ_KEY,
                W::READ_CELL,
                W::READ_TERM,
            ]
            .into_iter()
            .map(|name| (name, 0))
            .collect(),
        }
    }

    pub fn new(
        code_version: u64,
        weights: impl IntoIterator<Item = (WeightName, u64)>,
    ) -> Result<Self, ScheduleError> {
        if code_version != WRITE_CODE_VERSION {
            return Err(ScheduleError::UnsupportedCodeVersion(code_version));
        }
        let mut values = BTreeMap::new();
        for (name, price) in weights {
            match values.entry(name) {
                Entry::Vacant(entry) => {
                    entry.insert(price);
                }
                Entry::Occupied(entry) => {
                    return Err(ScheduleError::DuplicateWeight(entry.key().clone()));
                }
            }
        }
        Ok(Self {
            code_version,
            weights: values,
        })
    }

    pub fn code_version(&self) -> u64 {
        self.code_version
    }

    pub fn weight(&self, name: &str) -> Option<u64> {
        self.weights.get(name).copied()
    }

    pub fn weights(&self) -> impl Iterator<Item = (&WeightName, u64)> {
        self.weights.iter().map(|(name, weight)| (name, *weight))
    }

    /// Reconstruct a receipt's cost, with checked arithmetic, from its ledger.
    /// Zero-count weights are absent from ledgers and need no price.
    pub fn price(&self, ledger: &Ledger) -> Result<u64, PricingError> {
        ledger.iter().try_fold(0u64, |total, (name, count)| {
            let weight = self
                .weight(name.as_str())
                .ok_or_else(|| PricingError::Unpriced(name.clone()))?;
            let amount = weight.checked_mul(count).ok_or(PricingError::Overflow)?;
            total.checked_add(amount).ok_or(PricingError::Overflow)
        })
    }

    pub(crate) fn account(&self, ledger: Ledger) -> Result<Accounting, PricingError> {
        Ok(Accounting {
            cost: self.price(&ledger)?,
            ledger,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Budget, INITIAL_DEPTHS, MutationCharge, PlanningStep, RecordOpKind, WeightName as W,
        WriteMeter,
    };

    #[test]
    fn zero_write_prices_every_supported_step_and_mutation_without_hiding_missing_weights() {
        let schedule = Schedule::zero_write();
        let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Limited(0));
        for step in [
            PlanningStep::AdmissionBase,
            PlanningStep::AdmissionEntry,
            PlanningStep::AdmissionBytes(10),
            PlanningStep::RecordBase(RecordOpKind::Create),
            PlanningStep::RecordBase(RecordOpKind::Patch),
            PlanningStep::RecordBase(RecordOpKind::Delete),
            PlanningStep::CellRead,
            PlanningStep::TermRead,
        ] {
            meter.run_step(step, || ()).unwrap();
        }
        for mutation in [
            MutationCharge::CellCreate { bytes_written: 10 },
            MutationCharge::CellUpdate { bytes_written: 20 },
            MutationCharge::CellDelete,
            MutationCharge::IndexJoin {
                bytes_written: 5,
                create_term: false,
            },
            MutationCharge::IndexJoin {
                bytes_written: 6,
                create_term: true,
            },
            MutationCharge::IndexLeave,
        ] {
            meter.add_planned_mutation(mutation).unwrap();
        }
        let accounting = meter.finish().unwrap();
        assert_eq!(accounting.cost, 0);
        assert_eq!(accounting.ledger.iter().count(), schedule.weights().count());
        assert_eq!(accounting.ledger.count("read.key"), 3);
        assert_eq!(accounting.ledger.count("write.cell_byte"), 30);
        assert_eq!(accounting.ledger.count("write.index_term_create"), 1);
        assert_eq!(schedule.weight("host.custom"), None);
        assert!(matches!(
            schedule.price(&Ledger::from_counts([(WeightName::from("host.custom"), 1)])),
            Err(PricingError::Unpriced(_))
        ));
    }

    #[test]
    fn rejects_unsupported_code_and_duplicate_names_instead_of_selecting_a_value() {
        assert_eq!(
            Schedule::new(WRITE_CODE_VERSION + 1, []).unwrap_err(),
            ScheduleError::UnsupportedCodeVersion(WRITE_CODE_VERSION + 1)
        );
        assert_eq!(
            Schedule::new(
                WRITE_CODE_VERSION,
                [(W::READ_CELL, 1), (WeightName::from("read.cell"), 2),]
            )
            .unwrap_err(),
            ScheduleError::DuplicateWeight(W::READ_CELL)
        );
    }

    #[test]
    fn keeps_host_weights_and_distinguishes_missing_from_free() {
        let schedule = Schedule::new(
            WRITE_CODE_VERSION,
            [
                (W::READ_CELL, 0),
                (WeightName::from("host.arkiv.storage_price"), u64::MAX),
            ],
        )
        .unwrap();
        assert_eq!(schedule.code_version(), WRITE_CODE_VERSION);
        assert_eq!(schedule.weight("read.cell"), Some(0));
        assert_eq!(schedule.weight("read.key"), None);
        assert_eq!(schedule.weight("host.arkiv.storage_price"), Some(u64::MAX));
        let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Limited(0));
        let absent = meter
            .run_step(PlanningStep::CellRead, || None::<()>)
            .unwrap();
        assert!(absent.is_none());
        let accounting = meter.finish().unwrap();
        assert_eq!(accounting.cost, 0);
        assert_eq!(accounting.ledger.count("read.cell"), 1);
        assert_eq!(accounting.ledger.count("host.arkiv.storage_price"), 0);
    }
}
