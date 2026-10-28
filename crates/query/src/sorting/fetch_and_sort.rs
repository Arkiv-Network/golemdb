//! [`FetchAndSort`]: the sorting strategy of the spec (design §12, rule 8
//! of the query proposal).

use golemdb_cells::CellValue;
use golemdb_index::Bitmap;

use super::{SortPlan, SortingStrategy, Window};
use crate::{CellSource, PostingSource, Result};

/// Fetch every matched record's sort value, in ascending record ID order,
/// then order them in memory. Reads one cell per matched record, whatever
/// page is asked for, and never the index; keeps only as many values as the
/// page reaches, about twice that at most.
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
        let matches = matches.treemap();
        let mut least = Least::new(sort, window.end(), matches.len());
        for record_id in matches.iter() {
            let value = sort.sort_value(cells.cell(record_id, sort.field())?);
            least.push(Entry { record_id, value });
        }
        Ok(window.cut(least.into_sorted().into_iter().map(|entry| entry.record_id)))
    }
}

/// A matched record and its sort value; `None` when absent.
struct Entry {
    record_id: u64,
    value: Option<CellValue>,
}

impl Entry {
    /// What [`SortPlan::compare`] orders it by.
    fn key(&self) -> (u64, Option<&[u8]>) {
        (self.record_id, self.value.as_ref().map(CellValue::value))
    }
}

/// The least `k` entries pushed, in a plan's order. It holds at most `2k`:
/// once full, it keeps the least `k` and drops the rest, so `n` pushes take
/// O(n) time on average and O(k) memory.
struct Least<'a> {
    sort: &'a SortPlan,
    k: usize,
    entries: Vec<Entry>,
}

impl<'a> Least<'a> {
    /// Room for `2k` entries, or for all `expected` when fewer.
    fn new(sort: &'a SortPlan, k: usize, expected: u64) -> Self {
        let expected = usize::try_from(expected).unwrap_or(usize::MAX);
        Self {
            sort,
            k,
            entries: Vec::with_capacity(k.saturating_mul(2).min(expected)),
        }
    }

    fn push(&mut self, entry: Entry) {
        if self.k == 0 {
            return;
        }
        if self.entries.len() == self.k.saturating_mul(2) {
            self.keep_least();
        }
        self.entries.push(entry);
    }

    /// Drop all but the least `k`.
    fn keep_least(&mut self) {
        if self.entries.len() > self.k {
            let sort = self.sort;
            self.entries
                .select_nth_unstable_by(self.k - 1, |a, b| sort.compare(a.key(), b.key()));
            self.entries.truncate(self.k);
        }
    }

    /// The least `k`, in order.
    fn into_sorted(mut self) -> Vec<Entry> {
        self.keep_least();
        let sort = self.sort;
        self.entries
            .sort_unstable_by(|a, b| sort.compare(a.key(), b.key()));
        self.entries
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use golemdb_cells::{CellKind, CellName, marker::U32};
    use golemdb_index::{IndexError, IndexTerm, TermRange};

    use super::*;
    use crate::{
        SortKey,
        fakes::{FakeState, bitmap},
    };

    fn sort_by_n() -> SortPlan {
        SortPlan::admit(&[SortKey::asc("n", U32)])
            .expect("a valid key")
            .expect("one key")
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

    /// Cells that count their reads.
    struct CountingCells<'a> {
        inner: &'a dyn CellSource,
        reads: Cell<usize>,
    }

    impl CellSource for CountingCells<'_> {
        fn cell(
            &self,
            record_id: u64,
            name: &CellName,
        ) -> golemdb_cells::Result<Option<CellValue>> {
            self.reads.set(self.reads.get() + 1);
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
            let cells = CountingCells {
                inner: &state.cells,
                reads: Cell::new(0),
            };
            let window = Window { offset, page_size };
            FetchAndSort
                .sorted_page(&NoIndex, &cells, &bitmap(64..=73), &sort_by_n(), window)
                .expect("in-memory reads");
            assert_eq!(cells.reads.get(), 10, "{window:?}");
        }
    }

    #[test]
    fn the_buffer_holds_at_most_twice_what_it_keeps() {
        let sort = sort_by_n();
        let mut least = Least::new(&sort, 3, 100);
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
    fn with_no_room_nothing_is_kept() {
        let sort = sort_by_n();
        let mut least = Least::new(&sort, 0, 5);
        for record_id in 0..5 {
            least.push(Entry {
                record_id,
                value: None,
            });
        }
        assert!(least.into_sorted().is_empty());
    }
}
