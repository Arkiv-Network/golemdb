//! [`Budget`]: the shared explicit allowance for a record, query or count call.

/// The work a call may pay for, with no implicit default. Requests currently
/// pass this allowance without enforcing it until record planner integration.
/// [`crate::WriteMeter`] enforces it for supplied logical steps now. Unlimited
/// skips budget comparisons only, not missing weights or arithmetic checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Limited(u64),
    Unlimited,
}
