use crate::{
    Accounting, Budget, FailureReason, Ledger, MeteringFailure, Schedule, WeightName as W,
};

/// Immediate read work. Live full-record scans prepay scans and returned bytes;
/// historical scans prepay returned bytes alone and charge traversal incrementally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStep {
    AdmissionBase,
    AdmissionEntry {
        bytes: u64,
    },
    ReadBase,
    KeyResolve,
    CellRead,
    CellScan,
    /// Prepay one encoded history bitmap's copying, validation and successor seek.
    CommitSet {
        encoded_bytes: u64,
    },
    ReturnedBytes(u64),
    FullRecordScan {
        cells: u64,
        returned_bytes: u64,
    },
}

impl ReadStep {
    fn ledger(self) -> Ledger {
        Ledger::from_counts(match self {
            Self::AdmissionBase => vec![(W::READ_ADMISSION_BASE, 1)],
            Self::AdmissionEntry { bytes } => vec![
                (W::READ_ADMISSION_ENTRY, 1),
                (W::READ_ADMISSION_BYTE, bytes),
            ],
            Self::ReadBase => vec![(W::READ_BASE, 1)],
            Self::KeyResolve => vec![(W::READ_KEY, 1)],
            Self::CellRead => vec![(W::READ_CELL, 1)],
            Self::CellScan => vec![(W::READ_CELL_SCAN, 1)],
            Self::CommitSet { encoded_bytes } => {
                vec![(W::READ_COMMIT_SET, 1), (W::READ_BYTE, encoded_bytes)]
            }
            Self::ReturnedBytes(bytes) => vec![(W::READ_BYTE, bytes)],
            Self::FullRecordScan {
                cells,
                returned_bytes,
            } => vec![(W::READ_CELL_SCAN, cells), (W::READ_BYTE, returned_bytes)],
        })
    }
}

/// One read's budget and charged work. No deferred mutations or final approval.
pub struct ReadMeter<'s> {
    schedule: &'s Schedule,
    budget: Budget,
    accounting: Accounting,
    failed: Option<FailureReason>,
}

impl<'s> ReadMeter<'s> {
    pub fn new(schedule: &'s Schedule, budget: Budget) -> Self {
        Self {
            schedule,
            budget,
            accounting: Accounting::default(),
            failed: None,
        }
    }

    /// Charge atomically before executing the step. A refused step contributes
    /// nothing, and a stopped meter cannot admit further work, even if cheaper.
    pub fn charge(&mut self, step: ReadStep) -> Result<(), MeteringFailure> {
        if let Some(reason) = self.failed.clone() {
            return Err(self.stop(reason));
        }
        let next = self
            .schedule
            .account(step.ledger())
            .and_then(|charge| self.accounting.checked_merge(&charge))
            .map_err(|error| self.stop(error.into()))?;
        if matches!(self.budget, Budget::Limited(limit) if next.cost > limit) {
            return Err(self.stop(FailureReason::BudgetExceeded));
        }
        self.accounting = next;
        Ok(())
    }

    fn stop(&mut self, reason: FailureReason) -> MeteringFailure {
        if self.failed.is_none()
            && let FailureReason::Pricing(crate::PricingError::Unpriced(weight)) = &reason
        {
            tracing::warn!(%weight, "read failed because metering weight is unpriced");
        }
        self.failed = Some(reason.clone());
        MeteringFailure {
            accounting: self.accounting.clone(),
            required: None,
            reason,
        }
    }

    pub fn into_accounting(self) -> Accounting {
        self.accounting
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PricingError, WRITE_CODE_VERSION};
    use ReadStep::*;

    fn schedule() -> Schedule {
        Schedule::new(
            WRITE_CODE_VERSION,
            [
                (W::READ_ADMISSION_BASE, 2),
                (W::READ_ADMISSION_ENTRY, 3),
                (W::READ_ADMISSION_BYTE, 5),
                (W::READ_BASE, 7),
                (W::READ_KEY, 11),
                (W::READ_CELL, 13),
                (W::READ_CELL_SCAN, 17),
                (W::READ_COMMIT_SET, 23),
                (W::READ_BYTE, 19),
            ],
        )
        .unwrap()
    }

    #[test]
    fn steps_produce_expected_ledgers_and_prices() {
        let schedule = schedule();
        for (step, counts, cost) in [
            (AdmissionBase, vec![("read.admission.base", 1)], 2),
            (
                AdmissionEntry { bytes: 4 },
                vec![("read.admission.entry", 1), ("read.admission.byte", 4)],
                23,
            ),
            (ReadBase, vec![("read.base", 1)], 7),
            (KeyResolve, vec![("read.key", 1)], 11),
            (CellRead, vec![("read.cell", 1)], 13),
            (CellScan, vec![("read.cell_scan", 1)], 17),
            (ReturnedBytes(4), vec![("read.byte", 4)], 76),
            (
                CommitSet { encoded_bytes: 4 },
                vec![("read.commit_set", 1), ("read.byte", 4)],
                99,
            ),
            (
                FullRecordScan {
                    cells: 2,
                    returned_bytes: 4,
                },
                vec![("read.cell_scan", 2), ("read.byte", 4)],
                110,
            ),
        ] {
            let expected = Accounting {
                cost,
                ledger: Ledger::from_counts(counts.into_iter().map(|(name, n)| (name.into(), n))),
            };
            assert_eq!(step.ledger(), expected.ledger, "{step:?}");
            for budget in [Budget::Limited(cost), Budget::Unlimited] {
                let mut meter = ReadMeter::new(&schedule, budget);
                meter.charge(step).unwrap();
                assert_eq!(meter.into_accounting(), expected, "{step:?}");
            }
        }
    }

    #[test]
    fn composite_charges_are_atomic_and_failure_stops_the_meter() {
        let schedule = schedule();
        for (step, budget) in [
            (AdmissionEntry { bytes: 4 }, 24),
            (CommitSet { encoded_bytes: 4 }, 100),
            (
                FullRecordScan {
                    cells: 2,
                    returned_bytes: 4,
                },
                111,
            ),
        ] {
            let mut meter = ReadMeter::new(&schedule, Budget::Limited(budget));
            meter.charge(AdmissionBase).unwrap();
            // Each budget is one short of the total, but can fund part of the step.
            let failure = meter.charge(step).unwrap_err();
            assert_eq!(failure.reason, FailureReason::BudgetExceeded);
            assert_eq!(failure.required, None);
            assert_eq!(
                failure.accounting,
                Accounting {
                    cost: 2,
                    ledger: Ledger::from_counts([(W::READ_ADMISSION_BASE, 1)]),
                }
            );
            // Even a free step cannot continue a stopped meter.
            assert_eq!(meter.charge(ReturnedBytes(0)).unwrap_err(), failure);
            assert_eq!(meter.into_accounting(), failure.accounting);
        }
    }

    #[test]
    fn zero_prices_preserve_counts_and_missing_prices_fail_only_when_used() {
        let schedule = Schedule::new(WRITE_CODE_VERSION, [(W::READ_ADMISSION_ENTRY, 0)]).unwrap();
        for budget in [
            Budget::Limited(0),
            Budget::Limited(u64::MAX),
            Budget::Unlimited,
        ] {
            let mut meter = ReadMeter::new(&schedule, budget);
            meter.charge(AdmissionEntry { bytes: 0 }).unwrap();
            meter.charge(ReturnedBytes(0)).unwrap(); // Zero units need no price.
            let failure = meter.charge(AdmissionEntry { bytes: 1 }).unwrap_err();
            assert_eq!(
                failure.reason,
                FailureReason::Pricing(PricingError::Unpriced(W::READ_ADMISSION_BYTE))
            );
            assert_eq!(failure.required, None);
            assert_eq!(
                failure.accounting,
                Accounting {
                    cost: 0,
                    ledger: Ledger::from_counts([(W::READ_ADMISSION_ENTRY, 1)]),
                }
            );
            assert_eq!(meter.charge(ReturnedBytes(0)).unwrap_err(), failure);
            assert_eq!(meter.into_accounting(), failure.accounting);
        }
    }

    #[test]
    fn overflow_preserves_accounting_even_with_unlimited_budget() {
        for budget in [Budget::Limited(u64::MAX), Budget::Unlimited] {
            for (byte_price, first, overflowing) in [
                // Price multiplication, accumulated cost, then zero-price count overflow.
                (u64::MAX, AdmissionBase, ReturnedBytes(2)),
                (1, ReturnedBytes(u64::MAX), AdmissionBase),
                (0, ReturnedBytes(u64::MAX), ReturnedBytes(1)),
                // Summing weights within one composite step also must not overflow.
                (
                    1,
                    AdmissionBase,
                    FullRecordScan {
                        cells: 1,
                        returned_bytes: 1,
                    },
                ),
            ] {
                let schedule = Schedule::new(
                    WRITE_CODE_VERSION,
                    [
                        (W::READ_ADMISSION_BASE, 1),
                        (W::READ_CELL_SCAN, u64::MAX),
                        (W::READ_BYTE, byte_price),
                    ],
                )
                .unwrap();
                let mut meter = ReadMeter::new(&schedule, budget);
                meter.charge(first).unwrap();
                let before = meter.accounting.clone();
                let failure = meter.charge(overflowing).unwrap_err();
                assert_eq!(
                    failure.reason,
                    FailureReason::Pricing(PricingError::Overflow)
                );
                assert_eq!(failure.required, None);
                assert_eq!(failure.accounting, before);
                assert_eq!(meter.charge(ReturnedBytes(0)).unwrap_err(), failure);
                assert_eq!(meter.into_accounting(), before);
            }
        }
    }
    #[test]
    fn commit_set_requires_explicit_prices_and_rejects_all_overflows_atomically() {
        let step = CommitSet { encoded_bytes: 2 };
        for budget in [Budget::Limited(u64::MAX), Budget::Unlimited] {
            for prices in [
                vec![(W::READ_BYTE, 0)],
                vec![(W::READ_COMMIT_SET, 0)],
                vec![(W::READ_COMMIT_SET, 0), (W::READ_BYTE, u64::MAX)],
                vec![(W::READ_COMMIT_SET, u64::MAX), (W::READ_BYTE, 1)],
            ] {
                let schedule = Schedule::new(WRITE_CODE_VERSION, prices).unwrap();
                let mut meter = ReadMeter::new(&schedule, budget);
                let failure = meter.charge(step).unwrap_err();
                assert!(matches!(failure.reason, FailureReason::Pricing(_)));
                assert_eq!(failure.accounting, Accounting::default());
                assert_eq!(meter.charge(ReturnedBytes(0)).unwrap_err(), failure);
            }
        }
        let schedule = Schedule::new(
            WRITE_CODE_VERSION,
            [(W::READ_COMMIT_SET, 0), (W::READ_BYTE, 0)],
        )
        .unwrap();
        let mut meter = ReadMeter::new(&schedule, Budget::Limited(0));
        meter.charge(step).unwrap();
        assert_eq!(meter.accounting.ledger.count("read.commit_set"), 1);
        assert_eq!(meter.accounting.ledger.count("read.byte"), 2);
        meter.charge(ReturnedBytes(u64::MAX - 2)).unwrap();
        let before = meter.accounting.clone();
        assert!(meter.charge(step).is_err());
        assert_eq!(meter.into_accounting(), before);
    }
}
