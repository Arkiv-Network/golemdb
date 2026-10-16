//! [`Budget`]: how much work a query or count may do.

/// The work a call may pay for. Required on every query and count, with no
/// default. Execution stops at the first step the remaining budget cannot pay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Limited(u64),
    Unlimited,
}
