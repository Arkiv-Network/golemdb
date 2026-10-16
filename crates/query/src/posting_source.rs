//! The storage seam: everything the evaluator reads goes through
//! [`PostingSource`], so query logic is testable without a database.

use golemdb_index::{Bitmap, Index, IndexTerm, TermRange};
use golemdb_merkle::HashProvider;
use golemdb_storage::ReadTransaction;

/// Read access to the index's posting lists.
pub(crate) trait PostingSource {
    /// The record IDs indexed under `term`; empty when the term is absent.
    fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap>;

    /// The postings of every term in `range`, one bitmap per term, in
    /// ascending key order. Lazy: each term is stepped to and read as the
    /// iterator advances.
    fn scan(
        &self,
        range: TermRange,
    ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>>;
}

/// [`PostingSource`] over an [`Index`] at one read snapshot. Use one snapshot
/// for a whole query, so every leaf sees the same state.
pub(crate) struct IndexSource<'a, 'h, H: HashProvider, T: ReadTransaction> {
    index: &'a Index<'h, H>,
    tx: &'a T,
}

impl<'a, 'h, H: HashProvider, T: ReadTransaction> IndexSource<'a, 'h, H, T> {
    pub(crate) fn new(index: &'a Index<'h, H>, tx: &'a T) -> Self {
        Self { index, tx }
    }
}

impl<H: HashProvider, T: ReadTransaction> PostingSource for IndexSource<'_, '_, H, T> {
    fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<Bitmap> {
        Ok(self.index.bitmap(self.tx, term)?.unwrap_or_default())
    }

    fn scan(
        &self,
        range: TermRange,
    ) -> golemdb_index::Result<impl Iterator<Item = golemdb_index::Result<Bitmap>>> {
        Ok(self.index.terms(self.tx, range)?.map(|entry| {
            let (term, root) = entry?;
            self.index.bitmap_at(self.tx, &term, root)
        }))
    }
}
