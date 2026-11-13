use std::iter::{FusedIterator, Peekable};

use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{Entry, ReadCursor, ReadTransaction, Scan, scan, scan_prefix};

use super::Index;
use crate::{
    Bitmap, BitmapContainer, INDEX_TRIE_PATH_BYTES, IndexKey, IndexTerm, Result, TermRange,
    history, key, tables,
};

/// Committed container reads from one caller-owned snapshot, without a hash
/// provider. These flat reads validate codecs but do not authenticate a root.
/// Historical term assembly requires discovery of both live and deleted
/// containers; this reader currently exposes only individual container reads.
pub struct IndexReader<'tx, R: ReadTransaction> {
    tx: &'tx R,
    target: Option<u64>,
}

impl<'tx, R: ReadTransaction> IndexReader<'tx, R> {
    pub fn new(tx: &'tx R) -> Self {
        Self { tx, target: None }
    }

    /// Read state after `commit`. The caller must validate target/head and
    /// retention coverage using this same snapshot before using the reader.
    pub fn at(tx: &'tx R, commit: u64) -> Self {
        Self {
            tx,
            target: Some(commit),
        }
    }

    pub fn container(&self, key: &IndexKey) -> Result<Option<BitmapContainer>> {
        self.with_encoded_container(key, |bytes| {
            bytes
                .map(|bytes| BitmapContainer::decode(key.container_id, bytes))
                .transpose()
                .map_err(Into::into)
        })?
    }

    /// Inspect raw bytes before validation or allocation. The callback may
    /// charge/refuse further work before decoding. An empty historical pre-image
    /// becomes `None`; a missing selected changeset is an error. Empty live rows
    /// remain `Some(empty)` so typed decoding rejects them as malformed.
    pub fn with_encoded_container<T>(
        &self,
        key: &IndexKey,
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> Result<T> {
        match self.target {
            None => Ok(self.tx.with_value(tables::INDEX, &key.encode(), inspect)?),
            Some(target) => {
                history::with_encoded_container(self.tx, &key.encode(), target, inspect)
            }
        }
    }
}

/// Whether `term` has postings: one seek to its first container row, decoding
/// only its key without copying the container, and without traversing the
/// trie. A row under `term` whose key is not an `Index` key is an error.
pub fn term_exists(tx: &impl ReadTransaction, term: &IndexTerm) -> Result<bool> {
    let term = term.as_bytes();
    tx.cursor(tables::INDEX, term)?.with_next(|row| match row {
        Some((key, _)) if key.starts_with(term) => IndexKey::decode(key).map(|_| true),
        _ => Ok(false),
    })?
}

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
        // No other term starts with this one, so the scan holds only its rows.
        let mut rows = TermScan::new(scan_prefix(tx, tables::INDEX, term.as_bytes().to_vec())?);
        Ok(rows.next().transpose()?.map(|(_, bitmap)| bitmap))
    }

    /// Materialize the postings of a term; empty when the term is absent.
    pub fn postings(&self, tx: &impl ReadTransaction, term: &IndexTerm) -> Result<Bitmap> {
        Ok(self.bitmap(tx, term)?.unwrap_or_default())
    }

    /// Ordered scan of the terms in `range`: each term with its postings, in
    /// one pass over the container rows.
    pub fn terms<'a, T: ReadTransaction>(
        &'a self,
        tx: &'a T,
        range: TermRange,
    ) -> Result<TermScan<T::Cursor<'a>>> {
        let (lower, upper) = key::bounds(range.bounds());
        Ok(TermScan::new(scan(tx, tables::INDEX, lower, upper)?))
    }
}

/// Lazy typed term scan over the container rows: one cursor walks the range,
/// and each term's adjacent rows are grouped into its postings. Any storage
/// or decoding error ends iteration.
pub struct TermScan<C: ReadCursor> {
    rows: Peekable<Scan<C>>,
    done: bool,
}

impl<C: ReadCursor> TermScan<C> {
    fn new(rows: Scan<C>) -> Self {
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
        let IndexKey { term, container_id } = IndexKey::decode(&key)?;
        let mut containers = vec![BitmapContainer::decode(container_id, &value)?];
        // The term ends unambiguously, so its rows are those it prefixes.
        // An error may hide one of the term's rows, so it fails the term.
        let same_term = |row: &golemdb_storage::Result<Entry>| {
            row.as_ref()
                .map_or(true, |(next, _)| next.starts_with(term.as_bytes()))
        };
        while let Some(row) = self.rows.next_if(same_term) {
            let (next, value) = row?;
            let container_id = key::container_id(&next[term.as_bytes().len()..])?;
            containers.push(BitmapContainer::decode(container_id, &value)?);
        }
        Ok(Some((term, Bitmap::from_containers(containers)?)))
    }
}

impl<C: ReadCursor> Iterator for TermScan<C> {
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

impl<C: ReadCursor> FusedIterator for TermScan<C> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PostingChange;
    use golemdb_cells::CellType;
    use golemdb_merkle::Keccak256Hasher;
    use golemdb_storage::{MemoryStore, Store, WriteTransaction};

    /// A term exists while it has a container, and a term whose value another
    /// term's value extends is told apart from it.
    #[test]
    fn term_presence_follows_its_containers() {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        let index = Index::new(&Keccak256Hasher);
        let term = |value: &[u8]| IndexTerm::new("color", CellType::Str, value).unwrap();
        let (short, long) = (term(b"blue"), term(b"blue\x01"));
        assert!(!term_exists(&tx, &short).unwrap());
        let add = PostingChange::Add {
            term: long.clone(),
            record_id: 1 << 20,
        };
        let root = index.apply(&mut tx, 1, RootRef::Empty, [add]).unwrap().root;
        assert!(term_exists(&tx, &long).unwrap());
        assert!(!term_exists(&tx, &short).unwrap());
        let stray = [short.as_bytes(), &[0]].concat();
        tx.put(tables::INDEX, &stray, &[]).unwrap();
        assert!(term_exists(&tx, &short).is_err());
        tx.delete(tables::INDEX, &stray).unwrap();
        let remove = PostingChange::Remove {
            term: long.clone(),
            record_id: 1 << 20,
        };
        index.apply(&mut tx, 2, root, [remove]).unwrap();
        assert!(!term_exists(&tx, &long).unwrap());
    }
}
