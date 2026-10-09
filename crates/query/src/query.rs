//! The query object: what the caller asks for, before any validation.

use crate::Value;

/// A query over indexed cells.
///
/// Built directly by the caller; there is no query string. Validation happens
/// when the query is executed, so a `Query` is plain data.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// Records matching one predicate.
    Predicate(Predicate),
}

impl From<Predicate> for Query {
    fn from(predicate: Predicate) -> Self {
        Self::Predicate(predicate)
    }
}

/// `field op value`, typed by `value`: a cell of another type under the same
/// name is treated as absent.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub field: String,
    pub op: Op,
    pub value: Value,
}

impl Predicate {
    pub fn eq(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Self {
            field: field.into(),
            op: Op::Eq,
            value: value.into(),
        }
    }
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// The cell holds exactly this value, with this type.
    Eq,
}
