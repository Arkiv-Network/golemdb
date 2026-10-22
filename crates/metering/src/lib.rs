//! Write pricing and per-call accounting, independent of storage and record plans.
//!
//! A [`WriteMeter`] takes one immutable [`Schedule`], explicit [`ModeledDepths`]
//! and a [`Budget`]. [`WriteMeter::run_step`] checks and charges a complete planning
//! step before invoking its action, including actions that return a domain error.
//! Prospective mutations stay separate until [`WriteMeter::finish`] approves the
//! whole cost. A failure reports only planning work charged so far.
//!
//! Record execution will supply these logical steps in iteration 4. Public CRUD
//! calls still return zero cost until that integration and production scheduling
//! are enabled. Read-call pricing, genesis persistence and model activation are
//! outside this crate's current execution scope.

mod budget;
mod ledger;
mod meter;
mod pricing;
mod schedule;
mod weight;

pub use budget::Budget;
pub use ledger::{Accounting, Ledger};
pub use meter::{FailureReason, MeteringFailure, WriteMeter};
pub use pricing::{INITIAL_DEPTHS, ModeledDepths, MutationCharge, PlanningStep, RecordOpKind};
pub use schedule::{PricingError, Schedule, ScheduleError, WRITE_CODE_VERSION};
pub use weight::WeightName;
