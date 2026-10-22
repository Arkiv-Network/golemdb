//! [`Budget`]: the shared explicit allowance for a record, query or count call.

/// The work a call may pay for, with no implicit default. Requests currently
/// store this allowance without enforcing it; operation pricing is deferred.
/// Unlimited skips only the future cost cap, not validation or arithmetic checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Limited(u64),
    Unlimited,
}
