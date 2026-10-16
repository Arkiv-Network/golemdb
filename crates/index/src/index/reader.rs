use std::iter::FusedIterator;

use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{ReadCursor, ReadTransaction, Scan, scan, scan_prefix};

use super::Index;
use crate::{
    BITMAP_TRIE_PATH_BYTES, Bitmap, INDEX_TRIE_PATH_BYTES, IndexTerm, Result, TermRange,
    bitmap_trie::BitmapTrie, index_trie::decode_root, tables,
};

impl<H: HashProvider> Index<'_, H> {
    /// Validate a stored root, such as one read from `Roots`. Empty/singleton
    /// roots are checked against the flat table from this snapshot. This does
    /// not audit all descendants of a branch root.
    pub fn validate_root(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
    ) -> Result<()> {
        self.trie.validate_root(tx, root)
    }

    /// Materialize a term's postings; absent terms return None, never an empty bitmap.
    pub fn bitmap(&self, tx: &impl ReadTransaction, term: &IndexTerm) -> Result<Option<Bitmap>> {
        self.bitmap_root(tx, term)?
            .map(|root| self.bitmap_at(tx, term, root))
            .transpose()
    }

    /// Materialize the postings under a term's bitmap root, as a [`TermScan`]
    /// yields it, without looking its term up again. The root is a locator
    /// (hash and commit); the term supplies the rows' namespace.
    pub fn bitmap_at(
        &self,
        tx: &impl ReadTransaction,
        term: &IndexTerm,
        root: RootRef<BITMAP_TRIE_PATH_BYTES>,
    ) -> Result<Bitmap> {
        let term_path = term.routing_path(self.hasher);
        BitmapTrie::new(self.hasher, &term_path).bitmap(tx, root)
    }

    /// Ordered scan of one name/type run, between the range's bounds.
    /// This returns roots without materializing their bitmaps:
    /// see [`Self::bitmap_at`].
    pub fn terms<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        range: TermRange,
    ) -> Result<TermScan<T::Cursor<'a>>> {
        let (lower, upper) = range.into_bounds();
        Ok(TermScan {
            scan: scan(tx, tables::INDEX, lower, upper)?,
            done: false,
        })
    }

    /// Prefix from `IndexTerm::prefix`, optionally followed by an ordered value
    /// prefix (for example UTF-8 for a string). Empty prefix scans every term.
    pub fn terms_with_prefix<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        prefix: Vec<u8>,
    ) -> Result<TermScan<T::Cursor<'a>>> {
        Ok(TermScan {
            scan: scan_prefix(tx, tables::INDEX, prefix)?,
            done: false,
        })
    }
}

/// Lazy typed term scan; any storage or decoding error ends iteration.
pub struct TermScan<C: ReadCursor> {
    scan: Scan<C>,
    done: bool,
}

impl<C: ReadCursor> Iterator for TermScan<C> {
    type Item = Result<(IndexTerm, RootRef<BITMAP_TRIE_PATH_BYTES>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = self
            .scan
            .next()?
            .map_err(Into::into)
            .and_then(|(key, value)| Ok((IndexTerm::decode(&key)?, decode_root(&value)?)));
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}

impl<C: ReadCursor> FusedIterator for TermScan<C> {}
