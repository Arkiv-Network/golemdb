//! Plan evaluation over any [`PostingSource`]. Pure; reads only through the
//! source, so it is testable without storage.

use golemdb_index::Bitmap;

use crate::{Result, plan::Plan, posting_source::PostingSource};

/// The match set of an admitted filter: the IDs of every matching record,
/// ascending. Consumes the plan. Fails only when the source does.
pub(crate) fn match_set(source: &impl PostingSource, plan: Plan) -> Result<Bitmap> {
    match plan {
        Plan::TermRead(term) => Ok(source.postings(&term)?),
        Plan::TermReads(terms) => union(terms.iter().map(|term| source.postings(term))),
        Plan::Scan(range) => union(source.scan(range)?),
        Plan::Empty => Ok(Bitmap::default()),
    }
}

/// The union of `postings`, read one after another in the order given.
fn union(mut postings: impl Iterator<Item = golemdb_index::Result<Bitmap>>) -> Result<Bitmap> {
    postings.try_fold(Bitmap::default(), |mut union, postings| {
        union |= postings?;
        Ok(union)
    })
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        ops::{Bound, RangeBounds},
    };

    use golemdb_cells::{CellKind, CellName, CellNameRef, CellType, CellValue};
    use golemdb_index::{BitmapContainer, IndexError, IndexTerm, TermError, TermRange, path};

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

    impl FakeSource {
        /// The entries in `range`, in key order, as a storage scan yields them.
        fn run(&self, range: TermRange) -> impl Iterator<Item = (&IndexTerm, &Bitmap)> {
            self.0
                .iter()
                .filter(move |(term, _)| range.bounds().contains(term.as_bytes()))
        }
    }

    impl PostingSource for FakeSource {
        fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            Ok(self.0.get(term).cloned().unwrap_or_default())
        }

        fn scan(
            &self,
            range: TermRange,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Ok(self.run(range).map(|(_, postings)| Ok(postings.clone())))
        }
    }

    /// A [`FakeSource`] that records every term it reads, in order: asked
    /// for, or stepped to by a scan.
    struct RecordingSource {
        inner: FakeSource,
        reads: RefCell<Vec<IndexTerm>>,
    }

    impl PostingSource for RecordingSource {
        fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            self.reads.borrow_mut().push(term.clone());
            self.inner.postings(term)
        }

        fn scan(
            &self,
            range: TermRange,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Ok(self.inner.run(range).map(|(term, postings)| {
                self.reads.borrow_mut().push(term.clone());
                Ok(postings.clone())
            }))
        }
    }

    /// A source whose every read fails. A scan fails at its first term.
    struct FailingSource;

    impl PostingSource for FailingSource {
        fn postings(&self, _: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            Err(IndexError::Corruption("injected"))
        }

        fn scan(
            &self,
            _: TermRange,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Ok(injected_failure())
        }
    }

    /// A [`FakeSource`] whose scans read `good` terms, then fail.
    struct FailingMidScan {
        inner: FakeSource,
        good: usize,
    }

    impl PostingSource for FailingMidScan {
        fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            self.inner.postings(term)
        }

        fn scan(
            &self,
            range: TermRange,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Ok(self
                .inner
                .scan(range)?
                .take(self.good)
                .chain(injected_failure()))
        }
    }

    /// The injected read error, then a panic if a scan is stepped past it.
    fn injected_failure() -> impl Iterator<Item = golemdb_index::Result<Bitmap>> {
        let past_the_error = std::iter::from_fn(|| panic!("scan stepped past an error"));
        std::iter::once(Err(IndexError::Corruption("injected"))).chain(past_the_error)
    }

    fn is_injected_failure(result: &Result<Bitmap>) -> bool {
        matches!(
            result,
            Err(QueryError::Internal(source))
                if matches!(source.downcast_ref(), Some(IndexError::Corruption("injected")))
        )
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

    fn range(field: &str, lower: Bound<CellValue>, upper: Bound<CellValue>) -> Filter {
        Filter::Range {
            field: name(field.as_bytes()),
            lower,
            upper,
        }
    }

    fn str(value: &str) -> CellValue {
        CellValue::from_str(value)
    }

    /// Admission, then evaluation: the sequence `query` and `count` follow.
    fn evaluate(source: &impl PostingSource, filter: &Filter) -> Result<Bitmap> {
        match_set(source, Plan::try_from(filter)?)
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
        let term =
            |n| IndexTerm::from_str_name("n", CellValue::from_i32(n).as_view()).expect("term");
        assert_eq!(*source.reads.borrow(), [term(-1), term(0), term(1)]);
    }

    fn i32s(lower: Bound<i32>, upper: Bound<i32>) -> Filter {
        range(
            "n",
            lower.map(CellValue::from_i32),
            upper.map(CellValue::from_i32),
        )
    }

    #[test]
    fn range_matches_the_union_of_the_terms_between_its_bounds() {
        let source = [-3, -1, 0, 1, 4]
            .into_iter()
            .fold(FakeSource::default(), |source, n| {
                source.with("n", CellValue::from_i32(n), [n.unsigned_abs().into(), 10])
            });
        use Bound::{Excluded, Included, Unbounded};
        for (filter, expected) in [
            (i32s(Included(-1), Included(1)), &[0, 1, 10][..]),
            (i32s(Excluded(-1), Included(1)), &[0, 1, 10]),
            (i32s(Excluded(-1), Excluded(1)), &[0, 10]),
            (i32s(Included(-2), Included(2)), &[0, 1, 10]),
            (i32s(Included(2), Included(3)), &[]),
            (i32s(Included(0), Included(0)), &[0, 10]),
            (i32s(Unbounded, Excluded(0)), &[1, 3, 10]),
            (i32s(Unbounded, Included(-4)), &[]),
            (i32s(Excluded(1), Unbounded), &[4, 10]),
            // Bounds that admit no value: well-formed, and empty.
            (i32s(Included(1), Excluded(1)), &[]),
            (i32s(Excluded(1), Included(1)), &[]),
            (i32s(Excluded(1), Excluded(1)), &[]),
            (i32s(Included(1), Included(-1)), &[]),
            (i32s(Excluded(4), Excluded(-3)), &[]),
            (
                i32s(Included(i32::MIN), Included(i32::MAX)),
                &[0, 1, 3, 4, 10],
            ),
        ] {
            assert_eq!(ids(&source, filter.clone()), expected, "{filter:?}");
        }
    }

    #[test]
    fn range_reads_only_its_run_in_key_order() {
        let inner = FakeSource::default()
            .with("n", CellValue::from_i32(-1), [1])
            .with("n", CellValue::from_i32(1), [2])
            .with("n", CellValue::from_i32(i32::MIN), [3])
            .with("n", CellValue::from_u32(0), [4])
            .with("n", CellValue::from_i64(0), [5])
            .with("nn", CellValue::from_i32(0), [6])
            .with("m", CellValue::from_i32(0), [7]);
        let source = RecordingSource {
            inner,
            reads: RefCell::default(),
        };
        let filter = i32s(Bound::Unbounded, Bound::Included(i32::MAX));
        let matches = evaluate(&source, &filter).expect("filter evaluates");
        assert_eq!(matches.treemap().iter().collect::<Vec<_>>(), [1, 2, 3]);
        let term =
            |n| IndexTerm::from_str_name("n", CellValue::from_i32(n).as_view()).expect("term");
        assert_eq!(*source.reads.borrow(), [term(i32::MIN), term(-1), term(1)]);
    }

    #[test]
    fn float_ranges_span_infinities_and_one_zero() {
        let float = |value: f64| CellValue::from_f64(value).expect("not NaN");
        let source = FakeSource::default()
            .with("x", float(f64::NEG_INFINITY), [1])
            .with("x", float(-1.5), [2])
            .with("x", float(-0.0), [3])
            .with("x", float(f64::MIN_POSITIVE), [4])
            .with("x", float(f64::INFINITY), [5]);
        let floats =
            |lower: Bound<f64>, upper: Bound<f64>| range("x", lower.map(float), upper.map(float));
        use Bound::{Excluded, Included, Unbounded};
        for (filter, expected) in [
            (
                floats(Included(f64::NEG_INFINITY), Included(f64::INFINITY)),
                &[1, 2, 3, 4, 5][..],
            ),
            (
                floats(Excluded(f64::NEG_INFINITY), Excluded(f64::INFINITY)),
                &[2, 3, 4],
            ),
            // -0.0 and 0.0 are one value.
            (floats(Included(0.0), Included(0.0)), &[3]),
            (floats(Excluded(-0.0), Unbounded), &[4, 5]),
            (floats(Unbounded, Excluded(0.0)), &[1, 2]),
        ] {
            assert_eq!(ids(&source, filter.clone()), expected, "{filter:?}");
        }
    }

    #[test]
    fn range_bounds_are_typed() {
        let source = FakeSource::default()
            .with("a", CellValue::from_i32(1), [1])
            .with("a", CellValue::from_u32(1), [2])
            .with("a", str("1"), [3]);
        let filter = range(
            "a",
            Bound::Included(CellValue::from_u32(0)),
            Bound::Unbounded,
        );
        assert_eq!(ids(&source, filter), [2]);
        let filter = range("a", Bound::Unbounded, Bound::Included(str("2")));
        assert_eq!(ids(&source, filter), [3]);
    }

    #[test]
    fn malformed_range_is_rejected_before_reading() {
        use Bound::{Excluded, Included, Unbounded};
        let u = CellValue::from_u32;
        for filter in [
            range("a", Unbounded, Unbounded),
            range("a", Included(u(1)), Included(CellValue::from_u64(2))),
            range("a", Included(u(2)), Included(CellValue::from_u64(1))),
            // The shape is checked before the field and the type.
            range("1bad", Unbounded, Unbounded),
            range("1bad", Included(u(1)), Excluded(str("x"))),
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
    fn an_empty_leaf_matches_nothing_without_reading() {
        use Bound::{Excluded, Included};
        // FailingSource would surface as QueryError::Internal if it were consulted.
        for filter in [
            is_in("n", []),
            i32s(Included(1), Included(-1)),
            i32s(Excluded(1), Excluded(-1)),
            i32s(Included(1), Excluded(1)),
            i32s(Excluded(1), Included(1)),
            i32s(Excluded(1), Excluded(1)),
        ] {
            let matches = evaluate(&FailingSource, &filter).expect("filter evaluates");
            assert!(matches.treemap().is_empty(), "{filter:?}");
        }
    }

    #[test]
    fn malformed_in_is_rejected_before_reading() {
        for filter in [
            is_in("a", [CellValue::from_i32(1), CellValue::from_u32(1)]),
            is_in("a", [str("x"), CellValue::from_bytes(b"x")]),
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
            // With no value, the name is still checked.
            (is_in("1bad", []), name(b"1bad"), TermError::InvalidName),
            (
                range("1bad", Bound::Unbounded, Bound::Included(str("x"))),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            (
                range(
                    "b",
                    Bound::Excluded(CellValue::from_bytes(b"x")),
                    Bound::Unbounded,
                ),
                name(b"b"),
                TermError::UnsupportedType(CellType::Bytes),
            ),
            // Emptiness is checked on the terms, after the field and the type.
            (
                range(
                    "1bad",
                    Bound::Included(CellValue::from_u32(2)),
                    Bound::Included(CellValue::from_u32(1)),
                ),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            (
                range(
                    "b",
                    Bound::Included(CellValue::from_bytes(b"y")),
                    Bound::Included(CellValue::from_bytes(b"x")),
                ),
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
            range(
                "a",
                Bound::Included(CellValue::from_i32(1)),
                Bound::Unbounded,
            ),
        ] {
            let result = evaluate(&FailingSource, &filter);
            assert!(is_injected_failure(&result), "{filter:?}: {result:?}");
        }
    }

    #[test]
    fn a_scan_failing_midway_fails_the_range_with_no_partial_union() {
        let inner = || {
            [-1, 0, 1]
                .into_iter()
                .fold(FakeSource::default(), |source, n| {
                    source.with("n", CellValue::from_i32(n), [n.unsigned_abs().into()])
                })
        };
        let filter = i32s(Bound::Unbounded, Bound::Included(i32::MAX));
        // Fails after the first term, after the second, and after the last.
        for good in 1..=3 {
            let source = FailingMidScan {
                inner: inner(),
                good,
            };
            let result = evaluate(&source, &filter);
            assert!(is_injected_failure(&result), "{good}: {result:?}");
        }
    }
}
