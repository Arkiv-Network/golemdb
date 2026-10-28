//! Sorting: how a query's match set is put in sort order.
//!
//! [`SortPlan`] is the sort after admission, and [`SortPlan::compare`] the
//! one order every strategy produces. A [`SortingStrategy`] decides what is
//! read to produce it: [`FetchAndSort`] reads every matched record's sort
//! value. [`Queries`](crate::Queries) runs the strategy it is built with.

mod fetch_and_sort;

pub use fetch_and_sort::FetchAndSort;

use std::cmp::Ordering;

use golemdb_cells::{CellName, CellType, CellValue};
use golemdb_index::{Bitmap, IndexTerm};

use crate::{
    CellSource, Direction, PostingSource, QueryError, Result, SortKey, plan::invalid_argument,
};

/// How a match set is put in sort order.
///
/// Every strategy returns the same page for the same state and inputs: the
/// records of the window, in the order [`SortPlan::compare`] defines.
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

/// A query's sort after admission: one key for now. Built by
/// [`SortPlan::admit`] only, so its name is a valid cell name and its type
/// indexable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortPlan {
    field: CellName,
    ty: CellType,
    direction: Direction,
}

impl SortPlan {
    /// The plan of `keys`, most significant first; `None` with no key.
    ///
    /// # Errors
    ///
    /// [`QueryError::InvalidArgument`] for a key whose name is not a valid
    /// cell name, as for a filter leaf's; [`QueryError::NotImplemented`]
    /// for more than one key. A key's type is indexable by construction.
    pub fn admit(keys: &[SortKey]) -> Result<Option<Self>> {
        match keys {
            [] => Ok(None),
            [key] => {
                IndexTerm::check_cell_name(key.field().as_view())
                    .map_err(|source| invalid_argument(key.field(), source))?;
                Ok(Some(Self {
                    field: key.field().clone(),
                    ty: key.cell_type(),
                    direction: key.direction(),
                }))
            }
            [_, _, ..] => Err(QueryError::NotImplemented {
                operation: "query: multi-key sort",
            }),
        }
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
    /// plan's type, whatever its kind; `None`, absent, when it is of
    /// another type or there is no cell.
    pub fn sort_value(&self, cell: Option<CellValue>) -> Option<CellValue> {
        cell.filter(|cell| cell.cell_type() == self.ty)
    }

    /// The order of two matched records, each given by its record ID and
    /// its sort value's order-encoded payload ([`CellValue::value`]), `None`
    /// when absent. Payloads of one type compare as bytes in the type's
    /// order; an absent value sorts below every present one. Descending
    /// reverses both, so absent values come first ascending and last
    /// descending. Ties fall to ascending record ID, in either direction.
    pub fn compare(&self, a: (u64, Option<&[u8]>), b: (u64, Option<&[u8]>)) -> Ordering {
        let ((a_id, a_value), (b_id, b_value)) = (a, b);
        let by_value = a_value.cmp(&b_value);
        match self.direction {
            Direction::Asc => by_value,
            Direction::Desc => by_value.reverse(),
        }
        .then(a_id.cmp(&b_id))
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
        CellKind, CellNameRef, Width,
        marker::{I32, Str, U32, U64},
    };
    use golemdb_index::TermError;
    use proptest::prelude::*;

    use super::*;
    use crate::fakes::{FakeState, bitmap};

    #[test]
    fn no_key_is_no_sort() {
        assert!(matches!(SortPlan::admit(&[]), Ok(None)));
    }

    #[test]
    fn one_key_is_admitted_as_written() {
        let plan = SortPlan::admit(&[SortKey::desc("price", U64)])
            .expect("a valid key")
            .expect("one key");
        assert_eq!(plan.field(), &CellName::from(CellNameRef::raw(b"price")));
        assert_eq!(plan.cell_type(), CellType::Uint(Width::W8));
        assert_eq!(plan.direction(), Direction::Desc);
    }

    #[test]
    fn a_key_with_a_bad_name_is_an_invalid_argument() {
        for name in [&b"1bad"[..], b"a b", b"", &[0xff]] {
            assert!(
                matches!(
                    SortPlan::admit(&[SortKey::asc(name, U32)]),
                    Err(QueryError::InvalidArgument {
                        field,
                        source: TermError::InvalidName,
                    }) if field.as_bytes() == name
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn more_than_one_key_is_not_implemented_yet() {
        let keys = [SortKey::asc("a", U32), SortKey::desc("b", Str)];
        assert!(matches!(
            SortPlan::admit(&keys),
            Err(QueryError::NotImplemented {
                operation: "query: multi-key sort"
            })
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

    /// The page `strategy` returns over `state`, for the records `matched`.
    fn page(
        strategy: &impl SortingStrategy,
        state: &FakeState,
        matched: impl IntoIterator<Item = u64>,
        key: SortKey,
        window: Window,
    ) -> Vec<u64> {
        let sort = SortPlan::admit(&[key])
            .expect("a valid key")
            .expect("one key");
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
                state.with(id, "s", attribute(CellValue::from_str(s)))
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

    /// A record's cell `n`, for the reference check.
    #[derive(Debug, Clone, Copy)]
    enum Cell {
        Absent,
        Attribute(i32),
        Field(i32),
        /// A `u32` attribute: absent to an `i32` key.
        OtherType(u32),
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

    /// Records from ID 64 up: each one's cell, and whether it matched.
    fn records() -> impl Strategy<Value = Vec<(Cell, bool)>> {
        prop::collection::vec((cell(), any::<bool>()), 0..24)
    }

    fn window() -> impl Strategy<Value = Window> {
        (0u64..30, 0usize..30).prop_map(|(offset, page_size)| Window { offset, page_size })
    }

    /// The strategy's page equals the reference's: the matched records
    /// sorted by native `i32` values, absent below every value, then
    /// reversed for descending, ties by ascending ID, then cut.
    fn agrees_with_the_reference(
        strategy: &impl SortingStrategy,
        records: &[(Cell, bool)],
        direction: Direction,
        window: Window,
    ) -> std::result::Result<(), TestCaseError> {
        let records: Vec<_> = (64..).zip(records.iter().copied()).collect();
        let state =
            records
                .iter()
                .fold(FakeState::default(), |state, &(id, (cell, _))| match cell {
                    Cell::Absent => state,
                    Cell::Attribute(n) => state.with(id, "n", attribute(CellValue::from_i32(n))),
                    Cell::Field(n) => state.with(id, "n", CellValue::from_i32(n)),
                    Cell::OtherType(n) => state.with(id, "n", u32(n)),
                });
        let matched: Vec<(u64, Option<i32>)> = records
            .iter()
            .filter(|(_, (_, matched))| *matched)
            .map(|&(id, (cell, _))| match cell {
                Cell::Attribute(n) | Cell::Field(n) => (id, Some(n)),
                Cell::Absent | Cell::OtherType(_) => (id, None),
            })
            .collect();
        let expected = {
            let mut sorted = matched.clone();
            sorted.sort_by(|(a_id, a), (b_id, b)| {
                match direction {
                    Direction::Asc => a.cmp(b),
                    Direction::Desc => b.cmp(a),
                }
                .then(a_id.cmp(b_id))
            });
            window.cut(sorted.into_iter().map(|(id, _)| id))
        };
        let key = SortKey::try_new("n", CellType::Int(Width::W4), direction).expect("i32");
        let ids = matched.iter().map(|(id, _)| *id);
        prop_assert_eq!(page(strategy, &state, ids, key, window), expected);
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

                proptest! {
                    #[test]
                    fn agrees_with_the_reference(
                        records in records(),
                        descending in any::<bool>(),
                        window in window(),
                    ) {
                        let direction = if descending { Direction::Desc } else { Direction::Asc };
                        super::agrees_with_the_reference(&$strategy, &records, direction, window)?;
                    }
                }
            }
        };
    }

    conformance!(fetch_and_sort, FetchAndSort);
}
