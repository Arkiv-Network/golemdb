use golemdb_history::{HistoryError, ReadOperation, ValueSource};
use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{ReadTransaction, scan_prefix};

use super::Index;
use crate::{
    Bitmap, BitmapContainer, ContainerScan, INDEX_TRIE_PATH_BYTES, IndexKey, IndexTerm, Result,
    TermRange, TermScan, tables,
};

/// Committed container reads from one caller-owned snapshot, without a hash
/// provider. These flat reads validate codecs but do not authenticate a root.
/// Historical scans discover both live and deleted containers and resolve them
/// independently before assembling each term's postings.
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

    /// Inspect selected borrowed bytes without work callbacks. See
    /// [`Self::with_encoded_container_with_callbacks`] for upfront work authorization.
    pub fn with_encoded_container<T>(
        &self,
        key: &IndexKey,
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> Result<T> {
        self.with_encoded_container_with_callbacks(
            key,
            &mut (),
            |_, _| Ok(()),
            |_, bytes| Ok(inspect(bytes)),
        )
    }

    /// Inspect borrowed bytes before domain parsing/copying. `before` authorizes
    /// each lookup and commit-set package; a callback returning `Ok(())` permits
    /// internal reads without charging. Empty pre-images mean absence; missing
    /// selected changesets fail and empty live rows remain visible to validation.
    pub fn with_encoded_container_with_callbacks<C, T, E>(
        &self,
        key: &IndexKey,
        context: &mut C,
        mut before: impl FnMut(&mut C, ReadOperation) -> std::result::Result<(), E>,
        inspect: impl FnOnce(&mut C, Option<&[u8]>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<crate::IndexError> + From<HistoryError>,
    {
        let encoded = key.encode();
        let key = encoded.as_slice();
        let Some(target) = self.target else {
            before(context, ReadOperation::Lookup(tables::INDEX))?;
            return self
                .tx
                .with_value(tables::INDEX, key, |raw| inspect(context, raw))
                .map_err(crate::IndexError::from)?;
        };
        tables::HISTORY.with_resolved_with_callbacks(
            self.tx,
            key,
            target,
            &mut (context, &mut before),
            |(context, before), work| before(context, work),
            |(context, before), source| match source {
                ValueSource::Preimage(bytes) => {
                    inspect(context, (!bytes.is_empty()).then_some(bytes))
                }
                ValueSource::Live => {
                    before(context, ReadOperation::Lookup(tables::INDEX))?;
                    self.tx
                        .with_value(tables::INDEX, key, |raw| inspect(context, raw))
                        .map_err(crate::IndexError::from)?
                }
            },
        )
    }

    /// Present containers of exactly one term, in `hi48` order. Also discovers
    /// containers deleted since the target. No rows are fetched at construction.
    pub fn containers_for_term(&self, term: &IndexTerm) -> Result<ContainerScan<'tx, R>> {
        ContainerScan::for_term(self.tx, self.target, term)
    }

    /// Present containers in a term range, ordered by `(term, hi48)`.
    /// Use the stream's work hooks before decoding or assembling large postings.
    pub fn containers_for_range(&self, range: TermRange) -> Result<ContainerScan<'tx, R>> {
        ContainerScan::for_range(self.tx, self.target, range)
    }

    /// A term exists if at least one container is present at the selected target.
    /// Stops after the first present container, validating its canonical payload.
    pub fn term_exists(&self, term: &IndexTerm) -> Result<bool> {
        Ok(self
            .containers_for_term(term)?
            .next()
            .transpose()?
            .is_some())
    }

    /// Assemble an exact term's postings. Absent terms return `None`.
    pub fn bitmap(&self, term: &IndexTerm) -> Result<Option<Bitmap>> {
        Ok(TermScan::new(self.containers_for_term(term)?)
            .next()
            .transpose()?
            .map(|(_, bitmap)| bitmap))
    }

    /// Assemble an exact term's postings; empty when the term is absent.
    pub fn postings(&self, term: &IndexTerm) -> Result<Bitmap> {
        Ok(self.bitmap(term)?.unwrap_or_default())
    }

    /// Each present term in the range with its bitmap, in term order.
    pub fn terms(&self, range: TermRange) -> Result<TermScan<'tx, R>> {
        Ok(TermScan::new(self.containers_for_range(range)?))
    }
}

/// Whether `term` has postings: one seek to its first container row, decoding
/// only its key, without traversing the trie. A row whose key is not an
/// `Index` key of `term` is an error.
pub fn term_exists(tx: &impl ReadTransaction, term: &IndexTerm) -> Result<bool> {
    match scan_prefix(tx, tables::INDEX, term.as_bytes().to_vec())?
        .next()
        .transpose()?
    {
        None => Ok(false),
        Some((row, _)) => {
            IndexKey::decode(&row)?;
            Ok(true)
        }
    }
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
        IndexReader::new(tx).bitmap(term)
    }

    /// Materialize the postings of a term; empty when the term is absent.
    pub fn postings(&self, tx: &impl ReadTransaction, term: &IndexTerm) -> Result<Bitmap> {
        IndexReader::new(tx).postings(term)
    }

    /// Ordered scan of the terms in `range`: each term with its postings, in
    /// one pass over the container rows.
    pub fn terms<'tx, R: ReadTransaction>(
        &self,
        tx: &'tx R,
        range: TermRange,
    ) -> Result<TermScan<'tx, R>> {
        IndexReader::new(tx).terms(range)
    }
}

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
