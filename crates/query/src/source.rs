//! The storage seam: everything the evaluator reads goes through
//! [`PostingSource`], so query logic is testable without a database.

use golemdb_index::{Bitmap, Index, IndexTerm};
use golemdb_merkle::HashProvider;
use golemdb_storage::ReadTransaction;
use roaring::RoaringTreemap;

/// Read access to the index's posting lists.
pub trait PostingSource {
    /// The record IDs indexed under `term`; empty when the term is absent.
    fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<RoaringTreemap>;
}

/// [`PostingSource`] over an [`Index`] at one read snapshot. Use one snapshot
/// for a whole query, so every predicate sees the same state.
pub struct IndexSource<'a, 'h, H: HashProvider, T: ReadTransaction> {
    index: &'a Index<'h, H>,
    tx: &'a T,
}

impl<'a, 'h, H: HashProvider, T: ReadTransaction> IndexSource<'a, 'h, H, T> {
    pub fn new(index: &'a Index<'h, H>, tx: &'a T) -> Self {
        Self { index, tx }
    }
}

impl<H: HashProvider, T: ReadTransaction> PostingSource for IndexSource<'_, '_, H, T> {
    fn postings(&self, term: &IndexTerm) -> golemdb_index::Result<RoaringTreemap> {
        Ok(self
            .index
            .bitmap(self.tx, term)?
            .map(Bitmap::into_treemap)
            .unwrap_or_default())
    }
}
