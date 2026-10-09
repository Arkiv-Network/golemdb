//! Query evaluation over any [`PostingSource`].

use roaring::RoaringTreemap;

use crate::{
    PostingSource, Query, Result,
    plan::{Lookup, plan},
};

/// The IDs of every record matching `query`, in ascending order.
///
/// # Errors
///
/// [`QueryError::InvalidPredicate`](crate::QueryError::InvalidPredicate) for a
/// predicate with no valid index term; [`QueryError::Index`](crate::QueryError::Index)
/// when the source fails.
pub fn execute(source: &impl PostingSource, query: &Query) -> Result<RoaringTreemap> {
    match query {
        Query::Predicate(predicate) => match plan(predicate)? {
            Lookup::Term(term) => Ok(source.postings(&term)?),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use golemdb_index::{IndexError, IndexTerm, TermError};

    use super::*;
    use crate::{Predicate, QueryError, Value};

    /// Posting lists in a `BTreeMap`: byte-ordered like the `Index` table.
    #[derive(Default)]
    struct FakeSource(BTreeMap<IndexTerm, RoaringTreemap>);

    impl FakeSource {
        fn with(mut self, term: IndexTerm, ids: impl IntoIterator<Item = u64>) -> Self {
            self.0.entry(term).or_default().extend(ids);
            self
        }
    }

    impl PostingSource for FakeSource {
        fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<RoaringTreemap> {
            Ok(self.0.get(term).cloned().unwrap_or_default())
        }
    }

    /// A source whose every read fails.
    struct FailingSource;

    impl PostingSource for FailingSource {
        fn postings(&self, _: &IndexTerm) -> golemdb_index::Result<RoaringTreemap> {
            Err(IndexError::Corruption("injected"))
        }
    }

    fn term(field: &str, value: impl Into<Value>) -> IndexTerm {
        value.into().term(field).expect("valid term")
    }

    fn ids(source: &FakeSource, predicate: Predicate) -> Vec<u64> {
        execute(source, &Query::from(predicate))
            .expect("query runs")
            .iter()
            .collect()
    }

    #[test]
    fn equality_returns_the_term_postings() {
        let source = FakeSource::default()
            .with(term("color", "blue"), [1, 5, 1 << 40])
            .with(term("color", "red"), [2]);
        assert_eq!(
            ids(&source, Predicate::eq("color", "blue")),
            [1, 5, 1 << 40]
        );
        assert_eq!(ids(&source, Predicate::eq("color", "red")), [2]);
    }

    #[test]
    fn absent_term_matches_nothing() {
        let source = FakeSource::default().with(term("color", "blue"), [1]);
        assert!(ids(&source, Predicate::eq("color", "green")).is_empty());
        assert!(ids(&source, Predicate::eq("shape", "blue")).is_empty());
        assert!(ids(&FakeSource::default(), Predicate::eq("color", "blue")).is_empty());
    }

    #[test]
    fn other_types_under_the_same_name_are_absent() {
        let source = FakeSource::default()
            .with(term("amount", Value::I32(100)), [1])
            .with(term("amount", Value::U32(100)), [2])
            .with(term("amount", Value::Dec32(100)), [3])
            .with(term("amount", "100"), [4])
            .with(term("amount", Value::I64(100)), [5]);
        assert_eq!(ids(&source, Predicate::eq("amount", Value::I32(100))), [1]);
        assert_eq!(ids(&source, Predicate::eq("amount", Value::U32(100))), [2]);
        assert_eq!(
            ids(&source, Predicate::eq("amount", Value::Dec32(100))),
            [3]
        );
        assert_eq!(ids(&source, Predicate::eq("amount", "100")), [4]);
        assert_eq!(ids(&source, Predicate::eq("amount", Value::I64(100))), [5]);
        assert!(ids(&source, Predicate::eq("amount", Value::U64(100))).is_empty());
    }

    #[test]
    fn names_sharing_a_prefix_are_distinct() {
        let source = FakeSource::default()
            .with(term("a", "x"), [1])
            .with(term("ab", "x"), [2])
            .with(term("a", "bx"), [3]);
        assert_eq!(ids(&source, Predicate::eq("a", "x")), [1]);
        assert_eq!(ids(&source, Predicate::eq("ab", "x")), [2]);
        assert_eq!(ids(&source, Predicate::eq("a", "bx")), [3]);
    }

    #[test]
    fn negative_zero_finds_zero() {
        let source = FakeSource::default().with(term("t", 0.0f64), [7]);
        assert_eq!(ids(&source, Predicate::eq("t", -0.0f64)), [7]);
    }

    #[test]
    fn invalid_predicate_is_rejected_before_reading() {
        // FailingSource would surface as QueryError::Index if it were consulted.
        for (predicate, expected) in [
            (Predicate::eq("1bad", 1i32), TermError::InvalidName),
            (Predicate::eq("t", f64::NAN), TermError::NaN),
        ] {
            let field = predicate.field.clone();
            match execute(&FailingSource, &Query::from(predicate)) {
                Err(QueryError::InvalidPredicate { field: got, source }) => {
                    assert_eq!(got, field);
                    assert_eq!(source, expected);
                }
                other => panic!("expected InvalidPredicate, got {other:?}"),
            }
        }
    }

    #[test]
    fn source_errors_propagate() {
        let result = execute(&FailingSource, &Query::from(Predicate::eq("a", 1i32)));
        assert!(matches!(
            result,
            Err(QueryError::Index(IndexError::Corruption("injected")))
        ));
    }
}
