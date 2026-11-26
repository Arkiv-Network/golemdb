use std::{
    iter::{FusedIterator, Peekable},
    ops::Bound,
};

use golemdb_merkle::{HashProvider, RootRef};
use golemdb_storage::{Entry, ReadCursor, ReadTransaction, Scan, scan, scan_prefix};

use super::Index;
use crate::{
    Bitmap, BitmapContainer, ContainerId, INDEX_TRIE_PATH_BYTES, IndexKey, IndexTerm, Regions,
    Result, TermRange, key, tables,
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
        let (start, upper) = key::bounds(range.bounds());
        Ok(TermScan::new(scan(
            tx,
            tables::INDEX,
            Bound::Included(start),
            upper,
        )?))
    }

    /// [`Self::postings`] confined to `regions`: the term's containers in
    /// them, and no other is decoded. Equal to
    /// `postings(term).within(regions)`.
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
    /// term in `range`, in order, with its containers in them. One cursor
    /// walks the range, as for [`Self::terms`], and each term is read as
    /// [`Self::postings_within`] reads it, but on from the row that ended the
    /// term before it, which, wanted or not, never makes the scan seek. Past
    /// a term's last wanted region, the scan steps on, and seeks past the
    /// term only once it passes over another of the term's rows.
    pub fn terms_within<'t, 'r, T: ReadTransaction>(
        &self,
        tx: &'t T,
        range: TermRange,
        regions: &'r Regions,
    ) -> Result<TermsWithin<'t, 'r, T>> {
        let (start, upper) = key::bounds(range.bounds());
        Ok(TermsWithin {
            tx,
            cursor: tx.cursor(tables::INDEX, &start)?,
            upper,
            regions: regions.as_slice(),
            current: None,
            ended: false,
        })
    }
}

/// A term's read confined to some regions, partway through: the containers
/// taken so far, and the wanted regions still ahead.
struct Confined<'r> {
    /// The wanted regions past the last row read, ascending.
    rest: &'r [ContainerId],
    found: Vec<BitmapContainer>,
    /// Whether the last row read was passed over, so the read seeks to the
    /// next wanted region before reading on.
    behind: bool,
}

impl<'r> Confined<'r> {
    fn new(regions: &'r [ContainerId]) -> Self {
        Self {
            rest: regions,
            found: Vec::new(),
            behind: false,
        }
    }

    /// A scanned term's read, begun at its first row, in `region`, whose
    /// stored container is `value`. Even passed over, that row does not make
    /// the read seek: a term of one container ends at the next row, which
    /// stepping on reaches without one.
    fn starting_at(regions: &'r [ContainerId], region: ContainerId, value: &[u8]) -> Result<Self> {
        let mut read = Self::new(regions);
        read.read(region, value)?;
        read.behind = false;
        Ok(read)
    }

    /// Read the term's row in `region`, whose stored container is `value`:
    /// decoded when the region is wanted, otherwise passed over unread.
    fn read(&mut self, region: ContainerId, value: &[u8]) -> Result<()> {
        let rest: &'r [ContainerId] = self.rest;
        let (_, rest) = rest.split_at(rest.partition_point(|wanted| *wanted < region));
        match rest {
            [wanted, past @ ..] if *wanted == region => {
                self.found.push(BitmapContainer::decode(region, value)?);
                self.rest = past;
                self.behind = false;
            }
            _ => {
                self.rest = rest;
                self.behind = true;
            }
        }
        Ok(())
    }
}

/// The containers of `term` in `regions`, ascending: a leapfrog over the
/// term's rows, from its first wanted region. Each row is read in place: a
/// row of a region not wanted is passed over, its container neither copied
/// nor decoded, and the read then seeks to the next wanted region, skipping
/// the rows below it unread. It ends past the last wanted region or the
/// term's last row. So it seeks at most once per wanted region and passes
/// over at most one row per wanted region, however many containers the term
/// has.
fn containers_within(
    tx: &impl ReadTransaction,
    term: &IndexTerm,
    regions: &Regions,
) -> Result<Vec<BitmapContainer>> {
    let mut read = Confined::new(regions.as_slice());
    let &[first, ..] = read.rest else {
        return Ok(read.found);
    };
    let mut cursor = tx.cursor(tables::INDEX, &key::encode(term, first))?;
    while let &[want, ..] = read.rest {
        if read.behind {
            cursor = tx.cursor(tables::INDEX, &key::encode(term, want))?;
            read.behind = false;
        }
        let of_term = cursor.with_next(|row| {
            // No term is a prefix of another: a row it does not prefix is
            // past it.
            match row.and_then(|(key, value)| Some((key.strip_prefix(term.as_bytes())?, value))) {
                Some((hi48, value)) => read.read(key::container_id(hi48)?, value).map(|()| true),
                None => Ok(false),
            }
        })??;
        if !of_term {
            break;
        }
    }
    Ok(read.found)
}

/// Lazy term scan with each term's postings confined to a set of regions
/// ([`Index::terms_within`]). One cursor walks the range, seeking only to
/// skip rows; the row that ends a term starts the next. Any storage or
/// decoding error ends iteration.
pub struct TermsWithin<'t, 'r, T: ReadTransaction> {
    tx: &'t T,
    cursor: T::Cursor<'t>,
    upper: Bound<Vec<u8>>,
    regions: &'r [ContainerId],
    /// The term being read, from its first row on.
    current: Option<(IndexTerm, Confined<'r>)>,
    /// Whether no row is left in the range past `current`'s.
    ended: bool,
}

/// What the row a confined scan just read is.
enum Next<'r> {
    /// One more of the current term's rows, read into its read.
    Same,
    /// The first row of the next term, read into a read of it.
    Term(IndexTerm, Confined<'r>),
    /// No row, or one past the range: the scan is over.
    End,
}

impl<T: ReadTransaction> TermsWithin<'_, '_, T> {
    fn step(&mut self) -> Result<Option<(IndexTerm, Bitmap)>> {
        while !self.ended {
            if let Some((term, read)) = &mut self.current
                && read.behind
            {
                let target = match read.rest {
                    &[want, ..] => key::encode(term, want),
                    [] => key::after(term.as_bytes()),
                };
                self.cursor = self.tx.cursor(tables::INDEX, &target)?;
                read.behind = false;
            }
            let (current, upper, regions) = (&mut self.current, &self.upper, self.regions);
            let next = self
                .cursor
                .with_next(|row| next_row(row, current.as_mut(), upper, regions))??;
            match next {
                Next::Same => {}
                Next::Term(term, read) => {
                    if let Some(done) = self.current.replace((term, read)) {
                        return finish(done).map(Some);
                    }
                }
                Next::End => self.ended = true,
            }
        }
        self.current.take().map(finish).transpose()
    }
}

/// Classify `row` for a confined scan, and read it into the term it belongs
/// to: `current`, or the next term, with `regions` wanted, while it is below
/// `upper`.
fn next_row<'r>(
    row: Option<(&[u8], &[u8])>,
    current: Option<&mut (IndexTerm, Confined<'r>)>,
    upper: &Bound<Vec<u8>>,
    regions: &'r [ContainerId],
) -> Result<Next<'r>> {
    let Some((key, value)) = row else {
        return Ok(Next::End);
    };
    // No term is a prefix of another: a row the term prefixes is its own.
    if let Some((term, read)) = current
        && let Some(hi48) = key.strip_prefix(term.as_bytes())
    {
        read.read(key::container_id(hi48)?, value)?;
        return Ok(Next::Same);
    }
    let below_upper = match upper {
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Excluded(end) => key < end.as_slice(),
        Bound::Unbounded => true,
    };
    if !below_upper {
        return Ok(Next::End);
    }
    let IndexKey { term, container_id } = IndexKey::decode(key)?;
    let read = Confined::starting_at(regions, container_id, value)?;
    Ok(Next::Term(term, read))
}

/// A term whose confined read is done, with its postings.
fn finish((term, read): (IndexTerm, Confined<'_>)) -> Result<(IndexTerm, Bitmap)> {
    Ok((term, Bitmap::from_containers(read.found)?))
}

impl<T: ReadTransaction> Iterator for TermsWithin<'_, '_, T> {
    type Item = Result<(IndexTerm, Bitmap)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.step();
        if item.is_err() {
            // An error may hide rows of the term being read: end the scan
            // rather than read on.
            self.ended = true;
            self.current = None;
        }
        item.transpose()
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
    use golemdb_cells::{CellNameRef, CellType, CellValue};
    use golemdb_merkle::Keccak256Hasher;
    use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction};

    fn tag(value: u8) -> IndexTerm {
        IndexTerm::new("tag", CellType::Str, &[b'a' + value]).unwrap()
    }

    /// A term of `tagz`, a name whose terms all sort past every term of
    /// [`tag`]: a range over `tag` ends at one of its rows, not at the
    /// table's end.
    fn neighbour(value: u8) -> IndexTerm {
        IndexTerm::new("tagz", CellType::Str, &[b'a' + value]).unwrap()
    }

    /// The `str` terms of [`tag`] between the values `lower` and `upper`,
    /// `None` when the bounds admit none.
    fn tag_range(lower: Bound<u8>, upper: Bound<u8>) -> Option<TermRange> {
        let value = |n: u8| CellValue::from_str(&char::from(b'a' + n).to_string());
        let (lower, upper) = (lower.map(value), upper.map(value));
        TermRange::between(
            CellNameRef::raw(b"tag"),
            CellType::Str,
            lower.as_ref().map(CellValue::as_view),
            upper.as_ref().map(CellValue::as_view),
        )
        .unwrap()
    }

    /// A store whose index holds `postings`, per [`tag`] value.
    fn store_of(postings: &[(u8, Vec<u64>)]) -> MemoryStore {
        store_of_terms(
            postings
                .iter()
                .map(|(value, ids)| (tag(*value), ids.as_slice())),
        )
    }

    /// A store whose index holds `postings`, per term.
    fn store_of_terms<'a>(
        postings: impl IntoIterator<Item = (IndexTerm, &'a [u64])>,
    ) -> MemoryStore {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        let changes = postings.into_iter().flat_map(|(term, ids)| {
            ids.iter().map(move |&record_id| PostingChange::Add {
                term: term.clone(),
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
        for (wanted, seeks, rows) in [
            // One region: one seek, one row, of 150.
            (vec![77 << 16], 1, 1),
            // Consecutive regions: one seek, then one row each.
            ((10..20).map(|region| region << 16).collect(), 1, 10),
            // Every other region: a seek to each, after the row passed over
            // below it.
            ((0..75).map(|region| region << 17).collect(), 75, 149),
            // Scattered regions, and one past the term's last container:
            // after each taken, one row passed over, then a seek to the next;
            // the term's end ends the read.
            (vec![0, 50 << 16, 100 << 16, 149 << 16, 500 << 16], 4, 7),
            (vec![], 0, 0),
        ] {
            let regions = regions_of(wanted.iter().copied());
            let counted_rows = Cell::new(0);
            let counted = Counted {
                tx: &read,
                seeks: Cell::new(0),
                rows: &counted_rows,
            };
            let postings = index.postings_within(&counted, &tag(0), &regions).unwrap();
            let expected: Vec<u64> = wanted
                .iter()
                .copied()
                .filter(|id| ids.contains(id))
                .collect();
            assert_eq!(postings.treemap().iter().collect::<Vec<_>>(), expected);
            assert_eq!(counted.seeks.get(), seeks, "{wanted:?}: seeks");
            assert_eq!(counted_rows.get(), rows, "{wanted:?}: rows");
        }
    }

    #[test]
    fn a_confined_scan_carries_one_cursor_across_its_terms() {
        let one_each: Vec<_> = (0..20)
            .map(|value| (value, vec![u64::from(value) << 16]))
            .collect();
        let regions: Vec<u64> = (0..150).map(|region| region << 16).collect();
        let dense: Vec<_> = (0..3).map(|value| (value, regions.clone())).collect();
        for (postings, wanted, seeks, rows) in [
            // One container per term: the scan's one seek, then one row per
            // term, wanted or not; a term's first row never seeks.
            (&one_each, 5 << 16, 1, 20),
            // Many containers per term, the first wanted: one row passed over
            // past it, then a seek past the term.
            (&dense, 0, 1 + 3, 3 * 2),
            // Many containers per term, the last wanted: the first row and
            // one more passed over below it, then a seek to it; its row past
            // the term starts the next.
            (&dense, 149 << 16, 1 + 3, 3 * 3),
        ] {
            let store = store_of(postings);
            let read = store.begin_read().unwrap();
            let index = Index::new(&Keccak256Hasher);
            let regions = regions_of([wanted]);
            let counted_rows = Cell::new(0);
            let counted = Counted {
                tx: &read,
                seeks: Cell::new(0),
                rows: &counted_rows,
            };
            let confined: Vec<_> = index
                .terms_within(&counted, TermRange::all(), &regions)
                .unwrap()
                .collect::<Result<_>>()
                .unwrap();
            let full: Vec<_> = index
                .terms(&read, TermRange::all())
                .unwrap()
                .map(|entry| entry.map(|(term, bitmap)| (term, bitmap.within(&regions))))
                .collect::<Result<_>>()
                .unwrap();
            assert_eq!(confined, full, "{wanted}");
            assert_eq!(counted.seeks.get(), seeks, "{wanted}: seeks");
            assert_eq!(counted_rows.get(), rows, "{wanted}: rows");
        }
    }

    #[test]
    fn a_confined_scan_ends_at_the_first_row_past_its_range() {
        // Terms `a`, `b` and `c`, each with one container in each of 150
        // regions. The range holds `a` and `b`, so `c`'s rows lie past it.
        let regions: Vec<u64> = (0..150).map(|region| region << 16).collect();
        let dense: Vec<_> = (0..3).map(|value| (value, regions.clone())).collect();
        let store = store_of(&dense);
        let read = store.begin_read().unwrap();
        let index = Index::new(&Keccak256Hasher);
        for range in [
            tag_range(Bound::Included(0), Bound::Included(1)),
            tag_range(Bound::Unbounded, Bound::Excluded(2)),
        ] {
            let range = range.expect("the bounds admit `a` and `b`");
            for (wanted, seeks, rows) in [
                // Past `b`'s one wanted row, the seek past the term lands on
                // `c`'s first row, which ends the scan.
                (vec![0], 1 + 2, 2 * 2 + 1),
                // The seek to a region `b` lacks lands on `c`'s first row
                // likewise.
                (vec![0, 200 << 16], 1 + 2, 2 * 2 + 1),
                // `b`'s last row is wanted: the row after it, `c`'s first,
                // ends the scan without a seek.
                (vec![149 << 16], 1 + 2, 2 * 3 + 1),
            ] {
                let regions = regions_of(wanted.iter().copied());
                let counted_rows = Cell::new(0);
                let counted = Counted {
                    tx: &read,
                    seeks: Cell::new(0),
                    rows: &counted_rows,
                };
                let confined: Vec<_> = index
                    .terms_within(&counted, range.clone(), &regions)
                    .unwrap()
                    .collect::<Result<_>>()
                    .unwrap();
                let found: Vec<_> = confined
                    .iter()
                    .map(|(term, bitmap)| (term.clone(), bitmap.treemap().iter().collect()))
                    .collect();
                // The wanted IDs the terms hold: every term holds each region's first.
                let held: Vec<u64> = wanted
                    .iter()
                    .copied()
                    .filter(|&id| id < 150 << 16)
                    .collect();
                assert_eq!(
                    found,
                    [(tag(0), held.clone()), (tag(1), held)],
                    "{range:?}, {wanted:?}"
                );
                assert_eq!(counted.seeks.get(), seeks, "{range:?}, {wanted:?}: seeks");
                assert_eq!(counted_rows.get(), rows, "{range:?}, {wanted:?}: rows");
            }
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

    /// A bound on the values of [`tag`], all of them and one past.
    fn bound() -> impl Strategy<Value = Bound<u8>> {
        prop_oneof![
            Just(Bound::Unbounded),
            (0..5u8).prop_map(Bound::Included),
            (0..5u8).prop_map(Bound::Excluded),
        ]
    }

    proptest! {
        #[test]
        fn confined_reads_are_full_reads_within_the_regions(
            postings in postings(),
            neighbours in postings(),
            wanted in prop::collection::vec(prop_oneof![0..8u64 << 16, any::<u64>()], 0..12),
            lower in bound(),
            upper in bound(),
            prefix in prop_oneof![Just(String::new()), (0..5u8).prop_map(|n| char::from(b'a' + n).to_string())],
        ) {
            let store = store_of_terms(
                postings
                    .iter()
                    .map(|(value, ids)| (tag(*value), ids.as_slice()))
                    .chain(neighbours.iter().map(|(value, ids)| (neighbour(*value), ids.as_slice()))),
            );
            let read = store.begin_read().unwrap();
            let index = Index::new(&Keccak256Hasher);
            let regions = regions_of(wanted);
            for value in 0..5 {
                prop_assert_eq!(
                    index.postings_within(&read, &tag(value), &regions).unwrap(),
                    index.postings(&read, &tag(value)).unwrap().within(&regions)
                );
            }
            let ranges = [
                Some(TermRange::all()),
                tag_range(lower, upper),
                Some(TermRange::with_prefix(CellNameRef::raw(b"tag"), &prefix).unwrap()),
                Some(TermRange::of_name(CellNameRef::raw(b"tag"), None).unwrap()),
            ];
            for range in ranges.into_iter().flatten() {
                let full: Vec<_> = index
                    .terms(&read, range.clone())
                    .unwrap()
                    .map(|entry| entry.map(|(term, bitmap)| (term, bitmap.within(&regions))))
                    .collect::<Result<_>>()
                    .unwrap();
                let confined: Vec<_> = index
                    .terms_within(&read, range.clone(), &regions)
                    .unwrap()
                    .collect::<Result<_>>()
                    .unwrap();
                prop_assert_eq!(confined, full, "{:?}", range);
            }
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
