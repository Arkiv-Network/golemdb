//! Pure planning: turn a predicate into the index lookup that answers it.

use golemdb_index::IndexTerm;

use crate::{Op, Predicate, QueryError, Result};

/// The index work one predicate needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// The postings of one exact term.
    Term(IndexTerm),
}

pub(crate) fn plan(predicate: &Predicate) -> Result<Lookup> {
    let Predicate { field, op, value } = predicate;
    let term = value
        .term(field)
        .map_err(|source| QueryError::InvalidPredicate {
            field: field.clone(),
            source,
        })?;
    match op {
        Op::Eq => Ok(Lookup::Term(term)),
    }
}
