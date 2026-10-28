//! In-memory stand-ins for the storage seams, shared by the crate's tests:
//! [`FakeSource`] for posting lists, [`FakeCells`] for cells, and
//! [`FakeState`], which writes both from one list of cells, so the index and
//! the cells always agree.

use std::{collections::BTreeMap, ops::RangeBounds};

use golemdb_cells::{CellKind, CellName, CellNameRef, CellValue};
use golemdb_index::{Bitmap, BitmapContainer, IndexTerm, TermRange, path};

use crate::{CellSource, PostingSource};

/// Posting lists in a `BTreeMap`: byte-ordered like the `Index` table.
#[derive(Debug, Default)]
pub(crate) struct FakeSource(BTreeMap<IndexTerm, Bitmap>);

impl FakeSource {
    /// Index `ids` under `field = value` the way the write path does: as
    /// an attribute cell, through `IndexTerm::from_cell`.
    pub(crate) fn with(
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

    /// The entries in `range`, in key order, as a storage scan yields them.
    pub(crate) fn run(&self, range: TermRange) -> impl Iterator<Item = (&IndexTerm, &Bitmap)> {
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

/// Chunk record IDs by their 48-bit path, as the BitmapTrie stores them.
pub(crate) fn bitmap(ids: impl IntoIterator<Item = u64>) -> Bitmap {
    let mut chunks: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
    for id in ids {
        let (path, offset) = path::split(id);
        chunks.entry(path).or_default().push(offset);
    }
    Bitmap::from_containers(
        chunks.into_iter().map(|(path, offsets)| {
            BitmapContainer::from_values(path, offsets).expect("48-bit path")
        }),
    )
    .expect("unique, ordered paths")
}

/// Cells in a `BTreeMap`, by record ID and name.
#[derive(Debug, Default)]
pub(crate) struct FakeCells(BTreeMap<(u64, CellName), CellValue>);

impl CellSource for FakeCells {
    fn cell(&self, record_id: u64, name: &CellName) -> golemdb_cells::Result<Option<CellValue>> {
        Ok(self.0.get(&(record_id, name.clone())).cloned())
    }
}

/// Committed state as cells and the index over them, written together.
#[derive(Debug, Default)]
pub(crate) struct FakeState {
    pub(crate) index: FakeSource,
    pub(crate) cells: FakeCells,
}

impl FakeState {
    /// Store `value` as record `record_id`'s cell `name`, and index it as
    /// seal does: under its term when it is an attribute, not at all when
    /// it is a field. One cell per record and name.
    pub(crate) fn with(mut self, record_id: u64, name: &str, value: CellValue) -> Self {
        if let Some(term) = IndexTerm::from_cell(name, value.as_view()).expect("valid term") {
            let postings = self.index.0.entry(term).or_default();
            *postings |= bitmap([record_id]);
        }
        let key = (record_id, CellNameRef::raw(name.as_bytes()).into());
        assert!(
            self.cells.0.insert(key, value).is_none(),
            "one cell per record and name"
        );
        self
    }
}
