//! Plan evaluation over any [`PostingSource`]. Pure; reads only through the
//! source, so it is testable without storage.
//!
//! A child of an `And` after its first, and everything under it, is only
//! ever intersected with or subtracted from the `And`'s running set, so its
//! reads are confined to the running set's regions ([`Regions`]): a
//! container in any other region cannot change the result, and is not read.

use std::iter;

use golemdb_index::{Bitmap, IndexTerm, Regions};

use crate::{
    QueryError, Result,
    plan::{AndChild, Plan},
    posting_source::PostingSource,
};

/// Where a plan's reads are confined.
#[derive(Debug, Clone, Copy)]
enum Within<'a> {
    /// Every container of a term: the filter, and any node not under a
    /// later child of an `And`.
    Everywhere,
    /// Only the containers in these regions: a later child of an `And` and
    /// everything under it, confined to the running set's regions.
    Regions(&'a Regions),
}

/// The match set of an admitted filter: the IDs of every matching record,
/// ascending. Consumes the plan. Fails only when the source does.
pub(crate) fn match_set(source: &impl PostingSource, plan: Plan) -> Result<Bitmap> {
    eval(source, plan, Within::Everywhere)
}

/// The match set of `plan`, with its reads confined as `within` says: all
/// of it, or its record IDs in those regions only.
fn eval(source: &impl PostingSource, plan: Plan, within: Within<'_>) -> Result<Bitmap> {
    match plan {
        Plan::TermRead(term) => postings(source, &term, within),
        Plan::TermReads(terms) => union(terms.iter().map(|term| postings(source, term, within))),
        Plan::Scan(range) => match within {
            Within::Everywhere => union(source.scan(range)?),
            Within::Regions(regions) => union(source.scan_within(range, regions)?),
        },
        Plan::Empty => Ok(Bitmap::default()),
        Plan::And { first, rest } => and(source, *first, rest, within),
        Plan::Or { first, rest } => union(
            iter::once(*first)
                .chain(rest)
                .map(|child| eval(source, child, within)),
        ),
        Plan::Not(child) => not(source, *child, within),
    }
}

/// The postings of `term`, confined as `within` says.
fn postings(source: &impl PostingSource, term: &IndexTerm, within: Within<'_>) -> Result<Bitmap> {
    Ok(match within {
        Within::Everywhere => source.postings(term)?,
        Within::Regions(regions) => source.postings_within(term, regions)?,
    })
}

/// The match set of a standalone `Not`: every live user record, the
/// postings of [`IndexTerm::live`], less `child`'s match set, both confined
/// as `within` says. The live set is read first; when it is empty, `child`
/// is not evaluated, as an `And` stops once its running set is empty.
fn not(source: &impl PostingSource, child: Plan, within: Within<'_>) -> Result<Bitmap> {
    let mut live = postings(source, &IndexTerm::live(), within)?;
    if !live.treemap().is_empty() {
        live -= eval(source, child, within)?;
    }
    Ok(live)
}

/// The match set of an `And`: the running set starts as `first`'s, read
/// as `within` says, then each of `rest`, in the order given, is
/// intersected with it, or, a `Not` child, subtracted from it. Each of
/// `rest` reads only the regions of the running set as it stands before
/// it, which every child can only narrow. Once the running set is empty,
/// the remaining children are not evaluated: nothing of theirs is read.
fn and(
    source: &impl PostingSource,
    first: Plan,
    rest: Vec<AndChild>,
    within: Within<'_>,
) -> Result<Bitmap> {
    let mut running = eval(source, first, within)?;
    for child in rest {
        if running.treemap().is_empty() {
            break;
        }
        let regions = Regions::of(&running);
        let within = Within::Regions(&regions);
        match child {
            AndChild::Positive(child) => running &= eval(source, child, within)?,
            AndChild::Not(child) => running -= eval(source, child, within)?,
        }
    }
    Ok(running)
}

/// The union of `sets`, taken one after another in the order given: a
/// leaf's postings as they are read, or an `Or`'s children as they are
/// evaluated.
fn union<E>(mut sets: impl Iterator<Item = std::result::Result<Bitmap, E>>) -> Result<Bitmap>
where
    QueryError: From<E>,
{
    sets.try_fold(Bitmap::default(), |mut union, set| {
        union |= set?;
        Ok(union)
    })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, ops::Bound};

    use proptest::prelude::*;

    use golemdb_cells::{CellKind, CellName, CellNameRef, CellType, CellValue, Width, marker::I32};
    use golemdb_index::{IndexError, IndexTerm, TermError, TermRange};

    use super::*;
    use crate::{Bounds, EqValue, ExistsType, Filter, InValues, QueryError, fakes::FakeSource};

    /// A [`FakeSource`] that records every term it reads, in order: asked
    /// for, or stepped to by a scan. Beside each, `within` records the
    /// regions the read was confined to, `None` for none.
    struct RecordingSource {
        inner: FakeSource,
        reads: RefCell<Vec<IndexTerm>>,
        within: RefCell<Vec<Option<Vec<u64>>>>,
    }

    impl RecordingSource {
        fn new(inner: FakeSource) -> Self {
            Self {
                inner,
                reads: RefCell::default(),
                within: RefCell::default(),
            }
        }

        fn record(&self, term: &IndexTerm, regions: Option<&Regions>) {
            self.reads.borrow_mut().push(term.clone());
            let numbers = regions.map(|regions| {
                regions
                    .as_slice()
                    .iter()
                    .map(|region| region.get())
                    .collect()
            });
            self.within.borrow_mut().push(numbers);
        }
    }

    impl PostingSource for RecordingSource {
        fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            self.record(term, None);
            self.inner.postings(term)
        }

        fn scan(
            &self,
            range: TermRange,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Ok(self.inner.run(range).map(|(term, postings)| {
                self.record(term, None);
                Ok(postings.clone())
            }))
        }

        fn postings_within(
            &self,
            term: &IndexTerm,
            regions: &Regions,
        ) -> golemdb_index::Result<Bitmap> {
            self.record(term, Some(regions));
            self.inner.postings_within(term, regions)
        }

        fn scan_within(
            &self,
            range: TermRange,
            regions: &Regions,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Ok(self.inner.run(range).map(move |(term, postings)| {
                self.record(term, Some(regions));
                Ok(postings.within(regions))
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

        fn postings_within(&self, _: &IndexTerm, _: &Regions) -> golemdb_index::Result<Bitmap> {
            Err(IndexError::Corruption("injected"))
        }

        fn scan_within(
            &self,
            _: TermRange,
            _: &Regions,
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

        fn postings_within(
            &self,
            term: &IndexTerm,
            regions: &Regions,
        ) -> golemdb_index::Result<Bitmap> {
            self.inner.postings_within(term, regions)
        }

        fn scan_within(
            &self,
            range: TermRange,
            regions: &Regions,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Ok(self
                .inner
                .scan_within(range, regions)?
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

    /// Through the runtime path: the value must be of an indexable type.
    fn eq(field: &str, value: CellValue) -> Filter {
        Filter::Eq {
            field: name(field.as_bytes()),
            value: EqValue::try_from(value).expect("a value of an indexable type"),
        }
    }

    /// Through the runtime path: the values must be of one indexable type.
    fn is_in(field: &str, values: impl IntoIterator<Item = CellValue>) -> Filter {
        Filter::In {
            field: name(field.as_bytes()),
            values: InValues::try_from(values.into_iter().collect::<Vec<_>>())
                .expect("values of one indexable type"),
        }
    }

    /// A range of the bounds' type, through the runtime path: at least one
    /// side holds a value, and both are of one indexable type.
    fn range(field: &str, lower: Bound<CellValue>, upper: Bound<CellValue>) -> Filter {
        let ty = [&lower, &upper]
            .into_iter()
            .find_map(|bound| match bound {
                Bound::Included(value) | Bound::Excluded(value) => Some(value.cell_type()),
                Bound::Unbounded => None,
            })
            .expect("a bounded side names the type");
        Filter::Range {
            field: name(field.as_bytes()),
            bounds: Bounds::try_new(ty, lower, upper).expect("bounds of an indexable type"),
        }
    }

    fn prefix(field: &str, prefix: &str) -> Filter {
        Filter::Prefix {
            field: name(field.as_bytes()),
            prefix: prefix.to_owned(),
        }
    }

    /// Through the runtime path: `ty`, when given, must be indexable.
    fn exists(field: &str, ty: Option<CellType>) -> Filter {
        Filter::Exists {
            field: name(field.as_bytes()),
            ty: ty.map(|ty| ExistsType::try_from(ty).expect("an indexable type")),
        }
    }

    fn str(value: &str) -> CellValue {
        CellValue::from_str(value)
    }

    /// Admission, then evaluation: the sequence `query` and `count` follow.
    fn evaluate(source: &impl PostingSource, filter: &Filter) -> Result<Bitmap> {
        match_set(source, Plan::admit(filter, None)?)
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
        // `EqValue` keeps its value as a field, so an attribute and a field
        // of one value are one leaf, matching the attribute cells indexed.
        let source = FakeSource::default().with("a", CellValue::from_u64(7), [1]);
        let attribute = CellValue::from_u64(7)
            .with_kind(CellKind::Attribute)
            .expect("indexable type");
        assert_eq!(eq("a", attribute), eq("a", CellValue::from_u64(7)));
        assert_eq!(ids(&source, eq("a", CellValue::from_u64(7))), [1]);
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
        let source =
            RecordingSource::new(FakeSource::default().with("n", CellValue::from_i32(-1), [1]));
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
        let source = RecordingSource::new(inner);
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
    fn prefix_matches_the_union_of_the_strings_starting_with_it() {
        let source = FakeSource::default()
            .with("s", str(""), [1])
            .with("s", str("a"), [2, 40])
            .with("s", str("ab"), [3])
            .with("s", str("abc"), [4, 40])
            .with("s", str("b"), [5])
            .with("s", str("é"), [6])
            .with("s", CellValue::from_bytes4(*b"abcd"), [7])
            .with("sa", str("a"), [8]);
        for (filter, expected) in [
            (prefix("s", ""), &[1, 2, 3, 4, 5, 6, 40][..]),
            (prefix("s", "a"), &[2, 3, 4, 40]),
            (prefix("s", "ab"), &[3, 4, 40]),
            (prefix("s", "abc"), &[4, 40]),
            (prefix("s", "abcd"), &[]),
            (prefix("s", "e"), &[]),
            (prefix("s", "é"), &[6]),
        ] {
            assert_eq!(ids(&source, filter.clone()), expected, "{filter:?}");
        }
    }

    #[test]
    fn prefix_reads_only_its_strings_in_key_order() {
        let inner = FakeSource::default()
            .with("s", str("a"), [1])
            .with("s", str("ab"), [2])
            .with("s", str("a\u{10FFFF}"), [3])
            .with("s", str("aa"), [4])
            .with("s", str(""), [5])
            .with("s", str("b"), [6])
            .with("s", CellValue::from_bytes4(*b"abcd"), [7])
            .with("sa", str("a"), [8]);
        let source = RecordingSource::new(inner);
        let matches = evaluate(&source, &prefix("s", "a")).expect("filter evaluates");
        assert_eq!(matches.treemap().iter().collect::<Vec<_>>(), [1, 2, 3, 4]);
        let term = |value| IndexTerm::from_str_name("s", str(value).as_view()).expect("term");
        assert_eq!(
            *source.reads.borrow(),
            [term("a"), term("aa"), term("ab"), term("a\u{10FFFF}")]
        );
    }

    #[test]
    fn two_unbounded_sides_read_the_whole_run() {
        let inner = FakeSource::default()
            .with("n", CellValue::from_i32(1), [1])
            .with("n", CellValue::from_i32(i32::MIN), [2])
            .with("n", CellValue::from_u32(0), [3])
            .with("nn", CellValue::from_i32(0), [4]);
        let source = RecordingSource::new(inner);
        let filter = Filter::Range {
            field: name(b"n"),
            bounds: Bounds::new(I32, Bound::Unbounded, Bound::Unbounded),
        };
        let matches = evaluate(&source, &filter).expect("filter evaluates");
        assert_eq!(matches.treemap().iter().collect::<Vec<_>>(), [1, 2]);
        let term =
            |n| IndexTerm::from_str_name("n", CellValue::from_i32(n).as_view()).expect("term");
        assert_eq!(*source.reads.borrow(), [term(i32::MIN), term(1)]);
    }

    const I32_TYPE: CellType = CellType::Int(Width::W4);

    #[test]
    fn exists_matches_the_records_with_a_cell_of_its_field() {
        // One cell per record under a name, as records hold them.
        let source = FakeSource::default()
            .with("a", CellValue::from_i32(1), [1, 40])
            .with("a", CellValue::from_i32(2), [2])
            .with("a", str("x"), [3])
            .with("a", CellValue::from_bool(false), [4, 1 << 40])
            .with("ab", CellValue::from_i32(1), [5])
            .with("b", str("x"), [6]);
        for (filter, expected) in [
            (exists("a", None), &[1, 2, 3, 4, 40, 1 << 40][..]),
            (exists("a", Some(I32_TYPE)), &[1, 2, 40]),
            (exists("a", Some(CellType::Str)), &[3]),
            (exists("a", Some(CellType::Bool)), &[4, 1 << 40]),
            (exists("a", Some(CellType::Int(Width::W8))), &[]),
            (exists("c", None), &[]),
        ] {
            assert_eq!(ids(&source, filter.clone()), expected, "{filter:?}");
        }
    }

    #[test]
    fn exists_reads_only_the_terms_of_its_field_in_key_order() {
        let inner = FakeSource::default()
            .with("n", CellValue::from_i32(1), [1])
            .with("n", str(""), [2])
            .with("n", CellValue::from_i32(i32::MIN), [3])
            .with("n", CellValue::from_bool(true), [4])
            .with("m", CellValue::from_i32(0), [5])
            .with("n-", CellValue::from_i32(0), [6])
            .with("nn", CellValue::from_bool(false), [7]);
        let source = RecordingSource::new(inner);
        let term = |value: CellValue| IndexTerm::from_str_name("n", value.as_view()).expect("term");
        let (min, one) = (CellValue::from_i32(i32::MIN), CellValue::from_i32(1));

        // Of one type: that type's run only.
        let matches = evaluate(&source, &exists("n", Some(I32_TYPE))).expect("filter evaluates");
        assert_eq!(matches.treemap().iter().collect::<Vec<_>>(), [1, 3]);
        assert_eq!(
            *source.reads.borrow(),
            [term(min.clone()), term(one.clone())]
        );

        // Of no type: every term of the field, run by run in type tag order.
        source.reads.borrow_mut().clear();
        let matches = evaluate(&source, &exists("n", None)).expect("filter evaluates");
        assert_eq!(matches.treemap().iter().collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(
            *source.reads.borrow(),
            [
                term(CellValue::from_bool(true)),
                term(str("")),
                term(min),
                term(one)
            ]
        );
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
    fn invalid_arguments_are_rejected_before_reading() {
        // FailingSource would surface as QueryError::Internal if it were consulted.
        let not_utf8 = Filter::Eq {
            field: name(&[0xff]),
            value: EqValue::of(I32, 1),
        };
        let whole_run = Filter::Range {
            field: name(b"1bad"),
            bounds: Bounds::new(I32, Bound::Unbounded, Bound::Unbounded),
        };
        for (filter, field, expected) in [
            (
                eq("1bad", CellValue::from_i32(1)),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            (not_utf8, name(&[0xff]), TermError::InvalidName),
            (
                is_in("1bad", [CellValue::from_i32(1)]),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            // With no value, the name is still checked.
            (is_in("1bad", []), name(b"1bad"), TermError::InvalidName),
            (
                range("1bad", Bound::Unbounded, Bound::Included(str("x"))),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            // Emptiness is checked on the terms, after the field.
            (
                range(
                    "1bad",
                    Bound::Included(CellValue::from_u32(2)),
                    Bound::Included(CellValue::from_u32(1)),
                ),
                name(b"1bad"),
                TermError::InvalidName,
            ),
            // Two unbounded sides have no value, but the name is still checked.
            (whole_run, name(b"1bad"), TermError::InvalidName),
            (prefix("1bad", "a"), name(b"1bad"), TermError::InvalidName),
            (exists("1bad", None), name(b"1bad"), TermError::InvalidName),
            (
                exists("1bad", Some(CellType::Str)),
                name(b"1bad"),
                TermError::InvalidName,
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
            prefix("a", ""),
            exists("a", None),
            exists("a", Some(I32_TYPE)),
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

    fn and(children: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::And(children.into_iter().collect())
    }

    fn or(children: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::Or(children.into_iter().collect())
    }

    /// `field = n`, a `u32`.
    fn is(field: &str, n: u32) -> Filter {
        eq(field, CellValue::from_u32(n))
    }

    /// The term `field = n` is indexed under, a `u32`.
    fn term_of(field: &str, n: u32) -> IndexTerm {
        IndexTerm::from_str_name(field, CellValue::from_u32(n).as_view()).expect("valid term")
    }

    /// A source with `field = 1` indexed for `ids`, per field.
    fn source_of<'a>(fields: impl IntoIterator<Item = (&'a str, &'a [u64])>) -> RecordingSource {
        let inner = fields
            .into_iter()
            .fold(FakeSource::default(), |source, (field, ids)| {
                source.with(field, CellValue::from_u32(1), ids.iter().copied())
            });
        RecordingSource::new(inner)
    }

    fn matches(source: &RecordingSource, filter: &Filter) -> Vec<u64> {
        evaluate(source, filter)
            .expect("filter evaluates")
            .treemap()
            .iter()
            .collect()
    }

    #[test]
    fn and_matches_the_records_every_child_matches() {
        let source = source_of([
            ("a", &[1, 2, 3, 1 << 40][..]),
            ("b", &[2, 3, 4, 1 << 40]),
            ("c", &[3, 5, 1 << 40]),
        ]);
        assert_eq!(
            matches(&source, &and([is("a", 1), is("b", 1)])),
            [2, 3, 1 << 40]
        );
        assert_eq!(
            matches(&source, &and([is("a", 1), is("b", 1), is("c", 1)])),
            [3, 1 << 40]
        );
        assert_eq!(matches(&source, &and([is("a", 1)])), [1, 2, 3, 1 << 40]);
        assert!(matches(&source, &and([is("a", 1), is("a", 2)])).is_empty());
    }

    #[test]
    fn or_matches_the_records_some_child_matches() {
        let source = source_of([("a", &[1, 1 << 40][..]), ("b", &[2, 1 << 40])]);
        assert_eq!(
            matches(&source, &or([is("a", 1), is("b", 1)])),
            [1, 2, 1 << 40]
        );
        assert_eq!(
            matches(&source, &or([is("a", 2), is("b", 1)])),
            [2, 1 << 40]
        );
        assert_eq!(matches(&source, &or([is("b", 1)])), [2, 1 << 40]);
        assert!(matches(&source, &or([is("a", 2), is("c", 1)])).is_empty());
    }

    #[test]
    fn nodes_combine_as_written() {
        let source = source_of([("a", &[1, 2][..]), ("b", &[3]), ("c", &[2, 3, 4])]);
        // (a ∨ b) ∧ c, and a ∨ (b ∧ c).
        assert_eq!(
            matches(&source, &and([or([is("a", 1), is("b", 1)]), is("c", 1)])),
            [2, 3]
        );
        assert_eq!(
            matches(&source, &or([is("a", 1), and([is("b", 1), is("c", 1)])])),
            [1, 2, 3]
        );
    }

    #[test]
    fn and_stops_reading_once_no_record_is_left() {
        let source = source_of([("a", &[1][..]), ("b", &[2]), ("c", &[1, 2])]);
        // a ∩ b is empty: c is skipped.
        assert!(matches(&source, &and([is("a", 1), is("b", 1), is("c", 1)])).is_empty());
        assert_eq!(*source.reads.borrow(), [term_of("a", 1), term_of("b", 1)]);

        // An absent first term: nothing else is read.
        source.reads.borrow_mut().clear();
        assert!(matches(&source, &and([is("a", 9), is("c", 1), is("b", 1)])).is_empty());
        assert_eq!(*source.reads.borrow(), [term_of("a", 9)]);

        // A first leaf that admits no value reads nothing at all.
        source.reads.borrow_mut().clear();
        assert!(matches(&source, &and([is_in("a", []), is("c", 1)])).is_empty());
        assert!(source.reads.borrow().is_empty());
    }

    #[test]
    fn and_reads_every_child_while_records_are_left() {
        let source = source_of([("a", &[1, 2][..]), ("b", &[2]), ("c", &[2, 3])]);
        assert_eq!(
            matches(&source, &and([is("a", 1), is("b", 1), is("c", 1)])),
            [2]
        );
        assert_eq!(
            *source.reads.borrow(),
            [term_of("a", 1), term_of("b", 1), term_of("c", 1)]
        );
    }

    #[test]
    fn or_reads_every_child_in_the_written_order() {
        let source = source_of([("a", &[1][..]), ("b", &[2])]);
        assert_eq!(
            matches(&source, &or([is("b", 9), is("a", 1), is("b", 1)])),
            [1, 2]
        );
        assert_eq!(
            *source.reads.borrow(),
            [term_of("b", 9), term_of("a", 1), term_of("b", 1)]
        );
    }

    #[test]
    fn a_shared_condition_is_read_once() {
        // (a ∨ b) ∧ c reads c a single time.
        let source = source_of([("a", &[1][..]), ("b", &[2]), ("c", &[1, 2])]);
        let filter = and([or([is("a", 1), is("b", 1)]), is("c", 1)]);
        assert_eq!(matches(&source, &filter), [1, 2]);
        assert_eq!(
            *source.reads.borrow(),
            [term_of("a", 1), term_of("b", 1), term_of("c", 1)]
        );
    }

    #[test]
    fn nested_nodes_read_depth_first_left_to_right() {
        let source = source_of([("a", &[1][..]), ("b", &[1]), ("c", &[2]), ("d", &[2])]);
        let filter = or([and([is("a", 1), is("b", 1)]), and([is("c", 1), is("d", 1)])]);
        assert_eq!(matches(&source, &filter), [1, 2]);
        assert_eq!(
            *source.reads.borrow(),
            [
                term_of("a", 1),
                term_of("b", 1),
                term_of("c", 1),
                term_of("d", 1)
            ]
        );
    }

    fn not(filter: Filter) -> Filter {
        Filter::Not(Box::new(filter))
    }

    #[test]
    fn a_not_child_drops_its_matches_from_the_running_set() {
        let source = source_of([
            ("a", &[1, 2, 3, 1 << 40][..]),
            ("b", &[2, 1 << 40]),
            ("c", &[3, 4]),
        ]);
        assert_eq!(
            matches(&source, &and([is("a", 1), not(is("b", 1))])),
            [1, 3]
        );
        // In the written order, among positive children.
        assert_eq!(
            matches(&source, &and([is("a", 1), not(is("b", 1)), is("c", 1)])),
            [3]
        );
        assert!(matches(&source, &and([is("a", 1), is("c", 1), not(is("c", 1))])).is_empty());
        // The `Not` of a child that matches nothing keeps the running set.
        assert_eq!(
            matches(&source, &and([is("a", 1), not(is("d", 1))])),
            [1, 2, 3, 1 << 40]
        );
    }

    #[test]
    fn a_not_matches_absent_and_other_typed_cells() {
        // Record 4 has no `b`; records 2 and 3 have one of another type.
        let source = FakeSource::default()
            .with("a", CellValue::from_u32(1), [1, 2, 3, 4])
            .with("b", CellValue::from_u32(1), [1])
            .with("b", CellValue::from_i32(1), [2])
            .with("b", str("1"), [3]);
        assert_eq!(ids(&source, and([is("a", 1), not(is("b", 1))])), [2, 3, 4]);
    }

    #[test]
    fn a_not_subtracts_the_match_set_of_any_child() {
        let source = source_of([("a", &[1, 2, 3, 4][..]), ("b", &[1, 2]), ("c", &[2, 3])]);
        // a ∧ ¬(b ∨ c), a ∧ ¬(b ∧ c), and a ∧ ¬(b ∧ ¬c).
        let b_or_c = or([is("b", 1), is("c", 1)]);
        assert_eq!(matches(&source, &and([is("a", 1), not(b_or_c)])), [4]);
        let b_and_c = and([is("b", 1), is("c", 1)]);
        assert_eq!(
            matches(&source, &and([is("a", 1), not(b_and_c)])),
            [1, 3, 4]
        );
        let b_and_not_c = and([is("b", 1), not(is("c", 1))]);
        assert_eq!(
            matches(&source, &and([is("a", 1), not(b_and_not_c)])),
            [2, 3, 4]
        );
    }

    #[test]
    fn a_not_child_is_read_in_its_turn_while_records_are_left() {
        let source = source_of([("a", &[1, 2][..]), ("b", &[1]), ("c", &[2]), ("d", &[2])]);
        // a ∧ ¬b ∧ ¬(c ∨ d): ¬b leaves record 2, so c and d are read.
        let filter = and([
            is("a", 1),
            not(is("b", 1)),
            not(or([is("c", 1), is("d", 1)])),
        ]);
        assert!(matches(&source, &filter).is_empty());
        assert_eq!(
            *source.reads.borrow(),
            [
                term_of("a", 1),
                term_of("b", 1),
                term_of("c", 1),
                term_of("d", 1)
            ]
        );
    }

    #[test]
    fn and_stops_reading_once_a_not_leaves_no_record() {
        let source = source_of([("a", &[1][..]), ("b", &[1, 2]), ("c", &[1])]);
        // a − b is empty: c and the `Not` after it are skipped.
        let filter = and([is("a", 1), not(is("b", 1)), is("c", 1), not(is("c", 1))]);
        assert!(matches(&source, &filter).is_empty());
        assert_eq!(*source.reads.borrow(), [term_of("a", 1), term_of("b", 1)]);

        // A `Not` after the running set is empty is not read.
        source.reads.borrow_mut().clear();
        assert!(matches(&source, &and([is("a", 1), is("c", 9), not(is("b", 1))])).is_empty());
        assert_eq!(*source.reads.borrow(), [term_of("a", 1), term_of("c", 9)]);
    }

    /// [`source_of`], with `live` as the live user records.
    fn live_source_of<'a>(
        live: &[u64],
        fields: impl IntoIterator<Item = (&'a str, &'a [u64])>,
    ) -> RecordingSource {
        let RecordingSource { inner, .. } = source_of(fields);
        RecordingSource::new(inner.with_live(live.iter().copied()))
    }

    #[test]
    fn a_standalone_not_keeps_the_live_records_its_child_does_not_match() {
        let source = live_source_of(
            &[1, 2, 3, 4, 1 << 40],
            [("a", &[1, 2, 1 << 40][..]), ("b", &[2, 3])],
        );
        assert_eq!(matches(&source, &not(is("a", 1))), [3, 4]);
        // Under an `Or`, as an `And`'s first child, and under another `Not`.
        assert_eq!(
            matches(&source, &or([is("b", 1), not(is("a", 1))])),
            [2, 3, 4]
        );
        assert_eq!(matches(&source, &and([not(is("a", 1)), is("b", 1)])), [3]);
        assert_eq!(
            matches(&source, &and([is("a", 1), not(not(is("b", 1)))])),
            [2]
        );
        assert_eq!(matches(&source, &not(not(is("a", 1)))), [1, 2, 1 << 40]);
        // Over any child: ¬(a ∨ b), ¬(a ∧ b).
        assert_eq!(matches(&source, &not(or([is("a", 1), is("b", 1)]))), [4]);
        assert_eq!(
            matches(&source, &not(and([is("a", 1), is("b", 1)]))),
            [1, 3, 4, 1 << 40]
        );
        // The `Not` of a child that matches nothing is every live record.
        assert_eq!(matches(&source, &not(is("c", 1))), [1, 2, 3, 4, 1 << 40]);
    }

    #[test]
    fn a_standalone_not_matches_absent_and_other_typed_cells() {
        // Record 4 has no `b`; records 2 and 3 have one of another type.
        let source = FakeSource::default()
            .with_live([1, 2, 3, 4])
            .with("b", CellValue::from_u32(1), [1])
            .with("b", CellValue::from_i32(1), [2])
            .with("b", str("1"), [3]);
        assert_eq!(ids(&source, not(is("b", 1))), [2, 3, 4]);
    }

    #[test]
    fn a_standalone_not_reads_the_live_set_then_its_child() {
        let source = live_source_of(&[1, 2], [("a", &[1][..]), ("b", &[2])]);
        assert_eq!(matches(&source, &not(is("a", 1))), [2]);
        assert_eq!(*source.reads.borrow(), [IndexTerm::live(), term_of("a", 1)]);
        // In its turn among its siblings.
        source.reads.borrow_mut().clear();
        assert_eq!(matches(&source, &or([is("a", 1), not(is("b", 1))])), [1]);
        assert_eq!(
            *source.reads.borrow(),
            [term_of("a", 1), IndexTerm::live(), term_of("b", 1)]
        );
        // A `Not` after an `And`'s first child subtracts from the running
        // set: the live set is not read.
        source.reads.borrow_mut().clear();
        assert_eq!(matches(&source, &and([is("a", 1), not(is("b", 1))])), [1]);
        assert_eq!(*source.reads.borrow(), [term_of("a", 1), term_of("b", 1)]);
    }

    #[test]
    fn with_no_live_record_a_standalone_not_reads_nothing_more() {
        let source = live_source_of(&[], [("a", &[1][..]), ("b", &[1])]);
        assert!(matches(&source, &not(is("a", 1))).is_empty());
        assert_eq!(*source.reads.borrow(), [IndexTerm::live()]);
        // As an `And`'s first child, it leaves no running set for the rest.
        source.reads.borrow_mut().clear();
        assert!(matches(&source, &and([not(is("a", 1)), is("b", 1)])).is_empty());
        assert_eq!(*source.reads.borrow(), [IndexTerm::live()]);
    }

    /// Record `offset` of region `region`.
    fn at(region: u64, offset: u64) -> u64 {
        region << 16 | offset
    }

    #[test]
    fn a_later_child_of_an_and_reads_only_the_running_sets_regions() {
        let a = [at(0, 1), at(5, 1)];
        let b = [at(0, 1), at(1, 1), at(5, 2), at(9, 1)];
        let c = [at(0, 1), at(5, 2), at(7, 1)];
        let live = [at(0, 1), at(1, 1), at(5, 1), at(5, 2), at(7, 1), at(9, 1)];
        let fields = || [("a", &a[..]), ("b", &b[..]), ("c", &c[..])];
        let check = |filter: Filter, expected: &[u64], within: &[Option<&[u64]>]| {
            let source = live_source_of(&live, fields());
            assert_eq!(matches(&source, &filter), expected, "{filter:?}");
            let within: Vec<_> = within
                .iter()
                .map(|regions| regions.map(<[u64]>::to_vec))
                .collect();
            assert_eq!(*source.within.borrow(), within, "{filter:?}");
        };
        // The first child reads everywhere; the next, `a`'s regions 0 and 5.
        check(
            and([is("a", 1), is("b", 1)]),
            &[at(0, 1)],
            &[None, Some(&[0, 5])],
        );
        // Each child narrows the regions of the ones after it: a ∧ c is
        // left in region 0 alone.
        check(
            and([is("a", 1), is("c", 1), is("b", 1)]),
            &[at(0, 1)],
            &[None, Some(&[0, 5]), Some(&[0])],
        );
        // A `Not` child, as a positive one.
        check(
            and([is("a", 1), not(is("c", 1))]),
            &[at(5, 1)],
            &[None, Some(&[0, 5])],
        );
        // Everything under a later child: an `Or`, and a standalone `Not`
        // in it, its live set included.
        check(
            and([is("a", 1), or([is("b", 1), not(is("c", 1))])]),
            &[at(0, 1), at(5, 1)],
            &[None, Some(&[0, 5]), Some(&[0, 5]), Some(&[0, 5])],
        );
        // A nested `And` narrows further for its own later children.
        check(
            and([is("b", 1), and([is("a", 1), is("c", 1)])]),
            &[at(0, 1)],
            &[None, Some(&[0, 1, 5, 9]), Some(&[0, 5])],
        );
        // A scan, term by term.
        let at_least_1 = range(
            "c",
            Bound::Included(CellValue::from_u32(1)),
            Bound::Unbounded,
        );
        check(
            and([is("a", 1), at_least_1]),
            &[at(0, 1)],
            &[None, Some(&[0, 5])],
        );
        // With no `And` above, nothing is confined.
        check(
            or([is("a", 1), not(is("b", 1))]),
            &[at(0, 1), at(5, 1), at(7, 1)],
            &[None, None, None],
        );
    }

    /// A filter over `u32` attributes, with a model of which records match.
    #[derive(Debug, Clone)]
    enum Tree {
        Eq(&'static str, u32),
        In(&'static str, Vec<u32>),
        AtLeast(&'static str, u32),
        Exists(&'static str),
        And(Vec<Tree>),
        Or(Vec<Tree>),
        Not(Box<Tree>),
    }

    impl Tree {
        fn filter(&self) -> Filter {
            match self {
                Self::Eq(field, n) => is(field, *n),
                Self::In(field, ns) => is_in(field, ns.iter().copied().map(CellValue::from_u32)),
                Self::AtLeast(field, n) => range(
                    field,
                    Bound::Included(CellValue::from_u32(*n)),
                    Bound::Unbounded,
                ),
                Self::Exists(field) => exists(field, Some(CellType::Uint(Width::W4))),
                Self::And(children) => and(children.iter().map(Self::filter)),
                Self::Or(children) => or(children.iter().map(Self::filter)),
                Self::Not(child) => not(child.filter()),
            }
        }

        fn matches(&self, cells: &BTreeMap<&str, u32>) -> bool {
            match self {
                Self::Eq(field, n) => cells.get(field) == Some(n),
                Self::In(field, ns) => cells.get(field).is_some_and(|value| ns.contains(value)),
                Self::AtLeast(field, n) => cells.get(field).is_some_and(|value| value >= n),
                Self::Exists(field) => cells.contains_key(field),
                Self::And(children) => children.iter().all(|child| child.matches(cells)),
                Self::Or(children) => children.iter().any(|child| child.matches(cells)),
                Self::Not(child) => !child.matches(cells),
            }
        }
    }

    fn field() -> impl Strategy<Value = &'static str> {
        prop::sample::select(&["a", "b", "c"][..])
    }

    /// Trees up to four levels deep, `Not` anywhere.
    fn tree() -> impl Strategy<Value = Tree> {
        let leaf = prop_oneof![
            (field(), 0..3u32).prop_map(|(field, n)| Tree::Eq(field, n)),
            (field(), prop::collection::vec(0..3u32, 0..3))
                .prop_map(|(field, ns)| Tree::In(field, ns)),
            (field(), 0..3u32).prop_map(|(field, n)| Tree::AtLeast(field, n)),
            field().prop_map(Tree::Exists),
        ];
        leaf.prop_recursive(4, 32, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..4).prop_map(Tree::And),
                prop::collection::vec(inner.clone(), 1..4).prop_map(Tree::Or),
                inner.prop_map(|child| Tree::Not(Box::new(child))),
            ]
        })
    }

    /// Record IDs a few to each of 16 regions, so a running set occupies
    /// some regions and not others, and a few anywhere.
    fn record_id() -> impl Strategy<Value = u64> {
        prop_oneof![
            4 => (0..16u64, 0..4u64).prop_map(|(region, offset)| at(region, offset)),
            1 => any::<u64>(),
        ]
    }

    proptest! {
        #[test]
        fn confined_evaluation_matches_a_model(
            records in prop::collection::btree_map(
                record_id(),
                prop::collection::btree_map(field(), 0..3u32, 0..3),
                0..40,
            ),
            trees in prop::collection::vec(tree(), 1..8),
        ) {
            // Index every record's cells, and every record as live.
            let mut terms: BTreeMap<(&str, u32), Vec<u64>> = BTreeMap::new();
            for (&id, cells) in &records {
                for (&field, &value) in cells {
                    terms.entry((field, value)).or_default().push(id);
                }
            }
            let source = terms
                .into_iter()
                .fold(FakeSource::default(), |source, ((field, value), ids)| {
                    source.with(field, CellValue::from_u32(value), ids)
                })
                .with_live(records.keys().copied());
            for tree in trees {
                let expected: Vec<u64> = records
                    .iter()
                    .filter(|(_, cells)| tree.matches(cells))
                    .map(|(&id, _)| id)
                    .collect();
                prop_assert_eq!(ids(&source, tree.filter()), expected, "{:?}", tree);
            }
        }
    }

    #[test]
    fn invalid_trees_are_rejected_before_reading() {
        // FailingSource would surface as QueryError::Internal if it were consulted.
        for filter in [
            and([]),
            or([]),
            and([is("a", 1), or([is("b", 1), and([])])]),
            not(and([])),
        ] {
            assert!(
                matches!(
                    evaluate(&FailingSource, &filter),
                    Err(QueryError::InvalidQuery { .. })
                ),
                "{filter:?}"
            );
        }
        let filter = or([is("a", 1), and([is("b", 1), is("1bad", 1)])]);
        assert!(matches!(
            evaluate(&FailingSource, &filter),
            Err(QueryError::InvalidArgument { field, .. }) if field == name(b"1bad")
        ));
    }

    #[test]
    fn a_failing_child_fails_the_node() {
        // The first child reads; the second, a scan, fails at its first term.
        let source = FailingMidScan {
            inner: FakeSource::default()
                .with("a", CellValue::from_u32(1), [1])
                .with_live([1]),
            good: 0,
        };
        let scan = i32s(Bound::Unbounded, Bound::Included(i32::MAX));
        for filter in [
            and([is("a", 1), scan.clone()]),
            and([is("a", 1), not(scan.clone())]),
            not(scan.clone()),
            or([is("a", 1), scan]),
        ] {
            let result = evaluate(&source, &filter);
            assert!(is_injected_failure(&result), "{filter:?}: {result:?}");
        }
        // The live set's read fails as any other.
        for filter in [
            and([is("a", 1)]),
            and([is("a", 1), not(is("b", 1))]),
            not(is("b", 1)),
            or([is("a", 1)]),
        ] {
            let result = evaluate(&FailingSource, &filter);
            assert!(is_injected_failure(&result), "{filter:?}: {result:?}");
        }
    }
}
