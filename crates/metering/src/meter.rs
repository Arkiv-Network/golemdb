use crate::{
    Accounting, Budget, ModeledDepths, MutationCharge, PlanningStep, PricingError, Schedule,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FailureReason {
    #[error("limited budget exceeded")]
    BudgetExceeded,
    #[error(transparent)]
    Pricing(#[from] PricingError),
}

/// Maps to OutOfBudget at the API boundary. The accounting is always completed
/// planning work; prospective mutations appear only in a successful final total.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("write metering failed: {reason} (spent: {}, required: {required:?})", accounting.cost)]
pub struct MeteringFailure {
    pub accounting: Accounting,
    pub required: Option<u64>,
    #[source]
    pub reason: FailureReason,
}

/// One call's checked accounting. The schedule is borrowed immutably for the
/// entire call. No storage access, plan construction or database identifiers.
pub struct WriteMeter<'s> {
    schedule: &'s Schedule,
    depths: ModeledDepths,
    budget: Budget,
    // Charged admission and state-dependent planning work.
    planning_accounting: Accounting,
    // Prospective mutations, included only in the approved final accounting.
    mutation_accounting: Accounting,
    failed: Option<FailureReason>,
}

impl<'s> WriteMeter<'s> {
    pub fn new(schedule: &'s Schedule, depths: ModeledDepths, budget: Budget) -> Self {
        Self {
            schedule,
            depths,
            budget,
            planning_accounting: Accounting::default(),
            mutation_accounting: Accounting::default(),
            failed: None,
        }
    }

    /// Check and record the whole charge, then invoke the action exactly once.
    /// An unfunded step invokes no action and adds no ledger entries. A domain
    /// error returned by the action is still charged; a Result action therefore
    /// gives a nested Result, consumed by the record adapter before proceeding.
    ///
    /// Recording before invoking also preserves the charge on a local fault or
    /// unwind. Those are operational failures, outside deterministic fee policy.
    /// This combined operation replaces separately issued approval tokens: no
    /// approval can be reused, moved to another meter or left uncompleted.
    pub fn run_step<T>(
        &mut self,
        step: PlanningStep,
        action: impl FnOnce() -> T,
    ) -> Result<T, MeteringFailure> {
        self.ensure_running()?;
        let next_planning = step
            .price(self.schedule)
            .and_then(|charge| self.planning_accounting.checked_merge(&charge))
            .map_err(|error| self.stop(error.into()))?;
        if self.exceeds(next_planning.cost) {
            return Err(self.stop(FailureReason::BudgetExceeded));
        }
        // Validate the combined cost and counts without charging mutations.
        // checked_merge returns a new value; neither input is modified.
        let _checked_total = next_planning
            .checked_merge(&self.mutation_accounting)
            .map_err(|error| self.stop(error.into()))?;
        self.planning_accounting = next_planning;
        Ok(action())
    }

    /// Accumulate a prospective mutation without spending it or stopping at a
    /// limited budget. Only finish knows the full required cost. Arithmetic and
    /// missing-weight failures stop immediately, even under Unlimited.
    pub fn add_planned_mutation(
        &mut self,
        mutation: MutationCharge,
    ) -> Result<(), MeteringFailure> {
        self.ensure_running()?;
        let next_mutations = mutation
            .price(self.schedule, self.depths)
            .and_then(|charge| self.mutation_accounting.checked_merge(&charge))
            .map_err(|error| self.stop(error.into()))?;
        // Validate the combined cost and counts; planning charges stay unchanged.
        let _checked_total = self
            .planning_accounting
            .checked_merge(&next_mutations)
            .map_err(|error| self.stop(error.into()))?;
        self.mutation_accounting = next_mutations;
        Ok(())
    }

    /// Finish consumes the meter, so the approved mutations cannot be charged
    /// twice. Use the returned accounting only after successful application.
    pub fn finish(self) -> Result<Accounting, MeteringFailure> {
        self.ensure_running()?;
        let total = self
            .planning_accounting
            .checked_merge(&self.mutation_accounting)
            .map_err(|error| self.failure(error.into(), None))?;
        if self.exceeds(total.cost) {
            return Err(self.failure(FailureReason::BudgetExceeded, Some(total.cost)));
        }
        Ok(total)
    }

    /// For an ordinary planning error (AlreadyExists, invalid input, etc.),
    /// discard prospective writes and return the completed work's accounting.
    pub fn into_accounting(self) -> Accounting {
        self.planning_accounting
    }

    fn exceeds(&self, cost: u64) -> bool {
        matches!(self.budget, Budget::Limited(limit) if cost > limit)
    }

    fn ensure_running(&self) -> Result<(), MeteringFailure> {
        match &self.failed {
            Some(reason) => Err(self.failure(reason.clone(), None)),
            None => Ok(()),
        }
    }

    fn stop(&mut self, reason: FailureReason) -> MeteringFailure {
        self.failed = Some(reason.clone());
        self.failure(reason, None)
    }

    fn failure(&self, reason: FailureReason, required: Option<u64>) -> MeteringFailure {
        MeteringFailure {
            accounting: self.planning_accounting.clone(),
            required,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{INITIAL_DEPTHS, RecordOpKind, WRITE_CODE_VERSION, WeightName as W};

    fn schedule() -> Schedule {
        Schedule::new(
            WRITE_CODE_VERSION,
            [
                (W::WRITE_ADMISSION_BASE, 2),
                (W::WRITE_ADMISSION_ENTRY, 1),
                (W::WRITE_ADMISSION_BYTE, 1),
                (W::READ_KEY, 1),
                (W::WRITE_RECORD_CREATE, 2),
                (W::WRITE_RECORD_PATCH, 2),
                (W::WRITE_RECORD_DELETE, 2),
                (W::READ_CELL, 2),
                (W::READ_TERM, 2),
                (W::WRITE_CELL_CREATE, 5),
                (W::WRITE_CELL_UPDATE, 6),
                (W::WRITE_CELL_DELETE, 4),
                (W::WRITE_CELL_TRIE_UPDATE, 1),
                (W::WRITE_CELL_BYTE, 1),
                (W::WRITE_INDEX_JOIN, 3),
                (W::WRITE_INDEX_LEAVE, 2),
                (W::WRITE_INDEX_TRIE_UPDATE, 1),
                (W::WRITE_INDEX_TERM_CREATE, 7),
                (W::WRITE_INDEX_BYTE, 1),
            ],
        )
        .unwrap()
    }

    // Independent conformance fixture from the plan: two user cells (age, color),
    // three system cells, one new index term. Original depth-4 example is retained.
    fn create(schedule: &Schedule, budget: Budget) -> Result<Accounting, MeteringFailure> {
        let mut meter = WriteMeter::new(
            schedule,
            ModeledDepths {
                cell_trie: 4,
                index_trie: 4,
            },
            budget,
        );
        for step in [
            PlanningStep::AdmissionBase,
            PlanningStep::AdmissionEntry,
            PlanningStep::AdmissionBytes(7),
            PlanningStep::AdmissionEntry,
            PlanningStep::AdmissionBytes(8),
            PlanningStep::RecordBase(RecordOpKind::Create),
            PlanningStep::TermRead,
        ] {
            meter.run_step(step, || ())?;
        }
        for bytes_written in [16, 17, 45, 46, 49] {
            meter.add_planned_mutation(MutationCharge::CellCreate { bytes_written })?;
        }
        meter.add_planned_mutation(MutationCharge::IndexJoin {
            bytes_written: 10,
            create_term: true,
        })?;
        meter.finish()
    }

    #[test]
    fn worked_create_prices_user_and_system_writes_and_reconstructs_from_ledger() {
        let schedule = schedule();
        let result = create(&schedule, Budget::Limited(266)).unwrap();
        assert_eq!(result.cost, 266);
        let counts: Vec<_> = result
            .ledger
            .iter()
            .map(|(name, count)| (name.as_str(), count))
            .collect();
        assert_eq!(
            counts,
            [
                ("read.key", 1),
                ("read.term", 1),
                ("write.admission.base", 1),
                ("write.admission.byte", 15),
                ("write.admission.entry", 2),
                ("write.cell.create", 5),
                ("write.cell_byte", 173),
                ("write.cell_trie_update", 20),
                ("write.index.join", 1),
                ("write.index_byte", 10),
                ("write.index_term_create", 1),
                ("write.index_trie_update", 4),
                ("write.record.create", 1),
            ]
        );
        assert_eq!(schedule.price(&result.ledger).unwrap(), result.cost);
        assert_eq!(create(&schedule, Budget::Unlimited).unwrap(), result);
    }

    #[test]
    fn cutoffs_report_only_completed_steps_and_required_only_after_full_planning() {
        let schedule = schedule();
        for (budget, spent, required) in [
            (0, 0, None),
            (1, 0, None),
            (2, 2, None),
            (3, 3, None),
            (9, 3, None),
            (10, 10, None),
            (11, 11, None),
            (18, 11, None),
            (19, 19, None),
            (21, 19, None),
            (22, 22, None),
            (23, 22, None),
            (24, 24, Some(266)),
            (265, 24, Some(266)),
        ] {
            let failure = create(&schedule, Budget::Limited(budget)).unwrap_err();
            assert_eq!(failure.accounting.cost, spent, "budget {budget}");
            assert_eq!(failure.required, required, "budget {budget}");
            assert_eq!(failure.reason, FailureReason::BudgetExceeded);
            assert_eq!(schedule.price(&failure.accounting.ledger).unwrap(), spent);
            assert!(
                failure
                    .accounting
                    .ledger
                    .iter()
                    .all(|(name, _)| !name.as_str().starts_with("write.cell")
                        && !name.as_str().starts_with("write.index"))
            );
            assert_eq!(
                failure.accounting.ledger.count("read.key"),
                u64::from(spent >= 22)
            );
            assert_eq!(
                failure.accounting.ledger.count("write.record.create"),
                u64::from(spent >= 22)
            );
            assert_eq!(
                failure.accounting.ledger.count("read.term"),
                u64::from(spent == 24)
            );
        }
    }

    #[test]
    fn composite_base_is_atomic_and_failure_prevents_any_later_work() {
        let schedule = schedule();
        let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Limited(4));
        meter.run_step(PlanningStep::AdmissionBase, || ()).unwrap();
        let failure = meter
            .run_step(PlanningStep::RecordBase(RecordOpKind::Create), || {
                panic!("unfunded lookup")
            })
            .unwrap_err();
        assert_eq!(failure.accounting.cost, 2);
        assert_eq!(failure.accounting.ledger.count("read.key"), 0);
        assert_eq!(failure.accounting.ledger.count("write.record.create"), 0);
        assert_eq!(
            meter
                .run_step(PlanningStep::AdmissionEntry, || panic!(
                    "continued after failure"
                ))
                .unwrap_err(),
            failure
        );
        assert_eq!(
            meter
                .add_planned_mutation(MutationCharge::CellDelete)
                .unwrap_err(),
            failure
        );
        assert_eq!(meter.finish().unwrap_err(), failure);
    }

    #[test]
    fn rejected_validation_is_charged_and_domain_failure_discards_planned_writes() {
        let schedule = schedule();
        let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Unlimited);
        let error = meter
            .run_step(PlanningStep::AdmissionBase, || Err::<(), _>("invalid key"))
            .unwrap();
        assert_eq!(error, Err("invalid key"));
        assert_eq!(meter.into_accounting().cost, 2);

        let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Unlimited);
        meter.run_step(PlanningStep::AdmissionBase, || ()).unwrap();
        meter
            .run_step(PlanningStep::RecordBase(RecordOpKind::Patch), || ())
            .unwrap();
        meter
            .add_planned_mutation(MutationCharge::CellCreate { bytes_written: 100 })
            .unwrap();
        let error = meter
            .run_step(PlanningStep::CellRead, || Err::<(), _>("cell not found"))
            .unwrap();
        assert_eq!(error, Err("cell not found"));
        let accounting = meter.into_accounting();
        assert_eq!(accounting.cost, 7);
        assert_eq!(accounting.ledger.count("read.cell"), 1);
        assert_eq!(accounting.ledger.count("write.cell.create"), 0);
    }

    #[test]
    fn zero_weights_allow_zero_budget_but_still_record_work() {
        let weights = schedule();
        let free = Schedule::new(
            WRITE_CODE_VERSION,
            weights.weights().map(|(name, _)| (name.clone(), 0)),
        )
        .unwrap();
        let result = create(&free, Budget::Limited(0)).unwrap();
        assert_eq!(result.cost, 0);
        assert_eq!(
            result.ledger,
            create(&weights, Budget::Unlimited).unwrap().ledger
        );
    }

    #[test]
    fn missing_weights_fail_only_when_used_even_with_unlimited_budget() {
        let full = schedule();
        for budget in [Budget::Limited(u64::MAX), Budget::Unlimited] {
            for missing in [
                W::READ_KEY,
                W::WRITE_RECORD_CREATE,
                W::WRITE_INDEX_TERM_CREATE,
            ] {
                let partial = Schedule::new(
                    WRITE_CODE_VERSION,
                    full.weights()
                        .filter(|(name, _)| **name != missing)
                        .map(|(name, price)| (name.clone(), price)),
                )
                .unwrap();
                let failure = create(&partial, budget).unwrap_err();
                assert_eq!(failure.required, None);
                assert_eq!(
                    failure.accounting.cost,
                    if missing == W::WRITE_INDEX_TERM_CREATE {
                        24
                    } else {
                        19
                    }
                );
                assert_eq!(
                    failure.reason,
                    FailureReason::Pricing(PricingError::Unpriced(missing))
                );
                assert_eq!(failure.accounting.ledger.count("write.cell.create"), 0);
            }
        }
    }

    #[test]
    fn arithmetic_overflow_fails_under_both_budgets_without_partial_charge() {
        for budget in [Budget::Limited(u64::MAX), Budget::Unlimited] {
            let schedule = Schedule::new(
                WRITE_CODE_VERSION,
                [
                    (W::WRITE_ADMISSION_BASE, 2),
                    (W::WRITE_ADMISSION_BYTE, u64::MAX),
                ],
            )
            .unwrap();
            let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, budget);
            meter.run_step(PlanningStep::AdmissionBase, || ()).unwrap();
            let failure = meter
                .run_step(PlanningStep::AdmissionBytes(2), || {
                    panic!("overflowed charge")
                })
                .unwrap_err();
            assert_eq!(failure.accounting.cost, 2);
            assert_eq!(failure.accounting.ledger.count("write.admission.byte"), 0);
            assert_eq!(failure.required, None);
            assert_eq!(
                failure.reason,
                FailureReason::Pricing(PricingError::Overflow)
            );
            assert_eq!(meter.finish().unwrap_err(), failure);
        }
    }

    #[test]
    fn zero_price_does_not_hide_ledger_count_overflow() {
        let schedule = Schedule::new(WRITE_CODE_VERSION, [(W::WRITE_ADMISSION_BYTE, 0)]).unwrap();
        let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Unlimited);
        meter
            .run_step(PlanningStep::AdmissionBytes(u64::MAX), || ())
            .unwrap();
        let failure = meter
            .run_step(PlanningStep::AdmissionBytes(1), || panic!("count overflow"))
            .unwrap_err();
        assert_eq!(failure.accounting.cost, 0);
        assert_eq!(
            failure.accounting.ledger.count("write.admission.byte"),
            u64::MAX
        );
        assert_eq!(
            failure.reason,
            FailureReason::Pricing(PricingError::Overflow)
        );
    }

    #[test]
    fn overflow_of_combined_planning_and_mutations_never_becomes_a_required_total() {
        for budget in [Budget::Limited(u64::MAX), Budget::Unlimited] {
            let schedule = Schedule::new(
                WRITE_CODE_VERSION,
                [
                    (W::WRITE_ADMISSION_BASE, 1),
                    (W::WRITE_CELL_DELETE, u64::MAX),
                    (W::WRITE_CELL_TRIE_UPDATE, 0),
                ],
            )
            .unwrap();
            let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, budget);
            meter.run_step(PlanningStep::AdmissionBase, || ()).unwrap();
            let failure = meter
                .add_planned_mutation(MutationCharge::CellDelete)
                .unwrap_err();
            assert_eq!(failure.accounting.cost, 1);
            assert_eq!(failure.required, None);
            assert_eq!(
                failure.reason,
                FailureReason::Pricing(PricingError::Overflow)
            );
            assert_eq!(meter.finish().unwrap_err(), failure);
            // The same overflow must be caught if the mutation is priced first.
            let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, budget);
            meter
                .add_planned_mutation(MutationCharge::CellDelete)
                .unwrap();
            let failure = meter
                .run_step(PlanningStep::AdmissionBase, || {
                    panic!("overflow after planning")
                })
                .unwrap_err();
            assert_eq!(failure.accounting.cost, 0);
            assert_eq!(
                failure.reason,
                FailureReason::Pricing(PricingError::Overflow)
            );
        }
    }

    #[test]
    fn planning_may_continue_when_only_the_prospective_mutations_exceed_budget() {
        let schedule = schedule();
        let mut meter = WriteMeter::new(&schedule, INITIAL_DEPTHS, Budget::Limited(2));
        meter
            .add_planned_mutation(MutationCharge::CellCreate { bytes_written: 10 })
            .unwrap();
        meter.run_step(PlanningStep::CellRead, || ()).unwrap();
        let failure = meter.finish().unwrap_err();
        assert_eq!(failure.accounting.cost, 2);
        assert_eq!(failure.required, Some(23)); // read 2 + create 5 + path 6 + bytes 10
        assert_eq!(failure.accounting.ledger.count("read.cell"), 1);
        assert_eq!(failure.accounting.ledger.count("write.cell.create"), 0);
    }
}
