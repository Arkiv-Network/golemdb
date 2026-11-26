use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

use golemdb_cells::{CellNameRef, CellType, CellValue};
use golemdb_history::{HistoryError, changeset_key};
use golemdb_index::{
    BitmapContainer, ContainerId, INDEX_TRIE_PATH_BYTES, Index, IndexError, IndexKey, IndexReader,
    IndexTerm, PostingChange, TermRange, history, tables,
};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_storage::{
    MemoryStore, ReadCursor, ReadTransaction, StorageError, Store, Table, WriteTransaction,
};
use golemdb_storage_mdbx::MdbxStore;
use proptest::prelude::*;

type State = BTreeMap<IndexTerm, BTreeSet<u64>>;
type Terms = Vec<(IndexTerm, Vec<u64>)>;

fn term(name: &str, value: CellValue) -> IndexTerm {
    IndexTerm::from_str_name(name, value.as_view()).unwrap()
}

fn string(value: &str) -> IndexTerm {
    term("color", CellValue::from_str(value))
}

fn key(term: &IndexTerm, hi: u64) -> IndexKey {
    IndexKey {
        term: term.clone(),
        container_id: ContainerId::new(hi).unwrap(),
    }
}

fn posting(term: &IndexTerm, id: u64, present: bool) -> PostingChange {
    if present {
        PostingChange::Add {
            term: term.clone(),
            record_id: id,
        }
    } else {
        PostingChange::Remove {
            term: term.clone(),
            record_id: id,
        }
    }
}

fn publish(
    store: &impl Store,
    root: &mut RootRef<INDEX_TRIE_PATH_BYTES>,
    state: &mut State,
    commit: u64,
    changes: Vec<PostingChange>,
) {
    // Independent logical model: requests update sets, without reading history
    // or using the production container grouping/resolution code.
    for change in &changes {
        match change {
            PostingChange::Add { term, record_id } => {
                state.entry(term.clone()).or_default().insert(*record_id);
            }
            PostingChange::Remove { term, record_id } => {
                if let Some(ids) = state.get_mut(term) {
                    ids.remove(record_id);
                    if ids.is_empty() {
                        state.remove(term);
                    }
                }
            }
        }
    }
    let mut tx = store.begin_write().unwrap();
    let update = Index::new(&Keccak256Hasher)
        .apply(&mut tx, commit, *root, changes)
        .unwrap();
    if commit != 0 {
        history::record_changes(&mut tx, commit, &update.changed_containers).unwrap();
    }
    *root = update.root;
    tx.commit().unwrap();
}

fn expected(state: &State, range: &TermRange) -> Terms {
    let (lower, upper) = range.bounds();
    state
        .iter()
        .filter(|(term, _)| {
            let bytes = term.as_bytes();
            let after = match lower {
                Bound::Unbounded => true,
                Bound::Included(low) => bytes >= low,
                Bound::Excluded(low) => bytes > low,
            };
            let before = match upper {
                Bound::Unbounded => true,
                Bound::Included(high) => bytes <= high,
                Bound::Excluded(high) => bytes < high,
            };
            after && before
        })
        .map(|(term, ids)| (term.clone(), ids.iter().copied().collect()))
        .collect()
}

fn scanned(reader: &IndexReader<'_, impl ReadTransaction>, range: TermRange) -> Terms {
    reader
        .terms(range)
        .unwrap()
        .map(|row| {
            let (term, bitmap) = row.unwrap();
            (term, bitmap.treemap().iter().collect())
        })
        .collect()
}

fn ranges() -> Vec<TermRange> {
    let name = CellNameRef::raw(b"color");
    let mut ranges = vec![TermRange::all()];
    for name in [
        name,
        CellNameRef::raw(b"colorful"),
        CellNameRef::raw(b"missing"),
    ] {
        for ty in [
            None,
            Some(CellType::Str),
            Some(CellValue::from_u64(0).as_view().cell_type()),
        ] {
            ranges.push(TermRange::of_name(name, ty).unwrap());
        }
    }
    for prefix in [
        "",
        "b",
        "blue",
        "blue\u{1}",
        "r",
        "é",
        "\u{10ffff}",
        "missing",
    ] {
        ranges.push(TermRange::with_prefix(name, prefix).unwrap());
    }
    let bounds = [
        Bound::Unbounded,
        Bound::Included(""),
        Bound::Excluded(""),
        Bound::Included("blue"),
        Bound::Excluded("blue"),
        Bound::Included("red"),
        Bound::Excluded("red"),
    ];
    for lower in bounds {
        for upper in bounds {
            let lower = lower.map(CellValue::from_str);
            let upper = upper.map(CellValue::from_str);
            if let Some(range) = TermRange::between(
                name,
                CellType::Str,
                lower.as_ref().map(CellValue::as_view),
                upper.as_ref().map(CellValue::as_view),
            )
            .unwrap()
            {
                ranges.push(range);
            }
        }
    }
    ranges
}

fn check(tx: &impl ReadTransaction, snapshots: &[State], check_ranges: bool) {
    let all_terms = snapshots
        .iter()
        .flat_map(|state| state.keys().cloned())
        .chain([string("missing")])
        .collect::<BTreeSet<_>>();
    for (target, state) in snapshots.iter().enumerate() {
        let reader = IndexReader::at(tx, target as u64);
        assert_eq!(
            scanned(&reader, TermRange::all()),
            expected(state, &TermRange::all()),
            "target {target}"
        );
        // Flattening the lower-level stream must neither duplicate memberships
        // nor omit deleted containers, independently of term assembly.
        let actual: Vec<_> = reader
            .containers_for_range(TermRange::all())
            .unwrap()
            .flat_map(|row| {
                let (key, container) = row.unwrap();
                container
                    .iter()
                    .map(|id| (key.term.clone(), id))
                    .collect::<Vec<_>>()
            })
            .collect();
        let reference: Vec<_> = state
            .iter()
            .flat_map(|(term, ids)| ids.iter().map(|id| (term.clone(), *id)))
            .collect();
        assert_eq!(actual, reference);
        for term in &all_terms {
            let expected = state
                .get(term)
                .map(|ids| ids.iter().copied().collect::<Vec<_>>());
            assert_eq!(
                reader
                    .bitmap(term)
                    .unwrap()
                    .map(|b| b.treemap().iter().collect::<Vec<_>>()),
                expected,
                "term {term:?}, target {target}"
            );
            assert_eq!(reader.term_exists(term).unwrap(), expected.is_some());
            assert_eq!(
                reader
                    .postings(term)
                    .unwrap()
                    .treemap()
                    .iter()
                    .collect::<Vec<_>>(),
                expected.unwrap_or_default()
            );
        }
        if check_ranges {
            for range in ranges() {
                assert_eq!(
                    scanned(&reader, range.clone()),
                    expected(state, &range),
                    "range {range:?}, target {target}"
                );
            }
            for value in [
                CellValue::from_str(""),
                CellValue::from_str("blue"),
                CellValue::from_str("blue\u{1}"),
                CellValue::from_str("7"),
                CellValue::from_u64(7),
            ] {
                let exact = term("color", value.clone());
                let range = TermRange::between(
                    CellNameRef::raw(b"color"),
                    value.as_view().cell_type(),
                    Bound::Included(value.as_view()),
                    Bound::Included(value.as_view()),
                )
                .unwrap()
                .unwrap();
                assert_eq!(
                    scanned(&reader, range),
                    reader
                        .bitmap(&exact)
                        .unwrap()
                        .map(|b| (exact, b.treemap().iter().collect()))
                        .into_iter()
                        .collect::<Vec<_>>()
                );
            }
        }
    }
    assert_eq!(
        scanned(&IndexReader::new(tx), TermRange::all()),
        expected(snapshots.last().unwrap(), &TermRange::all())
    );
}

fn timeline(store: &impl Store) -> Vec<State> {
    let mut root = RootRef::Empty;
    let mut state = State::new();
    let blue = string("blue");
    let seven = string("7");
    let number = term("color", CellValue::from_u64(7));
    publish(
        store,
        &mut root,
        &mut state,
        0,
        vec![
            posting(&blue, 1, true),
            posting(&blue, (1 << 16) + 2, true),
            posting(&blue, (2 << 16) + 3, true),
            posting(&string("blue\u{1}"), 1, true),
            posting(&string(""), 2, true),
            posting(&seven, 7, true),
            posting(&string("é"), 8, true),
            posting(&string("\u{10ffff}"), 9, true),
            posting(&term("stable", CellValue::from_bool(true)), 42, true),
            posting(&term("colorful", CellValue::from_str("blue")), 1, true),
        ],
    );
    let mut snapshots = vec![state.clone()];
    publish(
        store,
        &mut root,
        &mut state,
        1,
        vec![
            posting(&blue, 1, false),
            posting(&blue, (3 << 16) + 4, true),
            posting(&string("blue\u{1}"), 1, false),
            posting(&string("red"), 1, true),
        ],
    );
    snapshots.push(state.clone());
    let old_tx = store.begin_read().unwrap();
    let mut old_scan = IndexReader::at(&old_tx, 1).terms(TermRange::all()).unwrap();
    let first = old_scan.next().unwrap().unwrap();
    publish(
        store,
        &mut root,
        &mut state,
        2,
        vec![
            posting(&blue, (1 << 16) + 2, false),
            posting(&blue, (1 << 16) + 5, true),
            posting(&blue, (3 << 16) + 4, false),
            posting(&blue, u64::MAX, true),
            posting(&seven, 7, false),
            posting(&number, 7, true),
        ],
    );
    snapshots.push(state.clone());
    publish(
        store,
        &mut root,
        &mut state,
        3,
        vec![
            posting(&blue, (2 << 16) + 3, false),
            posting(&blue, 1, true),
            posting(&string("red"), 1, false),
            posting(&blue, 5 << 16, true),
            posting(&blue, 5 << 16, false),
        ],
    );
    snapshots.push(state.clone());
    publish(store, &mut root, &mut state, 4, vec![]);
    snapshots.push(state.clone());
    let old: Terms = std::iter::once(Ok(first))
        .chain(old_scan)
        .map(|row| {
            let (term, bitmap) = row.unwrap();
            (term, bitmap.treemap().iter().collect())
        })
        .collect();
    assert_eq!(old, expected(&snapshots[1], &TermRange::all()));
    check(&old_tx, &snapshots[..2], true);
    check(&store.begin_read().unwrap(), &snapshots, true);
    snapshots
}

#[test]
fn memory_scans_match_saved_states_across_publication() {
    timeline(&MemoryStore::new());
}

#[test]
fn mdbx_scans_match_saved_states_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let snapshots = timeline(&MdbxStore::open(dir.path()).unwrap());
    let store = MdbxStore::open(dir.path()).unwrap();
    check(&store.begin_read().unwrap(), &snapshots, true);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn random_histories_match_independent_posting_sets(ops in prop::collection::vec((0usize..5, 0usize..6, any::<bool>()), 1..40)) {
        let store = MemoryStore::new();
        let mut root = RootRef::Empty;
        let mut state = State::new();
        let mut snapshots = vec![state.clone()];
        let terms = [string(""), string("blue"), string("blue\u{1}"), string("red"), term("color", CellValue::from_u64(7))];
        let ids = [0, 65535, 65536, (1 << 32) + 1, u64::MAX - 1, u64::MAX];
        for (step, (term, id, present)) in ops.into_iter().enumerate() {
            publish(&store, &mut root, &mut state, step as u64 + 1, vec![posting(&terms[term], ids[id], present)]);
            snapshots.push(state.clone());
        }
        check(&store.begin_read().unwrap(), &snapshots, false);
    }
}

#[test]
fn ranges_reject_empty_intervals_and_nul_prefixes() {
    let name = CellNameRef::raw(b"color");
    let value = CellValue::from_str("blue");
    assert!(
        TermRange::between(
            name,
            CellType::Str,
            Bound::Included(value.as_view()),
            Bound::Excluded(value.as_view())
        )
        .unwrap()
        .is_none()
    );
    assert!(TermRange::with_prefix(name, "blue\0").is_err());
    assert!(IndexTerm::new("color", CellType::Str, b"blue\0").is_err());
}

#[derive(Debug, PartialEq, Eq)]
enum Access {
    Open(Table),
    Next(Table),
    Value(Table),
}

#[derive(Clone, Copy)]
enum Fail {
    Cursor(Table),
    Value(Table),
}

struct Observed<R> {
    inner: R,
    accesses: RefCell<Vec<Access>>,
    fail: Option<Fail>,
}

struct Cursor<'a, C> {
    inner: C,
    table: Table,
    accesses: &'a RefCell<Vec<Access>>,
    fail: bool,
}

impl<C: ReadCursor> ReadCursor for Cursor<'_, C> {
    fn next(&mut self) -> golemdb_storage::Result<Option<golemdb_storage::Entry>> {
        panic!("borrow cursor rows before copying payloads")
    }
    fn prev(&mut self) -> golemdb_storage::Result<Option<golemdb_storage::Entry>> {
        panic!("scan must not rewind")
    }
    fn with_next<T>(
        &mut self,
        inspect: impl FnOnce(Option<(&[u8], &[u8])>) -> T,
    ) -> golemdb_storage::Result<T> {
        self.accesses.borrow_mut().push(Access::Next(self.table));
        if self.fail {
            return Err(StorageError::Full);
        }
        self.inner.with_next(inspect)
    }
}

impl<R: ReadTransaction> ReadTransaction for Observed<R> {
    type Cursor<'a>
        = Cursor<'a, R::Cursor<'a>>
    where
        Self: 'a;
    fn get(&self, _: Table, _: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        panic!("borrow values before copying")
    }
    fn with_value<T>(
        &self,
        table: Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> golemdb_storage::Result<T> {
        assert_ne!(table, tables::INDEX_HISTORY, "reuse scanned history rows");
        assert!(
            table == tables::INDEX || table == tables::INDEX_CHANGE_SET,
            "no trie or primary-cell reads"
        );
        self.accesses.borrow_mut().push(Access::Value(table));
        if matches!(self.fail, Some(Fail::Value(t)) if t == table) {
            return Err(StorageError::Full);
        }
        self.inner.with_value(table, key, inspect)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        assert!(table == tables::INDEX || table == tables::INDEX_HISTORY);
        self.accesses.borrow_mut().push(Access::Open(table));
        Ok(Cursor {
            inner: self.inner.cursor(table, key)?,
            table,
            accesses: &self.accesses,
            fail: matches!(self.fail, Some(Fail::Cursor(t)) if t == table),
        })
    }
}

fn observed<R>(inner: R, fail: Option<Fail>) -> Observed<R> {
    Observed {
        inner,
        accesses: RefCell::new(vec![]),
        fail,
    }
}

#[test]
fn absent_candidates_can_be_stopped_before_more_work() {
    let store = MemoryStore::new();
    let term = string("blue");
    let mut tx = store.begin_write().unwrap();
    for hi in 0..1000 {
        let key = key(&term, hi);
        let bytes = BitmapContainer::from_values(key.container_id, [1])
            .encode()
            .unwrap();
        tx.put(tables::INDEX, &key.encode(), &bytes).unwrap();
        tables::HISTORY
            .record(&mut tx, &key.encode(), 1, None)
            .unwrap();
    }
    tx.commit().unwrap();
    let tx = observed(store.begin_read().unwrap(), None);
    let mut scan = IndexReader::at(&tx, 0).containers_for_term(&term).unwrap();
    assert_eq!(
        tx.accesses.take(),
        [
            Access::Open(tables::INDEX),
            Access::Open(tables::INDEX_HISTORY)
        ]
    );
    let mut attempts = 0;
    let result: Result<Option<()>, Box<dyn std::error::Error>> = scan.next_with(
        &mut attempts,
        |n, work| {
            if work != golemdb_history::ReadOperation::Advance(tables::INDEX) {
                return Ok(());
            }
            *n += 1;
            if *n <= 3 {
                Ok(())
            } else {
                Err("budget exhausted".into())
            }
        },
        |_, _, _| panic!("all candidates are absent"),
    );
    assert_eq!(result.unwrap_err().to_string(), "budget exhausted");
    assert_eq!(attempts, 4);
    let accesses = tx.accesses.take();
    for access in [
        Access::Next(tables::INDEX),
        Access::Next(tables::INDEX_HISTORY),
        Access::Value(tables::INDEX_CHANGE_SET),
    ] {
        assert_eq!(accesses.iter().filter(|a| **a == access).count(), 3);
    }
    assert!(!accesses.contains(&Access::Value(tables::INDEX)));
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
    assert!(tx.accesses.take().is_empty());
}

#[test]
fn hooks_can_refuse_before_cursor_work_or_selected_payload_decoding() {
    let store = MemoryStore::new();
    let term = string("blue");
    let k = key(&term, 0).encode();
    let mut tx = store.begin_write().unwrap();
    // Invalid bytes make any accidental eager decoding observable.
    tx.put(tables::INDEX, &k, b"malformed").unwrap();
    tables::HISTORY
        .record(&mut tx, &k, 1, Some(b"malformed"))
        .unwrap();
    tx.commit().unwrap();
    for target in [None, Some(0), Some(1)] {
        let tx = observed(store.begin_read().unwrap(), None);
        let reader = target.map_or_else(|| IndexReader::new(&tx), |t| IndexReader::at(&tx, t));
        let mut scan = reader.containers_for_term(&term).unwrap();
        tx.accesses.take();
        let result: Result<Option<()>, Box<dyn std::error::Error>> = scan.next_with(
            &mut (),
            |_, _| Err("refused".into()),
            |_, _, _| panic!("before work"),
        );
        assert_eq!(result.unwrap_err().to_string(), "refused");
        assert!(tx.accesses.take().is_empty());
        let mut scan = reader.containers_for_term(&term).unwrap();
        tx.accesses.take();
        let result: Result<Option<()>, Box<dyn std::error::Error>> = scan.next_with(
            &mut (),
            |_, _| Ok(()),
            |_, _, bytes| {
                assert_eq!(bytes, b"malformed");
                Err("decode refused".into())
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "decode refused");
        if target.is_none() {
            assert_eq!(tx.accesses.take(), [Access::Next(tables::INDEX)]);
        }
        tx.accesses.take();
        assert!(scan.next().is_none());
        assert!(tx.accesses.take().is_empty());
        let mut scan = reader.containers_for_term(&term).unwrap();
        let mut attempts = 0;
        for expected in [Some(9), None] {
            let actual: golemdb_index::Result<_> = scan.next_with(
                &mut attempts,
                |n, _| {
                    *n += 1;
                    Ok(())
                },
                |_, _, bytes| Ok(bytes.len()),
            );
            assert_eq!(actual.unwrap(), expected);
        }
        assert_eq!(attempts, if target.is_none() { 2 } else { 6 });
        tx.accesses.take();
        assert!(scan.next().is_none());
        assert!(tx.accesses.take().is_empty());
    }
}

#[test]
fn scans_use_one_pass_and_live_reads_need_no_history_or_point_lookups() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    for term in [string("blue"), string("red")] {
        for hi in 0..4 {
            let k = key(&term, hi);
            let bytes = BitmapContainer::from_values(k.container_id, [1])
                .encode()
                .unwrap();
            tx.put(tables::INDEX, &k.encode(), &bytes).unwrap();
            tables::HISTORY
                .record(&mut tx, &k.encode(), 1, Some(&bytes))
                .unwrap();
        }
    }
    tx.commit().unwrap();
    for target in [None, Some(0), Some(1)] {
        let tx = observed(store.begin_read().unwrap(), None);
        let reader = target.map_or_else(|| IndexReader::new(&tx), |t| IndexReader::at(&tx, t));
        let scan = reader.terms(TermRange::all()).unwrap();
        let opens = tx.accesses.take();
        assert_eq!(opens.len(), if target.is_some() { 2 } else { 1 });
        let terms = scan.collect::<golemdb_index::Result<Vec<_>>>().unwrap();
        assert_eq!(terms.len(), 2);
        assert!(terms.iter().all(|(_, bitmap)| bitmap.treemap().len() == 4));
        let accesses = tx.accesses.take();
        assert_eq!(
            accesses
                .iter()
                .filter(|a| **a == Access::Next(tables::INDEX))
                .count(),
            9
        );
        if target.is_some() {
            assert_eq!(
                accesses
                    .iter()
                    .filter(|a| **a == Access::Next(tables::INDEX_HISTORY))
                    .count(),
                9
            );
            let table = if target == Some(0) {
                tables::INDEX_CHANGE_SET
            } else {
                tables::INDEX
            };
            assert_eq!(
                accesses
                    .iter()
                    .filter(|a| **a == Access::Value(table))
                    .count(),
                8
            );
            assert_eq!(accesses.len(), 26);
        } else {
            assert_eq!(accesses.len(), 9);
        }
    }
}

#[test]
fn returning_one_term_does_not_resolve_all_absent_later_terms() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    for n in 0..1000 {
        let k = key(&string(&format!("{n:04}")), 0);
        let bytes = BitmapContainer::from_values(k.container_id, [1])
            .encode()
            .unwrap();
        tx.put(tables::INDEX, &k.encode(), &bytes).unwrap();
        tables::HISTORY
            .record(
                &mut tx,
                &k.encode(),
                1,
                (n == 0).then_some(bytes.as_slice()),
            )
            .unwrap();
    }
    tx.commit().unwrap();
    let tx = observed(store.begin_read().unwrap(), None);
    let mut scan = IndexReader::at(&tx, 0).terms(TermRange::all()).unwrap();
    tx.accesses.take();
    let (term, bitmap) = scan.next().unwrap().unwrap();
    assert_eq!(term, string("0000"));
    assert_eq!(bitmap.treemap().iter().collect::<Vec<_>>(), [1]);
    let accesses = tx.accesses.take();
    // Only the current candidate and one candidate from the next term.
    for access in [
        Access::Next(tables::INDEX),
        Access::Next(tables::INDEX_HISTORY),
        Access::Value(tables::INDEX_CHANGE_SET),
    ] {
        assert_eq!(accesses.iter().filter(|a| **a == access).count(), 2);
    }
    assert_eq!(accesses.len(), 6);
}

#[test]
fn storage_errors_are_terminal_for_container_and_term_scans() {
    let store = MemoryStore::new();
    let term = string("blue");
    let k = key(&term, 0);
    let bytes = BitmapContainer::from_values(k.container_id, [1])
        .encode()
        .unwrap();
    let mut tx = store.begin_write().unwrap();
    tx.put(tables::INDEX, &k.encode(), &bytes).unwrap();
    tables::HISTORY
        .record(&mut tx, &k.encode(), 1, Some(&bytes))
        .unwrap();
    tx.commit().unwrap();
    for (target, fail) in [
        (0, Fail::Cursor(tables::INDEX)),
        (0, Fail::Cursor(tables::INDEX_HISTORY)),
        (0, Fail::Value(tables::INDEX_CHANGE_SET)),
        (1, Fail::Value(tables::INDEX)),
    ] {
        let tx = observed(store.begin_read().unwrap(), Some(fail));
        let reader = IndexReader::at(&tx, target);
        let mut containers = reader.containers_for_term(&term).unwrap();
        assert!(containers.next().unwrap().is_err());
        tx.accesses.take();
        assert!(containers.next().is_none());
        assert!(tx.accesses.take().is_empty());
        let mut terms = reader.terms(TermRange::all()).unwrap();
        assert!(terms.next().unwrap().is_err());
        tx.accesses.take();
        assert!(terms.next().is_none());
        assert!(terms.next().is_none());
        assert!(tx.accesses.take().is_empty());
    }
}

#[test]
fn malformed_later_containers_never_emit_partial_terms() {
    for defect in 0..6 {
        let store = MemoryStore::new();
        let term = string("blue");
        let first = key(&term, 0);
        let second = key(&term, 1);
        let bytes = BitmapContainer::from_values(first.container_id, [1])
            .encode()
            .unwrap();
        let mut tx = store.begin_write().unwrap();
        tx.put(tables::INDEX, &first.encode(), &bytes).unwrap();
        let mut encoded = second.encode();
        match defect {
            0 => {
                tables::HISTORY
                    .record(&mut tx, &encoded, 1, Some(&bytes))
                    .unwrap();
                tx.delete(tables::INDEX_CHANGE_SET, &changeset_key(1, &encoded))
                    .unwrap();
            }
            1 => {
                tx.put(tables::INDEX_HISTORY, &encoded, b"bad history")
                    .unwrap();
            }
            2 => {
                tables::HISTORY
                    .record(&mut tx, &encoded, 1, Some(b"bad container"))
                    .unwrap();
            }
            3 => {
                tx.put(tables::INDEX, &encoded, b"").unwrap();
            }
            4 => {
                // Keep the truncated suffix after the valid first key in byte order.
                encoded = key(&term, 256).encode();
                encoded.pop();
                tx.put(tables::INDEX_HISTORY, &encoded, b"bad key").unwrap();
            }
            5 => {
                encoded.push(0);
                tx.put(tables::INDEX, &encoded, &bytes).unwrap();
            }
            _ => unreachable!(),
        }
        tx.commit().unwrap();
        let tx = store.begin_read().unwrap();
        let reader = IndexReader::at(&tx, 0);
        let mut containers = reader.containers_for_term(&term).unwrap();
        assert_eq!(containers.next().unwrap().unwrap().0, first);
        let error = containers.next().unwrap().unwrap_err();
        if defect == 0 {
            assert!(matches!(
                error,
                IndexError::History(HistoryError::MissingPreimage { .. })
            ));
        }
        if defect == 1 {
            assert!(matches!(
                error,
                IndexError::History(HistoryError::InvalidCommits { .. })
            ));
        }
        assert!(containers.next().is_none());
        let mut terms = reader.terms(TermRange::all()).unwrap();
        assert!(
            terms.next().unwrap().is_err(),
            "defect {defect}: must not emit first container alone"
        );
        assert!(terms.next().is_none());
        assert!(reader.bitmap(&term).is_err());
    }
}

#[test]
fn preimages_override_corrupt_live_payloads_and_absence_is_not_term_presence() {
    let store = MemoryStore::new();
    let term = string("blue");
    let k = key(&term, 0);
    let bytes = BitmapContainer::from_values(k.container_id, [1])
        .encode()
        .unwrap();
    let mut tx = store.begin_write().unwrap();
    tx.put(tables::INDEX, &k.encode(), b"corrupt live").unwrap();
    tables::HISTORY
        .record(&mut tx, &k.encode(), 1, Some(&bytes))
        .unwrap();
    let future = string("red");
    let future_key = key(&future, 0).encode();
    tx.put(tables::INDEX, &future_key, b"corrupt live").unwrap();
    tables::HISTORY
        .record(&mut tx, &future_key, 1, None)
        .unwrap();
    // A history-only row with no successor must fall back to current absence.
    tables::HISTORY
        .record(&mut tx, &key(&string("old"), 0).encode(), 0, Some(&bytes))
        .unwrap();
    tx.commit().unwrap();
    let tx = observed(store.begin_read().unwrap(), None);
    let reader = IndexReader::at(&tx, 0);
    assert_eq!(
        scanned(&reader, TermRange::all()),
        vec![(term.clone(), vec![1])]
    );
    assert!(!reader.term_exists(&future).unwrap());
    assert!(reader.bitmap(&future).unwrap().is_none());
    assert!(reader.postings(&future).unwrap().treemap().is_empty());
    assert!(IndexReader::new(&tx).bitmap(&term).is_err());
}
