//! Shared metering types for record, query and count operations.
//!
//! This crate currently defines the explicit [`Budget`] allowance. Pricing and
//! budget enforcement will be added with the metering implementation.

mod budget;

pub use budget::Budget;
