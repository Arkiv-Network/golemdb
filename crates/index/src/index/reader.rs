use std::iter::FusedIterator;

use golemdb_merkle::{Hash, HashProvider, RootRef};
use golemdb_storage::{ReadCursor, ReadTransaction, Scan, scan, scan_prefix};

use super::Index;
use crate::{
    Bitmap, INDEX_TRIE_PATH_BYTES, IndexTerm, Result, TermRange, index_trie::decode_root, tables,
};

impl<H: HashProvider> Index<'_, H> {
    /// Reconstruct current root metadata. Singleton recovery uses the flat table
    /// from this snapshot. This does not audit all descendants of a branch root.
    pub fn reopen(
        &self,
        tx: &impl ReadTransaction,
        hash: Hash,
    ) -> Result<RootRef<INDEX_TRIE_PATH_BYTES>> {
        self.trie.reopen(tx, hash)
    }

    /// Materialize a term's postings; absent terms return None, never an empty bitmap.
    pub fn bitmap(&self, tx: &impl ReadTransaction, term: &IndexTerm) -> Result<Option<Bitmap>> {
        self.bitmap_root(tx, term)?
            .map(|hash| self.bitmap_at(tx, hash))
            .transpose()
    }

    /// Materialize the postings under a bitmap root, as a [`TermScan`]
    /// yields it, without looking its term up again.
    pub fn bitmap_at(&self, tx: &impl ReadTransaction, root: Hash) -> Result<Bitmap> {
        self.bitmaps.bitmap(tx, root)
    }

    /// Ordered scan of one name/type run, between the range's bounds.
    /// This returns roots without materializing their bitmaps:
    /// see [`Self::bitmap_at`].
    pub fn terms<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        range: &TermRange,
    ) -> Result<TermScan<'a, T::Cursor<'a>, H>> {
        let (lower, upper) = range.bounds();
        Ok(TermScan {
            scan: scan(
                tx,
                tables::INDEX,
                lower.map(<[u8]>::to_vec),
                upper.map(<[u8]>::to_vec),
            )?,
            hasher: self.hasher,
            done: false,
        })
    }

    /// Prefix from `IndexTerm::prefix`, optionally followed by an ordered value
    /// prefix (for example UTF-8 for a string). Empty prefix scans every term.
    pub fn terms_with_prefix<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        prefix: Vec<u8>,
    ) -> Result<TermScan<'a, T::Cursor<'a>, H>> {
        Ok(TermScan {
            scan: scan_prefix(tx, tables::INDEX, prefix)?,
            hasher: self.hasher,
            done: false,
        })
    }
}

/// Lazy typed term scan; any storage or decoding error ends iteration.
pub struct TermScan<'h, C: ReadCursor, H: HashProvider> {
    scan: Scan<C>,
    hasher: &'h H,
    done: bool,
}

impl<C: ReadCursor, H: HashProvider> Iterator for TermScan<'_, C, H> {
    type Item = Result<(IndexTerm, Hash)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = self
            .scan
            .next()?
            .map_err(Into::into)
            .and_then(|(key, value)| {
                Ok((IndexTerm::decode(&key)?, decode_root(&value, self.hasher)?))
            });
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}

impl<C: ReadCursor, H: HashProvider> FusedIterator for TermScan<'_, C, H> {}
