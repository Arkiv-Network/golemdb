//! [`FetchAndSort`]: the sorting strategy of the spec (design §12, rule 8
//! of the query proposal).

use std::{cmp::Ordering, ops::Range};

use golemdb_cells::CellValue;
use golemdb_index::Bitmap;

use super::{SortLevel, SortPlan, SortingStrategy, Window};
use crate::{CellSource, PostingSource, Result};

/// Fetch every matched record's first sort value, in ascending record ID
/// order, then order them in memory. Each deeper level orders the records
/// still tied before it, in the groups the page reaches: their values are
/// fetched level by level, each level in ascending record ID order, and a
/// group the page does not reach is never broken up. Never reads the index;
/// keeps only as many records as the page reaches, about twice that at
/// most, plus those tied with the last of them when a deeper level orders
/// them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FetchAndSort;

impl SortingStrategy for FetchAndSort {
    fn sorted_page(
        &self,
        _index: &impl PostingSource,
        cells: &impl CellSource,
        matches: &Bitmap,
        sort: &SortPlan,
        window: Window,
    ) -> Result<Vec<u64>> {
        let first = sort.first();
        let matches = matches.treemap();
        let ties = match sort.deeper() {
            [] => Ties::Broken,
            [_, ..] => Ties::Kept,
        };
        let mut least = Least::new(first, window.end(), matches.len(), ties);
        for record_id in matches.iter() {
            let value = first.sort_value(cells.cell(record_id, first.field())?);
            least.push(Entry { record_id, value });
        }
        // A prefix of the first level's order, so an entry's index is its
        // position in it.
        let mut entries = least.into_sorted();
        let mut groups: Vec<_> = tied_groups(&entries, 0..entries.len(), window).collect();
        for level in sort.deeper() {
            fetch(cells, level, &mut entries, &groups)?;
            let mut still_tied = Vec::new();
            for group in groups {
                entries[group.clone()].sort_unstable_by(|a, b| a.cmp_at(b, level));
                still_tied.extend(tied_groups(&entries, group, window));
            }
            groups = still_tied;
        }
        Ok(window.cut(entries.into_iter().map(|entry| entry.record_id)))
    }
}

/// A matched record and its sort value at the level being ordered; `None`
/// when absent.
struct Entry {
    record_id: u64,
    value: Option<CellValue>,
}

impl Entry {
    /// The sort value's order-encoded payload, as levels compare it.
    fn value(&self) -> Option<&[u8]> {
        self.value.as_ref().map(CellValue::value)
    }

    /// Its order against `other` at `level`, ties by ascending record ID.
    fn cmp_at(&self, other: &Self, level: &SortLevel) -> Ordering {
        level
            .compare(self.value(), other.value())
            .then(self.record_id.cmp(&other.record_id))
    }
}

/// Replace the value of every entry of `groups` with its value at `level`,
/// fetched in ascending record ID order.
fn fetch(
    cells: &impl CellSource,
    level: &SortLevel,
    entries: &mut [Entry],
    groups: &[Range<usize>],
) -> Result<()> {
    let mut members: Vec<usize> = groups.iter().flat_map(Range::clone).collect();
    members.sort_unstable_by_key(|&member| entries[member].record_id);
    for member in members {
        let entry = &mut entries[member];
        entry.value = level.sort_value(cells.cell(entry.record_id, level.field())?);
    }
    Ok(())
}

/// The runs of `entries[group]`, already in order, that tie on their value:
/// those of two entries or more that the page reaches, which the next level
/// must order.
fn tied_groups(
    entries: &[Entry],
    group: Range<usize>,
    window: Window,
) -> impl Iterator<Item = Range<usize>> {
    let mut start = group.start;
    entries[group]
        .chunk_by(|a, b| a.value() == b.value())
        .map(move |run| {
            let positions = start..start + run.len();
            start = positions.end;
            positions
        })
        .filter(move |positions| positions.len() > 1 && window.overlaps(positions.clone()))
}

/// What becomes of the entries [`Least`] finds tied in value with the last
/// one it keeps.
#[derive(Debug, Clone, Copy)]
enum Ties {
    /// Record ID breaks them: those past the last one kept are dropped.
    Broken,
    /// A deeper level breaks them, and any may come first: all are kept.
    Kept,
}

/// The least `k` entries pushed, in a level's order with ties by record ID,
/// and, when [`Ties::Kept`], every other entry tied in value with the
/// `k`-th. Once it holds twice what it last kept, `2k` at least, it drops
/// the rest, so `n` pushes take O(n) time on average, and memory for about
/// twice what it keeps.
struct Least<'a> {
    level: &'a SortLevel,
    k: usize,
    ties: Ties,
    entries: Vec<Entry>,
    /// How many entries it holds before it drops some.
    full: usize,
}

impl<'a> Least<'a> {
    /// Room for `2k` entries, or for all `expected` when fewer.
    fn new(level: &'a SortLevel, k: usize, expected: u64, ties: Ties) -> Self {
        let expected = usize::try_from(expected).unwrap_or(usize::MAX);
        let full = k.saturating_mul(2);
        Self {
            level,
            k,
            ties,
            entries: Vec::with_capacity(full.min(expected)),
            full,
        }
    }

    fn push(&mut self, entry: Entry) {
        if self.k == 0 {
            return;
        }
        if self.entries.len() >= self.full {
            self.keep_least();
        }
        self.entries.push(entry);
    }

    /// Drop all but the least `k`, and the entries `ties` keeps.
    fn keep_least(&mut self) {
        if self.entries.len() > self.k {
            let level = self.level;
            let (_, last, _) = self
                .entries
                .select_nth_unstable_by(self.k - 1, |a, b| a.cmp_at(b, level));
            match self.ties {
                Ties::Broken => self.entries.truncate(self.k),
                Ties::Kept => {
                    let tie = last.value.clone();
                    let tie = tie.as_ref().map(CellValue::value);
                    let mut position = 0;
                    self.entries.retain(|entry| {
                        position += 1;
                        position <= self.k || entry.value() == tie
                    });
                }
            }
            self.full = self.full.max(self.entries.len().saturating_mul(2));
        }
    }

    /// The entries it keeps, in order.
    fn into_sorted(mut self) -> Vec<Entry> {
        self.keep_least();
        let level = self.level;
        self.entries.sort_unstable_by(|a, b| a.cmp_at(b, level));
        self.entries
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use golemdb_cells::{
        CellKind, CellName,
        marker::{Str, U32},
    };
    use golemdb_index::{IndexError, IndexTerm, TermRange};

    use super::*;
    use crate::{
        SortKey,
        fakes::{FakeState, bitmap},
    };

    fn plan(keys: &[SortKey]) -> SortPlan {
        SortPlan::admit(keys, None)
            .expect("valid keys")
            .expect("a key at least")
    }

    fn sort_by_n() -> SortPlan {
        plan(&[SortKey::asc("n", U32)])
    }

    fn u32(n: u32) -> CellValue {
        CellValue::from_u32(n)
            .with_kind(CellKind::Attribute)
            .expect("indexable")
    }

    /// An index every read of which fails.
    struct NoIndex;

    impl PostingSource for NoIndex {
        fn postings(&self, _: &IndexTerm) -> golemdb_index::Result<Bitmap> {
            Err(IndexError::Corruption("the index was read"))
        }

        fn scan(
            &self,
            _: TermRange,
        ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
            Err::<std::iter::Empty<_>, _>(IndexError::Corruption("the index was read"))
        }
    }

    /// Cells that log their reads, in order.
    struct LoggedCells<'a> {
        inner: &'a dyn CellSource,
        reads: RefCell<Vec<(u64, CellName)>>,
    }

    impl<'a> LoggedCells<'a> {
        fn new(inner: &'a dyn CellSource) -> Self {
            Self {
                inner,
                reads: RefCell::default(),
            }
        }

        /// The records whose cell `name` was read, in read order.
        fn reads_of(&self, name: &str) -> Vec<u64> {
            self.reads
                .borrow()
                .iter()
                .filter(|(_, read)| read.as_bytes() == name.as_bytes())
                .map(|(record_id, _)| *record_id)
                .collect()
        }
    }

    impl CellSource for LoggedCells<'_> {
        fn cell(
            &self,
            record_id: u64,
            name: &CellName,
        ) -> golemdb_cells::Result<Option<CellValue>> {
            self.reads.borrow_mut().push((record_id, name.clone()));
            self.inner.cell(record_id, name)
        }
    }

    fn state() -> FakeState {
        (64..=73).fold(FakeState::default(), |state, id| {
            state.with(id, "n", u32(100 - u32::try_from(id).expect("small ID")))
        })
    }

    #[test]
    fn the_index_is_never_read() {
        let page = FetchAndSort.sorted_page(
            &NoIndex,
            &state().cells,
            &bitmap(64..=73),
            &sort_by_n(),
            Window {
                offset: 1,
                page_size: 2,
            },
        );
        assert_eq!(page.expect("no index read"), [72, 71]);
    }

    #[test]
    fn every_matched_value_is_fetched_whatever_the_window() {
        let state = state();
        for (offset, page_size) in [(0, 0), (0, 1), (3, 2), (10, 5), (u64::MAX, 1)] {
            let cells = LoggedCells::new(&state.cells);
            let window = Window { offset, page_size };
            FetchAndSort
                .sorted_page(&NoIndex, &cells, &bitmap(64..=73), &sort_by_n(), window)
                .expect("in-memory reads");
            assert_eq!(
                cells.reads_of("n"),
                (64..=73).collect::<Vec<_>>(),
                "{window:?}"
            );
        }
    }

    /// The example of design §12, with a third cell `k` that orders the
    /// records `status` and `amount` tie.
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
            let status = CellValue::from_str(status)
                .with_kind(CellKind::Attribute)
                .expect("indexable");
            state
                .with(id, "status", status)
                .with(id, "amount", u32(amount))
                .with(id, "k", u32(200 - u32::try_from(id).expect("small ID")))
        })
    }

    #[test]
    fn deeper_values_are_fetched_only_for_the_ties_the_page_reaches() {
        let state = statuses_and_amounts();
        // By `status` then `amount`, 102 | 100 103 | 104 101 | 105:
        // `status` ties {100, 102, 103} and {101, 104}, then `amount` ties
        // {100, 103}, which `k` orders 103 first.
        let keys = [
            SortKey::asc("status", Str),
            SortKey::desc("amount", U32),
            SortKey::asc("k", U32),
        ];
        let reads = |offset, page_size| {
            let cells = LoggedCells::new(&state.cells);
            let window = Window { offset, page_size };
            let page = FetchAndSort
                .sorted_page(&NoIndex, &cells, &bitmap(100..=105), &plan(&keys), window)
                .expect("in-memory reads");
            assert_eq!(cells.reads_of("status"), (100..=105).collect::<Vec<_>>());
            (page, cells.reads_of("amount"), cells.reads_of("k"))
        };
        // Every tie: each level read in ascending record ID order, across
        // groups. 105 ties with none, so it is read once.
        assert_eq!(
            reads(0, usize::MAX),
            (
                vec![102, 103, 100, 104, 101, 105],
                vec![100, 101, 102, 103, 104],
                vec![100, 103]
            )
        );
        // Only `active`, of which 102 alone: `k` orders nothing.
        assert_eq!(reads(0, 1), (vec![102], vec![100, 102, 103], vec![]));
        // Inside the tie on both.
        assert_eq!(
            reads(2, 1),
            (vec![100], vec![100, 102, 103], vec![100, 103])
        );
        // Only `closed`.
        assert_eq!(reads(3, 2), (vec![104, 101], vec![101, 104], vec![]));
        // Only `pending`, which ties with none.
        assert_eq!(reads(5, 1), (vec![105], vec![], vec![]));
        assert_eq!(reads(0, 0), (vec![], vec![], vec![]));
    }

    #[test]
    fn the_buffer_holds_at_most_twice_what_it_keeps() {
        let sort = sort_by_n();
        let mut least = Least::new(sort.first(), 3, 100, Ties::Broken);
        // Values repeat every ten IDs, so the least three tie on 0.
        for record_id in (0..100).rev() {
            let value = Some(u32(u32::try_from(record_id % 10).expect("small")));
            least.push(Entry { record_id, value });
            assert!(least.entries.len() <= 6, "{}", least.entries.len());
        }
        let kept: Vec<_> = least.into_sorted().iter().map(|e| e.record_id).collect();
        assert_eq!(kept, [0, 10, 20]);
    }

    #[test]
    fn ties_with_the_last_kept_are_kept_for_a_deeper_level() {
        let sort = sort_by_n();
        let kept = |ties| {
            let mut least = Least::new(sort.first(), 3, 5, ties);
            for (record_id, n) in (0..).zip([1, 0, 2, 1, 1]) {
                least.push(Entry {
                    record_id,
                    value: Some(u32(n)),
                });
            }
            least
                .into_sorted()
                .iter()
                .map(|e| e.record_id)
                .collect::<Vec<_>>()
        };
        // The third is 3, on 1, which 4 ties.
        assert_eq!(kept(Ties::Broken), [1, 0, 3]);
        assert_eq!(kept(Ties::Kept), [1, 0, 3, 4]);
    }

    #[test]
    fn the_buffer_holds_at_most_twice_what_it_keeps_with_ties() {
        let sort = sort_by_n();
        let mut least = Least::new(sort.first(), 3, 100, Ties::Kept);
        // The least three tie on 0 with seven more.
        for record_id in (0..100).rev() {
            let value = Some(u32(u32::try_from(record_id % 10).expect("small")));
            least.push(Entry { record_id, value });
            assert!(least.entries.len() <= 20, "{}", least.entries.len());
        }
        let kept: Vec<_> = least.into_sorted().iter().map(|e| e.record_id).collect();
        assert_eq!(kept, (0..100).step_by(10).collect::<Vec<_>>());
    }

    #[test]
    fn with_no_room_nothing_is_kept() {
        let sort = sort_by_n();
        for ties in [Ties::Broken, Ties::Kept] {
            let mut least = Least::new(sort.first(), 0, 5, ties);
            for record_id in 0..5 {
                least.push(Entry {
                    record_id,
                    value: None,
                });
            }
            assert!(least.into_sorted().is_empty());
        }
    }
}
