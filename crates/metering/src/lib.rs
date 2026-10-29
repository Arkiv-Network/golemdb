//! Pricing models, persisted schedules and per-call accounting.
//!
//! [`PricingProvider`] loads reserved cells through a caller-selected committed
//! reader. Its immutable cache is shared by API inspection and CUD execution.
//! It never selects head, accesses an overlay or constructs a plan.
//! The calculation engine ([`WriteMeter`]) remains independent of storage.
//!
//! A [`WriteMeter`] takes one immutable [`Schedule`], explicit [`ModeledDepths`]
//! and a [`Budget`]. [`WriteMeter::run_step`] checks and charges a complete planning
//! step before invoking its action, including actions that return a domain error.
//! Prospective mutations stay separate until [`WriteMeter::finish`] approves the
//! whole cost. A failure reports only planning work charged so far.
//!
//! Record planning supplies these logical steps directly. Public write calls
//! use a persisted snapshot captured at their validated branch origin and return
//! full ledgers, including on failure. Public read pricing and runtime model
//! activation remain outside this crate's current execution scope.

mod budget;
mod ledger;
mod meter;
mod model;
mod persisted;
mod pricing;
mod provider;
mod schedule;
mod weight;

pub use budget::Budget;
pub use ledger::{Accounting, Ledger};
pub use meter::{FailureReason, MeteringFailure, WriteMeter};
pub use model::{HostMeteringSchema, ModelError, ModelMetadata, PricingConfig};
pub use persisted::model_cells;
pub use pricing::{INITIAL_DEPTHS, ModeledDepths, MutationCharge, PlanningStep, RecordOpKind};
pub use provider::{PricingModel, PricingProvider, PricingSnapshot};
pub use schedule::{PricingError, Schedule, ScheduleError, WRITE_CODE_VERSION};
pub use weight::{GOLEMDB_WEIGHT_NAMES, WeightName};

#[cfg(test)]
mod tests;
