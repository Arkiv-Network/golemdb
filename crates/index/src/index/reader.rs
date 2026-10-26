use std::iter::{FusedIterator, Peekable};

use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{Entry, ReadTransaction, Scan, scan, scan_prefix};

use super::Index;
use crate::{
    Bitmap, BitmapContainer, INDEX_TRIE_PATH_BYTES, IndexTerm, Result, TermRange,
    index_trie::row_container, tables, term::key_bytes,
};

impl<H: HashProvider> Index<'_, H> {
    /// Validate a stored root, such as one read from `Roots`. Empty/singleton
    /// roots are checked against the container table from this snapshot. This
    /// does not audit all descendants of a branch root.
    pub fn validate_root(
        &self,
        tx: &impl ReadTransaction,
        root: RootRef<INDEX_TRIE_PATH_BYTES>,
    ) -> Result<()> {
        self.trie.validate_root(tx, root)
    }

    /// Materialize a term's postings from its container rows; absent terms
    /// return None, never an empty bitmap.
    pub fn bitmap(&self, tx: &impl ReadTransaction, term: &IndexTerm) -> Result<Option<Bitmap>> {
        let key = term.key();
        let containers = scan_prefix(tx, tables::INDEX, key.clone())?
            .map(|row| {
                let (row_key, value) = row?;
                let container = row_container(&row_key[key.len()..])?;
                Ok(BitmapContainer::decode_values(container, &value)?)
            })
            .collect::<Result<Vec<_>>>()?;
        if containers.is_empty() {
            return Ok(None);
        }
        Ok(Some(Bitmap::from_containers(containers)?))
    }

    /// Materialize the postings of a term; empty when the term is absent.
    pub fn postings(&self, tx: &impl ReadTransaction, term: &IndexTerm) -> Result<Bitmap> {
        Ok(self.bitmap(tx, term)?.unwrap_or_default())
    }

    /// Ordered scan of one name/type run, between the range's bounds: each
    /// term with its postings, in one pass over the container rows.
    pub fn terms<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        range: TermRange,
    ) -> Result<TermScan<'a, T>> {
        let (lower, upper) = range.into_key_bounds();
        Ok(TermScan::new(scan(tx, tables::INDEX, lower, upper)?))
    }

    /// Prefix from `IndexTerm::prefix`, optionally followed by an ordered value
    /// prefix (for example UTF-8 for a string). Empty prefix scans every term.
    pub fn terms_with_prefix<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        prefix: Vec<u8>,
    ) -> Result<TermScan<'a, T>> {
        Ok(TermScan::new(scan_prefix(
            tx,
            tables::INDEX,
            key_bytes(&prefix, false),
        )?))
    }
}

/// Lazy typed term scan over the container rows: one cursor walks the range,
/// and each term's adjacent rows are grouped into its postings. Any storage
/// or decoding error ends iteration.
pub struct TermScan<'a, T: ReadTransaction + 'a> {
    rows: Peekable<Scan<T::Cursor<'a>>>,
    done: bool,
}

impl<'a, T: ReadTransaction + 'a> TermScan<'a, T> {
    fn new(rows: Scan<T::Cursor<'a>>) -> Self {
        Self {
            rows: rows.peekable(),
            done: false,
        }
    }

    fn step(&mut self) -> Result<Option<(IndexTerm, Bitmap)>> {
        let Some(row) = self.rows.next() else {
            return Ok(None);
        };
        let (key, value) = row?;
        let (term, path) = IndexTerm::split_key(&key)?;
        let term_key = &key[..key.len() - path.len()];
        let mut containers = vec![BitmapContainer::decode_values(
            row_container(path)?,
            &value,
        )?];
        // The term key ends unambiguously, so its rows are those it prefixes.
        // An error may hide one of the term's rows, so it fails the term.
        let same_term = |row: &golemdb_storage::Result<Entry>| {
            row.as_ref()
                .map_or(true, |(next, _)| next.starts_with(term_key))
        };
        while let Some(row) = self.rows.next_if(same_term) {
            let (next, value) = row?;
            let container = row_container(&next[term_key.len()..])?;
            containers.push(BitmapContainer::decode_values(container, &value)?);
        }
        Ok(Some((term, Bitmap::from_containers(containers)?)))
    }
}

impl<'a, T: ReadTransaction + 'a> Iterator for TermScan<'a, T> {
    type Item = Result<(IndexTerm, Bitmap)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.step().transpose();
        self.done = !matches!(item, Some(Ok(_)));
        item
    }
}

impl<'a, T: ReadTransaction + 'a> FusedIterator for TermScan<'a, T> {}
