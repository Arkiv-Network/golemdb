use std::{
    iter::{FusedIterator, Peekable},
    ops::Bound,
};

use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{Entry, ReadCursor, ReadTransaction, Scan, scan, scan_prefix};

use super::Index;
use crate::{
    Bitmap, BitmapContainer, INDEX_TRIE_PATH_BYTES, IndexKey, IndexTerm, Regions, Result,
    TermRange, key, tables,
};

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

    /// [`Self::postings`] confined to `regions`: the term's containers in
    /// them, and no other is read. Equal to `postings(term).within(regions)`.
    pub fn postings_within(
        &self,
        tx: &impl ReadTransaction,
        term: &IndexTerm,
        regions: &Regions,
    ) -> Result<Bitmap> {
        Ok(Bitmap::from_containers(containers_within(
            tx, term, regions,
        )?)?)
    }

    /// [`Self::terms`] with each term's postings confined to `regions`: every
    /// term in `range`, in order, with its containers in them. Each term
    /// costs one seek to its first row, then [`Self::postings_within`]'s
    /// reads; its other containers are skipped unread.
    pub fn terms_within<'t, 'r, T: ReadTransaction>(
        &self,
        tx: &'t T,
        range: TermRange,
        regions: &'r Regions,
    ) -> TermsWithin<'t, 'r, T> {
        let (next, upper) = key::bounds(range.bounds());
        TermsWithin {
            tx,
            next,
            upper,
            regions,
            done: false,
        }
    }
}

/// The containers of `term` in `regions`, ascending: a leapfrog over the
/// term's rows. A seek to the next wanted region skips the rows below it
/// unread, and a row of a region not wanted is passed over, not decoded. So
/// it seeks at most once per wanted region and reads at most one row per
/// seek or per container taken, however many containers the term has.
fn containers_within(
    tx: &impl ReadTransaction,
    term: &IndexTerm,
    regions: &Regions,
) -> Result<Vec<BitmapContainer>> {
    let mut found = Vec::new();
    let [first, ..] = regions.as_slice() else {
        return Ok(found);
    };
    let mut wanted = regions.as_slice().iter().copied().peekable();
    let mut cursor = tx.cursor(tables::INDEX, &key::encode(term, *first))?;
    while let Some(&want) = wanted.peek() {
        let Some((row, value)) = cursor.next()? else {
            break;
        };
        // No term is a prefix of another: a row it does not prefix is past it.
        let Some(hi48) = row.strip_prefix(term.as_bytes()) else {
            break;
        };
        let region = key::container_id(hi48)?;
        if region < want {
            cursor = tx.cursor(tables::INDEX, &key::encode(term, want))?;
            continue;
        }
        while wanted.next_if(|wanted| *wanted < region).is_some() {}
        if wanted.next_if_eq(&region).is_some() {
            found.push(BitmapContainer::decode(region, &value)?);
        }
    }
    Ok(found)
}

/// Lazy term scan with each term's postings confined to a set of regions
/// ([`Index::terms_within`]). Any storage or decoding error ends iteration.
pub struct TermsWithin<'t, 'r, T: ReadTransaction> {
    tx: &'t T,
    /// The lower bound of the next term's rows.
    next: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
    regions: &'r Regions,
    done: bool,
}

impl<T: ReadTransaction> TermsWithin<'_, '_, T> {
    fn step(&mut self) -> Result<Option<(IndexTerm, Bitmap)>> {
        // Key bounds of a term range never cross, and the next term starts
        // past the last one's rows, so `next` passes `upper` only at the end.
        let past_the_end = match (&self.next, &self.upper) {
            (Bound::Included(next), Bound::Excluded(upper)) => next >= upper,
            (Bound::Included(next), Bound::Included(upper)) => next > upper,
            _ => false,
        };
        if past_the_end {
            return Ok(None);
        }
        let first = scan(
            self.tx,
            tables::INDEX,
            self.next.clone(),
            self.upper.clone(),
        )?
        .next()
        .transpose()?;
        let Some((row, _)) = first else {
            return Ok(None);
        };
        let IndexKey { term, .. } = IndexKey::decode(&row)?;
        let containers = containers_within(self.tx, &term, self.regions)?;
        self.next = key::after(term.as_bytes());
        Ok(Some((term, Bitmap::from_containers(containers)?)))
    }
}

impl<T: ReadTransaction> Iterator for TermsWithin<'_, '_, T> {
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

impl<T: ReadTransaction> FusedIterator for TermsWithin<'_, '_, T> {}

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
    use std::cell::Cell;

    use proptest::prelude::*;

    use super::*;
    use crate::{ContainerId, PostingChange};
    use golemdb_cells::CellType;
    use golemdb_merkle::Keccak256Hasher;
    use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction};

    fn tag(value: u8) -> IndexTerm {
        IndexTerm::new("tag", CellType::Str, &[b'a' + value]).unwrap()
    }

    /// A store whose index holds `postings`, per [`tag`] value.
    fn store_of(postings: &[(u8, Vec<u64>)]) -> MemoryStore {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        let changes = postings.iter().flat_map(|(value, ids)| {
            ids.iter().map(|&record_id| PostingChange::Add {
                term: tag(*value),
                record_id,
            })
        });
        Index::new(&Keccak256Hasher)
            .apply(&mut tx, 1, RootRef::Empty, changes)
            .unwrap();
        tx.commit().unwrap();
        store
    }

    /// The regions of `ids`.
    fn regions_of(ids: impl IntoIterator<Item = u64>) -> Regions {
        let mut bitmap = Bitmap::default();
        for id in ids {
            let (region, offset) = ContainerId::split(id);
            bitmap |=
                Bitmap::from_containers([BitmapContainer::from_values(region, [offset])]).unwrap();
        }
        Regions::of(&bitmap)
    }

    /// A transaction that counts its cursors, each a seek, and the rows they
    /// return.
    struct Counted<'a, T> {
        tx: &'a T,
        seeks: Cell<usize>,
        rows: &'a Cell<usize>,
    }

    struct CountedCursor<'a, C> {
        cursor: C,
        rows: &'a Cell<usize>,
    }

    impl<T: ReadTransaction> ReadTransaction for Counted<'_, T> {
        type Cursor<'c>
            = CountedCursor<'c, T::Cursor<'c>>
        where
            Self: 'c;

        fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
            self.tx.get(table, key)
        }

        fn with_value<O>(
            &self,
            table: Table,
            key: &[u8],
            inspect: impl FnOnce(Option<&[u8]>) -> O,
        ) -> golemdb_storage::Result<O> {
            self.tx.with_value(table, key, inspect)
        }

        fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
            self.seeks.set(self.seeks.get() + 1);
            Ok(CountedCursor {
                cursor: self.tx.cursor(table, key)?,
                rows: self.rows,
            })
        }
    }

    impl<C: ReadCursor> ReadCursor for CountedCursor<'_, C> {
        fn next(&mut self) -> golemdb_storage::Result<Option<Entry>> {
            let row = self.cursor.next()?;
            self.rows.set(self.rows.get() + usize::from(row.is_some()));
            Ok(row)
        }

        fn prev(&mut self) -> golemdb_storage::Result<Option<Entry>> {
            self.cursor.prev()
        }

        fn with_next<O>(
            &mut self,
            inspect: impl FnOnce(Option<(&[u8], &[u8])>) -> O,
        ) -> golemdb_storage::Result<O> {
            let rows = self.rows;
            self.cursor.with_next(|row| {
                rows.set(rows.get() + usize::from(row.is_some()));
                inspect(row)
            })
        }
    }

    #[test]
    fn a_confined_read_seeks_only_to_the_wanted_regions() {
        // One container in each of 150 regions.
        let ids: Vec<u64> = (0..150).map(|region| region << 16).collect();
        let store = store_of(&[(0, ids.clone())]);
        let read = store.begin_read().unwrap();
        let index = Index::new(&Keccak256Hasher);
        for (wanted, max_seeks, max_rows) in [
            // One region: one seek, one row, of 150.
            (vec![77 << 16], 1, 1),
            // Consecutive regions: one seek, then one row each.
            ((10..20).map(|region| region << 16).collect(), 1, 10),
            // Scattered regions, and one the term has no container in.
            (vec![0, 50 << 16, 100 << 16, 149 << 16, 500 << 16], 5, 10),
            (vec![], 0, 0),
        ] {
            let regions = regions_of(wanted.iter().copied());
            let rows = Cell::new(0);
            let counted = Counted {
                tx: &read,
                seeks: Cell::new(0),
                rows: &rows,
            };
            let postings = index.postings_within(&counted, &tag(0), &regions).unwrap();
            let expected: Vec<u64> = wanted
                .iter()
                .copied()
                .filter(|id| ids.contains(id))
                .collect();
            assert_eq!(postings.treemap().iter().collect::<Vec<_>>(), expected);
            assert!(
                counted.seeks.get() <= max_seeks,
                "{wanted:?}: {} seeks",
                counted.seeks.get()
            );
            assert!(rows.get() <= max_rows, "{wanted:?}: {} rows", rows.get());
        }
    }

    /// Postings of up to four terms, over IDs in a few regions and a few far
    /// ones, so terms and regions overlap often.
    fn postings() -> impl Strategy<Value = Vec<(u8, Vec<u64>)>> {
        let id = prop_oneof![0..8u64 << 16, any::<u64>()];
        prop::collection::btree_map(0..4u8, prop::collection::btree_set(id, 1..40), 0..4).prop_map(
            |terms| {
                terms
                    .into_iter()
                    .map(|(value, ids)| (value, ids.into_iter().collect()))
                    .collect()
            },
        )
    }

    proptest! {
        #[test]
        fn confined_reads_are_full_reads_within_the_regions(
            postings in postings(),
            wanted in prop::collection::vec(prop_oneof![0..8u64 << 16, any::<u64>()], 0..12),
        ) {
            let store = store_of(&postings);
            let read = store.begin_read().unwrap();
            let index = Index::new(&Keccak256Hasher);
            let regions = regions_of(wanted);
            for value in 0..5 {
                prop_assert_eq!(
                    index.postings_within(&read, &tag(value), &regions).unwrap(),
                    index.postings(&read, &tag(value)).unwrap().within(&regions)
                );
            }
            let full: Vec<_> = index
                .terms(&read, TermRange::all())
                .unwrap()
                .map(|entry| entry.map(|(term, bitmap)| (term, bitmap.within(&regions))))
                .collect::<Result<_>>()
                .unwrap();
            let confined: Vec<_> = index
                .terms_within(&read, TermRange::all(), &regions)
                .collect::<Result<_>>()
                .unwrap();
            prop_assert_eq!(confined, full);
        }
    }

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
