//! Sorting: how a query's match set is put in sort order.
//!
//! [`SortPlan`] is the sort after admission, its [`SortLevel`]s the order
//! every strategy produces. A [`SortingStrategy`] decides what is read to
//! produce it: [`FetchAndSort`] reads every matched record's first sort
//! value, and the deeper ones of the records the page needs them for.
//! [`Queries`](crate::Queries) runs the strategy it is built with.

mod fetch_and_sort;

pub use fetch_and_sort::FetchAndSort;

use std::{cmp::Ordering, ops::Range};

use golemdb_cells::{CellName, CellType, CellValue};
use golemdb_index::{Bitmap, IndexTerm};

use crate::{
    CellSource, Direction, PostingSource, QueryError, Result, SortKey, plan::invalid_argument,
};

/// How a match set is put in sort order.
///
/// Every strategy returns the same page for the same state and inputs: the
/// records of the window, in the order of the plan's first level
/// ([`SortLevel::compare`]), records it ties ordered by each deeper level in
/// turn, and records every level ties in ascending record ID order.
/// Strategies differ only in what they read to find it, and so in what they
/// cost.
pub trait SortingStrategy {
    /// The record IDs of `window`, in the order `sort` defines over
    /// `matches`. `index` and `cells` read the snapshot `matches` was
    /// evaluated at. Fails only when a read does.
    fn sorted_page(
        &self,
        index: &impl PostingSource,
        cells: &impl CellSource,
        matches: &Bitmap,
        sort: &SortPlan,
        window: Window,
    ) -> Result<Vec<u64>>;
}

/// A query's sort after admission: one level per sort key, most significant
/// first, at least one. Built by [`SortPlan::admit`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortPlan {
    first: SortLevel,
    deeper: Vec<SortLevel>,
}

impl SortPlan {
    /// The plan of `keys`, most significant first; `None` with no key.
    ///
    /// # Errors
    ///
    /// [`QueryError::LimitExceeded`] for more keys than `max_sort_keys`,
    /// checked first; then [`QueryError::InvalidArgument`] for the first
    /// key whose name is not a valid cell name, as for a filter leaf's. A
    /// key's type is indexable by construction.
    pub fn admit(keys: &[SortKey], max_sort_keys: Option<u32>) -> Result<Option<Self>> {
        if let Some(max) = max_sort_keys
            && keys.len() > max as usize
        {
            return Err(QueryError::LimitExceeded {
                limit: "sort key count",
                max,
            });
        }
        let mut levels = keys.iter().map(SortLevel::admit);
        let Some(first) = levels.next() else {
            return Ok(None);
        };
        Ok(Some(Self {
            first: first?,
            deeper: levels.collect::<Result<_>>()?,
        }))
    }

    /// The most significant level.
    pub fn first(&self) -> &SortLevel {
        &self.first
    }

    /// The levels after the first, in order: each orders the records the
    /// ones before it tie.
    pub fn deeper(&self) -> &[SortLevel] {
        &self.deeper
    }
}

/// One level of a [`SortPlan`]: a sort key after admission, so its name is
/// a valid cell name and its type indexable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortLevel {
    field: CellName,
    ty: CellType,
    direction: Direction,
}

impl SortLevel {
    fn admit(key: &SortKey) -> Result<Self> {
        IndexTerm::check_cell_name(key.field().as_view())
            .map_err(|source| invalid_argument(key.field(), source))?;
        Ok(Self {
            field: key.field().clone(),
            ty: key.cell_type(),
            direction: key.direction(),
        })
    }

    /// The name of the cell records are ordered by.
    pub fn field(&self) -> &CellName {
        &self.field
    }

    /// The type that cell is read as.
    pub fn cell_type(&self) -> CellType {
        self.ty
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The sort value a record's cell gives: the cell when it is of the
    /// level's type, whatever its kind; `None`, absent, when it is of
    /// another type or there is no cell.
    pub fn sort_value(&self, cell: Option<CellValue>) -> Option<CellValue> {
        cell.filter(|cell| cell.cell_type() == self.ty)
    }

    /// The order of two records' sort values at this level, each given by
    /// its order-encoded payload ([`CellValue::value`]), `None` when absent.
    /// Payloads of one type compare as bytes in the type's order; an absent
    /// value sorts below every present one. Descending reverses both, so
    /// absent values come first ascending and last descending. `Equal` is a
    /// tie, for the next level to break, and record ID after the last.
    pub fn compare(&self, a: Option<&[u8]>, b: Option<&[u8]>) -> Ordering {
        let by_value = a.cmp(&b);
        match self.direction {
            Direction::Asc => by_value,
            Direction::Desc => by_value.reverse(),
        }
    }
}

/// The part of the order a page takes: past the first `offset` records, at
/// most `page_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub offset: u64,
    pub page_size: usize,
}

impl Window {
    /// How many records of the order the page reaches: its offset plus its
    /// size, saturating.
    pub fn end(self) -> usize {
        self.skipped().saturating_add(self.page_size)
    }

    /// Whether the page takes any of the order's `positions`.
    pub fn overlaps(self, positions: Range<usize>) -> bool {
        positions.start.max(self.skipped()) < positions.end.min(self.end())
    }

    /// `ids`, in order, cut to the window.
    pub fn cut(self, ids: impl IntoIterator<Item = u64>) -> Vec<u64> {
        ids.into_iter()
            .skip(self.skipped())
            .take(self.page_size)
            .collect()
    }

    /// The offset as a count; one no `usize` holds skips everything.
    fn skipped(self) -> usize {
        usize::try_from(self.offset).unwrap_or(usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use golemdb_cells::{
        CellKind, Width,
        marker::{I32, Str, U32, U64},
    };
    use golemdb_index::TermError;
    use proptest::prelude::*;

    use super::*;
    use crate::fakes::{FakeState, bitmap};

    #[test]
    fn no_key_is_no_sort() {
        assert!(matches!(SortPlan::admit(&[], None), Ok(None)));
        assert!(matches!(SortPlan::admit(&[], Some(0)), Ok(None)));
    }

    #[test]
    fn keys_are_admitted_as_written_most_significant_first() {
        let keys = [
            SortKey::desc("price", U64),
            SortKey::asc("name", Str),
            SortKey::desc("age", I32),
        ];
        let plan = SortPlan::admit(&keys, None)
            .expect("valid keys")
            .expect("three keys");
        let levels: Vec<_> = std::iter::once(plan.first())
            .chain(plan.deeper())
            .map(|level| {
                (
                    level.field().as_bytes(),
                    level.cell_type(),
                    level.direction(),
                )
            })
            .collect();
        let expected: [(&[u8], _, _); 3] = [
            (b"price", CellType::Uint(Width::W8), Direction::Desc),
            (b"name", CellType::Str, Direction::Asc),
            (b"age", CellType::Int(Width::W4), Direction::Desc),
        ];
        assert_eq!(levels, expected);
    }

    #[test]
    fn a_key_with_a_bad_name_is_an_invalid_argument() {
        for name in [&b"1bad"[..], b"a b", b"", &[0xff]] {
            // As the only key, and after a good one.
            for keys in [
                vec![SortKey::asc(name, U32)],
                vec![SortKey::asc("good", U32), SortKey::desc(name, U32)],
            ] {
                assert!(
                    matches!(
                        SortPlan::admit(&keys, None),
                        Err(QueryError::InvalidArgument {
                            field,
                            source: TermError::InvalidName,
                        }) if field.as_bytes() == name
                    ),
                    "{name:?}"
                );
            }
        }
    }

    #[test]
    fn more_keys_than_the_cap_exceed_it() {
        let keys = [
            SortKey::asc("a", U32),
            SortKey::desc("b", Str),
            SortKey::asc("c", I32),
        ];
        for max_sort_keys in [None, Some(3), Some(4)] {
            assert!(
                SortPlan::admit(&keys, max_sort_keys).is_ok(),
                "{max_sort_keys:?}"
            );
        }
        assert!(matches!(
            SortPlan::admit(&keys, Some(2)),
            Err(QueryError::LimitExceeded {
                limit: "sort key count",
                max: 2
            })
        ));
        // The count is checked before the names.
        let bad_names = [SortKey::asc("1bad", U32), SortKey::asc("2bad", U32)];
        assert!(matches!(
            SortPlan::admit(&bad_names, Some(1)),
            Err(QueryError::LimitExceeded { .. })
        ));
    }

    #[test]
    fn a_window_cuts_past_its_offset_up_to_its_size() {
        let window = |offset, page_size| Window { offset, page_size };
        assert_eq!(window(1, 2).cut(0..5), [1, 2]);
        assert_eq!(window(3, usize::MAX).cut(0..5), [3, 4]);
        assert!(window(5, 1).cut(0..5).is_empty());
        assert!(window(u64::MAX, usize::MAX).cut(0..5).is_empty());
        assert_eq!(window(2, 3).end(), 5);
        assert_eq!(window(u64::MAX, 1).end(), usize::MAX);
    }

    #[test]
    fn a_window_overlaps_the_positions_it_takes_one_of() {
        let window = |offset, page_size| Window { offset, page_size };
        // Positions 2, 3 and 4.
        assert!(window(2, 3).overlaps(0..3));
        assert!(window(2, 3).overlaps(4..6));
        assert!(window(2, 3).overlaps(0..9));
        assert!(!window(2, 3).overlaps(0..2));
        assert!(!window(2, 3).overlaps(5..7));
        assert!(!window(2, 0).overlaps(0..9));
        assert!(!window(u64::MAX, 1).overlaps(0..usize::MAX));
    }

    // The conformance cases: what every strategy must return. Each is
    // generic over the strategy, and `conformance!` runs them all for one.

    const EVERYTHING: Window = Window {
        offset: 0,
        page_size: usize::MAX,
    };

    fn attribute(value: CellValue) -> CellValue {
        value
            .with_kind(CellKind::Attribute)
            .expect("an indexable type")
    }

    fn u32(n: u32) -> CellValue {
        attribute(CellValue::from_u32(n))
    }

    fn string(s: &str) -> CellValue {
        attribute(CellValue::from_str(s))
    }

    /// The page `strategy` returns over `state`, for the records `matched`,
    /// sorted by one key.
    fn page(
        strategy: &impl SortingStrategy,
        state: &FakeState,
        matched: impl IntoIterator<Item = u64>,
        key: SortKey,
        window: Window,
    ) -> Vec<u64> {
        page_by(strategy, state, matched, &[key], window)
    }

    /// The page `strategy` returns over `state`, for the records `matched`,
    /// sorted by `keys`.
    fn page_by(
        strategy: &impl SortingStrategy,
        state: &FakeState,
        matched: impl IntoIterator<Item = u64>,
        keys: &[SortKey],
        window: Window,
    ) -> Vec<u64> {
        let sort = SortPlan::admit(keys, None)
            .expect("valid keys")
            .expect("a key at least");
        strategy
            .sorted_page(&state.index, &state.cells, &bitmap(matched), &sort, window)
            .expect("in-memory reads")
    }

    fn absent_values_come_first_ascending_and_last_descending(strategy: &impl SortingStrategy) {
        // 65 and 67 have no `n`.
        let state = FakeState::default()
            .with(64, "n", u32(2))
            .with(66, "n", u32(1))
            .with(68, "n", u32(3));
        let asc = page(
            strategy,
            &state,
            64..=68,
            SortKey::asc("n", U32),
            EVERYTHING,
        );
        assert_eq!(asc, [65, 67, 66, 64, 68]);
        let desc = page(
            strategy,
            &state,
            64..=68,
            SortKey::desc("n", U32),
            EVERYTHING,
        );
        assert_eq!(desc, [68, 64, 66, 65, 67]);
    }

    fn ties_come_in_ascending_record_id_order(strategy: &impl SortingStrategy) {
        let state = [(64, 5), (65, 1), (66, 5), (67, 1)]
            .into_iter()
            .fold(FakeState::default(), |state, (id, n)| {
                state.with(id, "n", u32(n))
            });
        let asc = page(
            strategy,
            &state,
            64..=67,
            SortKey::asc("n", U32),
            EVERYTHING,
        );
        assert_eq!(asc, [65, 67, 64, 66]);
        let desc = page(
            strategy,
            &state,
            64..=67,
            SortKey::desc("n", U32),
            EVERYTHING,
        );
        assert_eq!(desc, [64, 66, 65, 67]);
    }

    fn a_cell_of_another_type_is_absent(strategy: &impl SortingStrategy) {
        let state = FakeState::default()
            .with(64, "n", u32(1))
            .with(65, "n", attribute(CellValue::from_i32(0)))
            .with(66, "n", CellValue::from_str("a"));
        let by_u32 = page(
            strategy,
            &state,
            64..=66,
            SortKey::asc("n", U32),
            EVERYTHING,
        );
        assert_eq!(by_u32, [65, 66, 64]);
        let by_i32 = page(
            strategy,
            &state,
            64..=66,
            SortKey::asc("n", I32),
            EVERYTHING,
        );
        assert_eq!(by_i32, [64, 66, 65]);
        let by_str = page(
            strategy,
            &state,
            64..=66,
            SortKey::desc("n", Str),
            EVERYTHING,
        );
        assert_eq!(by_str, [66, 64, 65]);
    }

    fn a_field_sorts_as_an_attribute_of_its_type(strategy: &impl SortingStrategy) {
        let state = FakeState::default()
            .with(64, "n", u32(2))
            .with(65, "n", CellValue::from_u32(1))
            .with(66, "n", CellValue::from_u32(3));
        let asc = page(
            strategy,
            &state,
            64..=66,
            SortKey::asc("n", U32),
            EVERYTHING,
        );
        assert_eq!(asc, [65, 64, 66]);
        let desc = page(
            strategy,
            &state,
            64..=66,
            SortKey::desc("n", U32),
            EVERYTHING,
        );
        assert_eq!(desc, [66, 64, 65]);
    }

    fn only_the_matches_are_ordered_by_the_named_cell(strategy: &impl SortingStrategy) {
        // `n` descends as the IDs ascend; `m` ascends with them.
        let state = (64..=68).fold(FakeState::default(), |state, id| {
            let id32 = u32::try_from(id).expect("small ID");
            state
                .with(id, "n", u32(100 - id32))
                .with(id, "m", u32(id32))
        });
        let by_n = page(
            strategy,
            &state,
            [65, 67],
            SortKey::asc("n", U32),
            EVERYTHING,
        );
        assert_eq!(by_n, [67, 65]);
        let by_m = page(
            strategy,
            &state,
            [65, 67],
            SortKey::asc("m", U32),
            EVERYTHING,
        );
        assert_eq!(by_m, [65, 67]);
    }

    fn values_come_in_their_types_order(strategy: &impl SortingStrategy) {
        // The type's order, not the native bytes': negative before positive.
        let state = [(64, 1), (65, i32::MIN), (66, -1), (67, 0), (68, i32::MAX)]
            .into_iter()
            .fold(FakeState::default(), |state, (id, n)| {
                state.with(id, "n", attribute(CellValue::from_i32(n)))
            });
        let asc = page(
            strategy,
            &state,
            64..=68,
            SortKey::asc("n", I32),
            EVERYTHING,
        );
        assert_eq!(asc, [65, 66, 67, 64, 68]);
        // Byte order, so a prefix first: "" < "a" < "ab" < "b".
        let state = [(64, "b"), (65, "ab"), (66, ""), (67, "a")]
            .into_iter()
            .fold(FakeState::default(), |state, (id, s)| {
                state.with(id, "s", string(s))
            });
        let asc = page(
            strategy,
            &state,
            64..=67,
            SortKey::asc("s", Str),
            EVERYTHING,
        );
        assert_eq!(asc, [66, 67, 65, 64]);
    }

    fn the_page_is_the_window_of_the_order(strategy: &impl SortingStrategy) {
        // Descending, so the order is 68, 67, 66, 65, 64.
        let state = (64..=68).fold(FakeState::default(), |state, id| {
            state.with(id, "n", u32(u32::try_from(id).expect("small ID")))
        });
        let desc = |offset, page_size| {
            let window = Window { offset, page_size };
            page(strategy, &state, 64..=68, SortKey::desc("n", U32), window)
        };
        assert_eq!(desc(1, 2), [67, 66]);
        assert_eq!(desc(3, usize::MAX), [65, 64]);
        assert!(desc(0, 0).is_empty());
        assert!(desc(5, 1).is_empty());
        assert!(desc(u64::MAX, usize::MAX).is_empty());
    }

    /// The example of design §12: six records with a `status` and an
    /// `amount`.
    fn statuses_and_amounts() -> FakeState {
        [
            (100, "active", 100),
            (101, "closed", 50),
            (102, "active", 300),
            (103, "active", 100),
            (104, "closed", 70),
            (105, "pending", 10),
        ]
        .into_iter()
        .fold(FakeState::default(), |state, (id, status, amount)| {
            state
                .with(id, "status", string(status))
                .with(id, "amount", u32(amount))
        })
    }

    fn deeper_keys_break_the_ties_of_the_ones_before(strategy: &impl SortingStrategy) {
        let state = statuses_and_amounts();
        let sorted = |keys: &[SortKey]| page_by(strategy, &state, 100..=105, keys, EVERYTHING);
        // 100 and 103 tie on both keys: ascending record ID, in either
        // direction.
        assert_eq!(
            sorted(&[SortKey::asc("status", Str), SortKey::desc("amount", U32)]),
            [102, 100, 103, 104, 101, 105]
        );
        assert_eq!(
            sorted(&[SortKey::desc("status", Str), SortKey::asc("amount", U32)]),
            [105, 101, 104, 100, 103, 102]
        );
        // A deeper key that breaks no tie changes nothing.
        assert_eq!(
            sorted(&[SortKey::desc("amount", U32), SortKey::asc("status", Str)]),
            [102, 100, 103, 104, 101, 105]
        );
    }

    fn absent_values_sort_below_every_value_at_any_level(strategy: &impl SortingStrategy) {
        // Every record but 68 ties on `a`. Their `b` is absent for 65, a
        // `str` for 66, and a field for 67.
        let state = FakeState::default()
            .with(64, "a", u32(1))
            .with(64, "b", u32(2))
            .with(65, "a", u32(1))
            .with(66, "a", u32(1))
            .with(66, "b", string("x"))
            .with(67, "a", u32(1))
            .with(67, "b", CellValue::from_u32(1))
            .with(68, "a", u32(0));
        let sorted = |b| {
            page_by(
                strategy,
                &state,
                64..=68,
                &[SortKey::asc("a", U32), b],
                EVERYTHING,
            )
        };
        assert_eq!(sorted(SortKey::asc("b", U32)), [68, 65, 66, 67, 64]);
        assert_eq!(sorted(SortKey::desc("b", U32)), [68, 64, 67, 65, 66]);
    }

    fn the_page_is_the_window_of_a_multi_key_order(strategy: &impl SortingStrategy) {
        // `a` ties in threes; `b` descending orders each three.
        let state = (64..=72).fold(FakeState::default(), |state, id| {
            let id32 = u32::try_from(id).expect("small ID");
            state
                .with(id, "a", u32((id32 - 64) / 3))
                .with(id, "b", u32(id32))
        });
        let keys = [SortKey::asc("a", U32), SortKey::desc("b", U32)];
        let sorted = |offset, page_size| {
            let window = Window { offset, page_size };
            page_by(strategy, &state, 64..=72, &keys, window)
        };
        assert_eq!(sorted(0, usize::MAX), [66, 65, 64, 69, 68, 67, 72, 71, 70]);
        // Pages that start or end inside a tie, or both.
        assert_eq!(sorted(1, 4), [65, 64, 69, 68]);
        assert_eq!(sorted(2, 2), [64, 69]);
        assert_eq!(sorted(4, 1), [68]);
        assert_eq!(sorted(8, 5), [70]);
        assert!(sorted(0, 0).is_empty());
        assert!(sorted(9, 1).is_empty());
    }

    /// A record's cell, for the reference check.
    #[derive(Debug, Clone, Copy)]
    enum Cell {
        Absent,
        Attribute(i32),
        Field(i32),
        /// A `u32` attribute: absent to an `i32` key.
        OtherType(u32),
    }

    impl Cell {
        /// The record's sort value under an `i32` key.
        fn value(self) -> Option<i32> {
            match self {
                Cell::Attribute(n) | Cell::Field(n) => Some(n),
                Cell::Absent | Cell::OtherType(_) => None,
            }
        }
    }

    fn cell() -> impl Strategy<Value = Cell> {
        // Few values, so ties are common.
        prop_oneof![
            Just(Cell::Absent),
            (-3..=3).prop_map(Cell::Attribute),
            (-3..=3).prop_map(Cell::Field),
            (0u32..=3).prop_map(Cell::OtherType),
        ]
    }

    /// The names of a reference record's cells.
    const NAMES: [&str; 3] = ["a", "b", "c"];

    /// A record of the reference check: its cells `NAMES`, and whether it
    /// matched.
    type Record = ([Cell; 3], bool);

    /// Records from ID 64 up.
    fn records() -> impl Strategy<Value = Vec<Record>> {
        prop::collection::vec(([cell(), cell(), cell()], any::<bool>()), 0..24)
    }

    /// One to three sort keys, each by one of `NAMES`, which may repeat.
    fn keys() -> impl Strategy<Value = Vec<(usize, Direction)>> {
        let direction = prop_oneof![Just(Direction::Asc), Just(Direction::Desc)];
        prop::collection::vec((0..NAMES.len(), direction), 1..=3)
    }

    fn window() -> impl Strategy<Value = Window> {
        (0u64..30, 0usize..30).prop_map(|(offset, page_size)| Window { offset, page_size })
    }

    /// The strategy's page equals the reference's: the matched records
    /// sorted by each key's native `i32` values in turn, absent below every
    /// value, then reversed for descending; ties by ascending ID; then cut.
    fn agrees_with_the_reference(
        strategy: &impl SortingStrategy,
        records: &[Record],
        keys: &[(usize, Direction)],
        window: Window,
    ) -> std::result::Result<(), TestCaseError> {
        let records: Vec<(u64, Record)> = (64..).zip(records.iter().copied()).collect();
        let state = records
            .iter()
            .flat_map(|&(id, (cells, _))| NAMES.into_iter().zip(cells).map(move |cell| (id, cell)))
            .fold(
                FakeState::default(),
                |state, (id, (name, cell))| match cell {
                    Cell::Absent => state,
                    Cell::Attribute(n) => state.with(id, name, attribute(CellValue::from_i32(n))),
                    Cell::Field(n) => state.with(id, name, CellValue::from_i32(n)),
                    Cell::OtherType(n) => state.with(id, name, u32(n)),
                },
            );
        let matched: Vec<(u64, [Option<i32>; 3])> = records
            .iter()
            .filter(|(_, (_, matched))| *matched)
            .map(|&(id, (cells, _))| (id, cells.map(Cell::value)))
            .collect();
        let expected = {
            let mut sorted = matched.clone();
            sorted.sort_by(|(a_id, a), (b_id, b)| {
                keys.iter()
                    .map(|&(cell, direction)| match direction {
                        Direction::Asc => a[cell].cmp(&b[cell]),
                        Direction::Desc => b[cell].cmp(&a[cell]),
                    })
                    .fold(Ordering::Equal, Ordering::then)
                    .then(a_id.cmp(b_id))
            });
            window.cut(sorted.into_iter().map(|(id, _)| id))
        };
        let i32 = CellType::Int(Width::W4);
        let keys: Vec<_> = keys
            .iter()
            .map(|&(cell, direction)| SortKey::try_new(NAMES[cell], i32, direction).expect("i32"))
            .collect();
        let ids = matched.iter().map(|(id, _)| *id);
        prop_assert_eq!(page_by(strategy, &state, ids, &keys, window), expected);
        Ok(())
    }

    /// Every conformance case, as one test each, for `$strategy`, in a
    /// module named `$name`.
    macro_rules! conformance {
        ($name:ident, $strategy:expr) => {
            mod $name {
                use proptest::prelude::*;

                use super::*;

                #[test]
                fn absent_values_come_first_ascending_and_last_descending() {
                    super::absent_values_come_first_ascending_and_last_descending(&$strategy);
                }

                #[test]
                fn ties_come_in_ascending_record_id_order() {
                    super::ties_come_in_ascending_record_id_order(&$strategy);
                }

                #[test]
                fn a_cell_of_another_type_is_absent() {
                    super::a_cell_of_another_type_is_absent(&$strategy);
                }

                #[test]
                fn a_field_sorts_as_an_attribute_of_its_type() {
                    super::a_field_sorts_as_an_attribute_of_its_type(&$strategy);
                }

                #[test]
                fn only_the_matches_are_ordered_by_the_named_cell() {
                    super::only_the_matches_are_ordered_by_the_named_cell(&$strategy);
                }

                #[test]
                fn values_come_in_their_types_order() {
                    super::values_come_in_their_types_order(&$strategy);
                }

                #[test]
                fn the_page_is_the_window_of_the_order() {
                    super::the_page_is_the_window_of_the_order(&$strategy);
                }

                #[test]
                fn deeper_keys_break_the_ties_of_the_ones_before() {
                    super::deeper_keys_break_the_ties_of_the_ones_before(&$strategy);
                }

                #[test]
                fn absent_values_sort_below_every_value_at_any_level() {
                    super::absent_values_sort_below_every_value_at_any_level(&$strategy);
                }

                #[test]
                fn the_page_is_the_window_of_a_multi_key_order() {
                    super::the_page_is_the_window_of_a_multi_key_order(&$strategy);
                }

                proptest! {
                    #[test]
                    fn agrees_with_the_reference(
                        records in records(),
                        keys in keys(),
                        window in window(),
                    ) {
                        super::agrees_with_the_reference(&$strategy, &records, &keys, window)?;
                    }
                }
            }
        };
    }

    conformance!(fetch_and_sort, FetchAndSort);
}
