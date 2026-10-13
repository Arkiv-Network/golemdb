//! Plan evaluation over any [`PostingSource`]. Pure; reads only through the
//! source, so it is testable without storage.

use golemdb_index::Bitmap;

use crate::{Result, plan::Plan, source::PostingSource};

/// The match set of an admitted filter: the IDs of every matching record,
/// ascending. Fails only when the source does.
pub(crate) fn match_set(source: &impl PostingSource, plan: &Plan) -> Result<Bitmap> {
    match plan {
        Plan::TermRead(term) => Ok(source.postings(term)?),
        Plan::TermReads(terms) => terms.iter().try_fold(Bitmap::default(), |mut union, term| {
            union |= source.postings(term)?;
            Ok(union)
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use golemdb_cells::{CellKind, CellName, CellNameRef, CellType, CellValue};
    use golemdb_index::{BitmapContainer, IndexError, IndexTerm, TermError, path};

    use super::*;
    use crate::{Filter, QueryError};

    /// Posting lists in a `BTreeMap`: byte-ordered like the `Index` table.
    #[derive(Default)]
    struct FakeSource(BTreeMap<IndexTerm, Bitmap>);

    impl FakeSource {
        /// Index `ids` under `field = value` the way the write path does: as
        /// an attribute cell, through `IndexTerm::from_cell`.
        fn with(
            mut self,
            field: &str,
            value: CellValue,
            ids: impl IntoIterator<Item = u64>,
        ) -> Self {
            let attribute = value
                .with_kind(CellKind::Attribute)
                .expect("indexable type");
            let term = IndexTerm::from_cell(field, attribute.as_view())
                .expect("valid term")
                .expect("attribute");
            self.0.insert(term, bitmap(ids));
            self
        }
    }

    /// Chunk record IDs by their 48-bit path, as the BitmapTrie stores them.
    fn bitmap(ids: impl IntoIterator<Item = u64>) -> Bitmap {
        let mut chunks: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
        for id in ids {
            let (path, offset) = path::split(id);
            chunks.entry(path).or_default().push(offset);
        }
        Bitmap::from_containers(chunks.into_iter().map(|(path, offsets)| {
            BitmapContainer::from_values(path, offsets).expect("48-bit path")
        }))
        .expect("unique, ordered paths")
    }

    impl PostingSource for FakeSource {
        fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            Ok(self.0.get(term).cloned().unwrap_or_default())
        }
    }

    /// A [`FakeSource`] that records every term it is asked for, in order.
    struct RecordingSource {
        inner: FakeSource,
        reads: RefCell<Vec<IndexTerm>>,
    }

    impl PostingSource for RecordingSource {
        fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            self.reads.borrow_mut().push(term.clone());
            self.inner.postings(term)
        }
    }

    /// A source whose every read fails.
    struct FailingSource;

    impl PostingSource for FailingSource {
        fn postings(&self, _: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            Err(IndexError::Corruption("injected"))
        }
    }

    /// Raw, so tests can also build names the grammar rejects.
    fn name(field: &[u8]) -> CellName {
        CellNameRef::raw(field).into()
    }

    fn eq(field: &str, value: CellValue) -> Filter {
        Filter::Eq {
            field: name(field.as_bytes()),
            value,
        }
    }

    fn is_in(field: &str, values: impl IntoIterator<Item = CellValue>) -> Filter {
        Filter::In {
            field: name(field.as_bytes()),
            values: values.into_iter().collect(),
        }
    }

    fn str(value: &str) -> CellValue {
        CellValue::from_str(value)
    }

    /// Admission, then evaluation: the sequence `query` and `count` follow.
    fn evaluate(source: &impl PostingSource, filter: &Filter) -> Result<Bitmap> {
        match_set(source, &Plan::try_from(filter)?)
    }

    fn ids(source: &FakeSource, filter: Filter) -> Vec<u64> {
        evaluate(source, &filter)
            .expect("filter evaluates")
            .treemap()
            .iter()
            .collect()
    }

    #[test]
    fn equality_returns_the_term_postings() {
        let source = FakeSource::default()
            .with("color", str("blue"), [1, 5, 1 << 40])
            .with("color", str("red"), [2]);
        assert_eq!(ids(&source, eq("color", str("blue"))), [1, 5, 1 << 40]);
        assert_eq!(ids(&source, eq("color", str("red"))), [2]);
    }

    #[test]
    fn absent_term_matches_nothing() {
        let source = FakeSource::default().with("color", str("blue"), [1]);
        assert!(ids(&source, eq("color", str("green"))).is_empty());
        assert!(ids(&source, eq("shape", str("blue"))).is_empty());
        assert!(ids(&FakeSource::default(), eq("color", str("blue"))).is_empty());
    }

    #[test]
    fn other_types_under_the_same_name_are_absent() {
        let source = FakeSource::default()
            .with("amount", CellValue::from_i32(100), [1])
            .with("amount", CellValue::from_u32(100), [2])
            .with("amount", CellValue::from_dec32_unscaled(100), [3])
            .with("amount", str("100"), [4])
            .with("amount", CellValue::from_i64(100), [5]);
        assert_eq!(ids(&source, eq("amount", CellValue::from_i32(100))), [1]);
        assert_eq!(ids(&source, eq("amount", CellValue::from_u32(100))), [2]);
        assert_eq!(
            ids(&source, eq("amount", CellValue::from_dec32_unscaled(100))),
            [3]
        );
        assert_eq!(ids(&source, eq("amount", str("100"))), [4]);
        assert_eq!(ids(&source, eq("amount", CellValue::from_i64(100))), [5]);
        assert!(ids(&source, eq("amount", CellValue::from_u64(100))).is_empty());
    }

    #[test]
    fn names_sharing_a_prefix_are_distinct() {
        let source = FakeSource::default()
            .with("a", str("x"), [1])
            .with("ab", str("x"), [2])
            .with("a", str("bx"), [3]);
        assert_eq!(ids(&source, eq("a", str("x"))), [1]);
        assert_eq!(ids(&source, eq("ab", str("x"))), [2]);
        assert_eq!(ids(&source, eq("a", str("bx"))), [3]);
    }

    #[test]
    fn the_value_kind_plays_no_part() {
        let source = FakeSource::default().with("a", CellValue::from_u64(7), [1]);
        let attribute = CellValue::from_u64(7)
            .with_kind(CellKind::Attribute)
            .expect("indexable type");
        assert_eq!(ids(&source, eq("a", CellValue::from_u64(7))), [1]);
        assert_eq!(ids(&source, eq("a", attribute)), [1]);
    }

    #[test]
    fn in_matches_the_union_of_its_values() {
        let source = FakeSource::default()
            .with("color", str("blue"), [1, 40])
            .with("color", str("red"), [2, 40])
            .with("color", str("green"), [3]);
        assert_eq!(
            ids(&source, is_in("color", [str("red"), str("blue")])),
            [1, 2, 40]
        );
        assert_eq!(ids(&source, is_in("color", [str("green")])), [3]);
        assert_eq!(
            ids(&source, is_in("color", [str("pink"), str("green")])),
            [3]
        );
        assert!(ids(&source, is_in("color", [str("pink")])).is_empty());
    }

    #[test]
    fn in_values_are_typed() {
        let source = FakeSource::default()
            .with("amount", CellValue::from_i32(1), [1])
            .with("amount", CellValue::from_u32(1), [2])
            .with("amount", CellValue::from_i32(2), [3]);
        let i32s = [CellValue::from_i32(1), CellValue::from_i32(2)];
        assert_eq!(ids(&source, is_in("amount", i32s)), [1, 3]);
        let u32s = [CellValue::from_u32(1), CellValue::from_u32(2)];
        assert_eq!(ids(&source, is_in("amount", u32s)), [2]);
    }

    #[test]
    fn in_reads_each_distinct_term_once_in_key_order() {
        let source = RecordingSource {
            inner: FakeSource::default().with("n", CellValue::from_i32(-1), [1]),
            reads: RefCell::default(),
        };
        let values = [1, -1, 1, 0, -1].map(CellValue::from_i32);
        let matches = evaluate(&source, &is_in("n", values)).expect("filter evaluates");
        assert_eq!(matches.treemap().iter().collect::<Vec<_>>(), [1]);
        let term = |n| IndexTerm::for_value("n", CellValue::from_i32(n).as_view()).expect("term");
        assert_eq!(*source.reads.borrow(), [term(-1), term(0), term(1)]);
    }

    #[test]
    fn malformed_in_is_rejected_before_reading() {
        for filter in [
            is_in("a", []),
            is_in("a", [CellValue::from_i32(1), CellValue::from_u32(1)]),
        ] {
            assert!(
                matches!(
                    evaluate(&FailingSource, &filter),
                    Err(QueryError::InvalidQuery { .. })
                ),
                "{filter:?}"
            );
        }
    }

    #[test]
    fn invalid_arguments_are_rejected_before_reading() {
        // FailingSource would surface as QueryError::Internal if it were consulted.
        let not_utf8 = Filter::Eq {
            field: name(&[0xff]),
            value: CellValue::from_i32(1),
        };
        for (filter, field, expected) in [
            (
                eq("1bad", CellValue::from_i32(1)),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            (not_utf8, name(&[0xff]), TermError::InvalidName),
            (
                eq("b", CellValue::from_bytes(b"x")),
                name(b"b"),
                TermError::UnsupportedType(CellType::Bytes),
            ),
            (
                is_in("1bad", [CellValue::from_i32(1)]),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            (
                is_in("b", [CellValue::from_bytes(b"x")]),
                name(b"b"),
                TermError::UnsupportedType(CellType::Bytes),
            ),
        ] {
            match evaluate(&FailingSource, &filter) {
                Err(QueryError::InvalidArgument { field: got, source }) => {
                    assert_eq!(got, field);
                    assert_eq!(source, expected);
                }
                other => panic!("expected InvalidArgument, got {other:?}"),
            }
        }
    }

    #[test]
    fn source_errors_propagate() {
        for filter in [
            eq("a", CellValue::from_i32(1)),
            is_in("a", [CellValue::from_i32(1), CellValue::from_i32(2)]),
        ] {
            let result = evaluate(&FailingSource, &filter);
            assert!(matches!(
                result,
                Err(QueryError::Internal(source))
                    if matches!(source.downcast_ref(), Some(IndexError::Corruption("injected")))
            ));
        }
    }
}
