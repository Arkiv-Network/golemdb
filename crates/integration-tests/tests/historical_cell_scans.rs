use std::{cell::RefCell, collections::BTreeMap};

use golemdb_cells::{
    CellChange, CellError, CellKey, CellKind, CellNameRef, CellReader, CellValue, Cells, history,
    tables,
};
use golemdb_history::{HistoryError, changeset_key};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_storage::{
    MemoryStore, ReadCursor, ReadTransaction, StorageError, Store, Table, WriteTransaction,
};
use golemdb_storage_mdbx::MdbxStore;
use proptest::prelude::*;

type State = BTreeMap<CellKey, CellValue>;

fn key(id: u64, name: &[u8]) -> CellKey {
    CellKey::new(id, CellNameRef::raw(name))
}

fn put(id: u64, name: &[u8], value: CellValue) -> CellChange {
    CellChange::Put {
        key: key(id, name),
        value,
    }
}

fn delete(id: u64, name: &[u8]) -> CellChange {
    CellChange::Delete { key: key(id, name) }
}

fn publish(
    store: &impl Store,
    root: &mut RootRef<{ golemdb_cells::CELL_TRIE_PATH_BYTES }>,
    state: &mut State,
    commit: u64,
    changes: Vec<CellChange>,
) {
    // The reference is a plain map updated from requests, independent of history.
    for change in &changes {
        match change {
            CellChange::Put { key, value } => {
                state.insert(key.clone(), value.clone());
            }
            CellChange::Delete { key } => {
                state.remove(key);
            }
        }
    }
    let mut tx = store.begin_write().unwrap();
    let update = Cells::new(&Keccak256Hasher)
        .apply(&mut tx, commit, *root, changes)
        .unwrap();
    if commit != 0 {
        history::record_changes(&mut tx, commit, &update.changed_cells).unwrap();
    }
    *root = update.root;
    tx.commit().unwrap();
}

fn expected(state: &State, id: u64, users: bool) -> Vec<(CellKey, CellValue)> {
    state
        .iter()
        .filter(|(key, _)| key.record_id() == id && (!users || key.name().as_bytes() >= b"$"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn check(tx: &impl ReadTransaction, snapshots: &[State]) {
    for (target, state) in snapshots.iter().enumerate() {
        let reader = CellReader::at(tx, target as u64);
        for id in [0, 1, 3, 4, 63, 64, 65, 66, u64::MAX - 1, u64::MAX] {
            assert_eq!(
                reader
                    .scan_record(id)
                    .unwrap()
                    .collect::<golemdb_cells::Result<Vec<_>>>()
                    .unwrap(),
                expected(state, id, false),
                "target {target}, record {id}"
            );
        }
        assert_eq!(
            reader
                .scan_user_cells(64)
                .unwrap()
                .collect::<golemdb_cells::Result<Vec<_>>>()
                .unwrap(),
            expected(state, 64, true)
        );
    }
    let latest = snapshots.last().unwrap();
    assert_eq!(
        CellReader::new(tx)
            .scan_record(64)
            .unwrap()
            .collect::<golemdb_cells::Result<Vec<_>>>()
            .unwrap(),
        expected(latest, 64, false)
    );
    assert_eq!(
        CellReader::new(tx)
            .scan_user_cells(64)
            .unwrap()
            .collect::<golemdb_cells::Result<Vec<_>>>()
            .unwrap(),
        expected(latest, 64, true)
    );
}

fn timeline(store: &impl Store) -> Vec<State> {
    let mut root = RootRef::Empty;
    let mut state = State::new();
    publish(
        store,
        &mut root,
        &mut state,
        0,
        vec![
            put(0, b"#parameter", CellValue::from_u64(17)),
            put(64, b"baseline", CellValue::from_str("unchanged")),
            put(64, b"changed", CellValue::from_str("genesis")),
            put(65, b"#key", CellValue::from_bytes(&[1; 32])),
        ],
    );
    let mut snapshots = vec![state.clone()];
    publish(
        store,
        &mut root,
        &mut state,
        1,
        vec![
            put(64, b"#key", CellValue::from_bytes(&[2; 32])),
            put(64, b"#meta", CellValue::from_bytes(&[0; 32])),
            put(64, b"$first", CellValue::from_bool(true)),
            put(64, b"a", CellValue::from_u64(1)),
            put(
                64,
                b"ab",
                CellValue::from_str("old")
                    .with_kind(CellKind::Attribute)
                    .unwrap(),
            ),
            put(64, b"changed", CellValue::from_str("first")),
            put(64, b"removed", CellValue::from_str("past")),
            put(3, b"", CellValue::from_u64(0)),
            put(3, b"\0\xff", CellValue::from_u64(1)),
            put(3, b"\xff", CellValue::from_u64(2)),
            put(63, b"neighbor", CellValue::from_u64(3)),
            put(u64::MAX, b"\xff", CellValue::from_u64(4)),
            put(u64::MAX, b"", CellValue::from_u64(5)),
        ],
    );
    snapshots.push(state.clone());
    let old = store.begin_read().unwrap();
    let mut old_scan = CellReader::at(&old, 1).scan_record(64).unwrap();
    let first = old_scan.next().unwrap().unwrap();
    publish(
        store,
        &mut root,
        &mut state,
        2,
        vec![
            delete(64, b"a"),
            put(64, b"ab", CellValue::from_u64(8)),
            delete(64, b"removed"),
            put(64, b"new", CellValue::from_str("future")),
            put(64, b"changed", CellValue::from_str("second")),
            delete(3, b"\0\xff"),
            delete(u64::MAX, b"\xff"),
        ],
    );
    snapshots.push(state.clone());
    publish(
        store,
        &mut root,
        &mut state,
        3,
        vec![
            put(64, b"a", CellValue::from_str("recreated")),
            put(64, b"temporary", CellValue::from_u64(4)),
            delete(64, b"temporary"),
            put(64, b"changed", CellValue::from_str("genesis")),
            put(3, b"\0\xff", CellValue::from_bool(true)),
            delete(u64::MAX, b""),
        ],
    );
    snapshots.push(state.clone());
    publish(store, &mut root, &mut state, 4, vec![]);
    snapshots.push(state);
    assert_eq!(
        std::iter::once(first)
            .chain(old_scan.map(Result::unwrap))
            .collect::<Vec<_>>(),
        expected(&snapshots[1], 64, false)
    );
    check(&store.begin_read().unwrap(), &snapshots);
    snapshots
}

#[test]
fn memory_enumerates_saved_states_and_keeps_existing_snapshots() {
    timeline(&MemoryStore::new());
}

#[test]
fn mdbx_enumerates_saved_states_across_publication_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let snapshots = timeline(&MdbxStore::open(dir.path()).unwrap());
    let store = MdbxStore::open(dir.path()).unwrap();
    check(&store.begin_read().unwrap(), &snapshots);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn random_scans_match_independent_maps(
        ops in prop::collection::vec((0usize..4, 0usize..5, prop::option::of(any::<u16>())), 1..45)
    ) {
        let store = MemoryStore::new();
        let mut state = State::new();
        let mut root = RootRef::Empty;
        let mut snapshots = vec![state.clone()];
        let ids = [0, 64, 65, u64::MAX];
        let names = [b"a".as_slice(), b"ab", b"#key", b"$user", b"\0\xff"];
        for (step, (id, name, value)) in ops.into_iter().enumerate() {
            let change = match value {
                Some(v) if v % 2 == 0 => put(ids[id], names[name], CellValue::from_u64(u64::from(v)).with_kind(CellKind::Attribute).unwrap()),
                Some(v) => put(ids[id], names[name], CellValue::from_str(&v.to_string())),
                None => delete(ids[id], names[name]),
            };
            publish(&store, &mut root, &mut state, step as u64 + 1, vec![change]);
            snapshots.push(state.clone());
        }
        check(&store.begin_read().unwrap(), &snapshots);
    }
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
        panic!("scan must borrow rows before deciding whether to copy values")
    }
    fn prev(&mut self) -> golemdb_storage::Result<Option<golemdb_storage::Entry>> {
        panic!("forward scan must not rewind")
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
        panic!("scan must borrow values before copying")
    }
    fn with_value<T>(
        &self,
        table: Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> golemdb_storage::Result<T> {
        self.accesses.borrow_mut().push(Access::Value(table));
        assert_ne!(table, tables::CELL_HISTORY, "reuse the scanned commit set");
        if matches!(self.fail, Some(Fail::Value(t)) if t == table) {
            return Err(StorageError::Full);
        }
        self.inner.with_value(table, key, inspect)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
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
fn absent_candidates_are_lazy_budgetable_and_do_not_copy_live_payloads() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    for n in 0..1000 {
        let key = key(64, format!("cell{n:04}").as_bytes()).encode();
        tx.put(
            tables::CELL,
            &key,
            CellValue::from_str("created later").encoded_bytes(),
        )
        .unwrap();
        tables::HISTORY.record(&mut tx, &key, 1, b"").unwrap();
    }
    tx.commit().unwrap();
    let tx = observed(store.begin_read().unwrap(), None);
    let mut scan = CellReader::at(&tx, 0).scan_record(64).unwrap();
    assert_eq!(
        tx.accesses.take(),
        [
            Access::Open(tables::CELL),
            Access::Open(tables::CELL_HISTORY)
        ]
    );
    let mut attempts = 0;
    let result: Result<Option<()>, Box<dyn std::error::Error>> = scan.next_with(
        &mut attempts,
        |n| {
            *n += 1;
            if *n <= 3 {
                Ok(())
            } else {
                Err("budget exhausted".into())
            }
        },
        |_, _, _| panic!("all candidates are absent at target"),
    );
    assert_eq!(result.unwrap_err().to_string(), "budget exhausted");
    assert_eq!(attempts, 4);
    let accesses = tx.accesses.take();
    assert_eq!(
        accesses
            .iter()
            .filter(|a| **a == Access::Next(tables::CELL))
            .count(),
        3
    );
    assert_eq!(
        accesses
            .iter()
            .filter(|a| **a == Access::Next(tables::CELL_HISTORY))
            .count(),
        3
    );
    assert_eq!(
        accesses
            .iter()
            .filter(|a| **a == Access::Value(tables::CELL_CHANGE_SET))
            .count(),
        3
    );
    assert!(!accesses.contains(&Access::Value(tables::CELL)));
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
    assert!(tx.accesses.take().is_empty());
}

#[test]
fn hooks_can_refuse_before_any_row_or_before_copying_a_selected_payload() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    let k = key(64, b"large").encode();
    let value = CellValue::from_bytes(&[9; 100_000]);
    tx.put(tables::CELL, &k, value.encoded_bytes()).unwrap();
    tables::HISTORY
        .record(&mut tx, &k, 1, value.encoded_bytes())
        .unwrap();
    tx.commit().unwrap();
    for target in [None, Some(0), Some(1)] {
        let tx = observed(store.begin_read().unwrap(), None);
        let reader = target.map_or_else(|| CellReader::new(&tx), |t| CellReader::at(&tx, t));
        let mut scan = reader.scan_record(64).unwrap();
        tx.accesses.take();
        let result: Result<Option<()>, Box<dyn std::error::Error>> = scan.next_with(
            &mut (),
            |_| Err("refused".into()),
            |_, _, _| panic!("refused before work"),
        );
        assert!(result.is_err());
        assert!(tx.accesses.take().is_empty());
        let mut scan = reader.scan_record(64).unwrap();
        tx.accesses.take();
        let result: Result<Option<()>, Box<dyn std::error::Error>> = scan.next_with(
            &mut (),
            |_| Ok(()),
            |_, key, bytes| {
                assert_eq!(key.name().as_bytes(), b"large");
                assert_eq!(bytes.len(), 100_001);
                Err("copy refused".into())
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "copy refused");
        let accesses = tx.accesses.take();
        if target.is_none() {
            assert_eq!(accesses, [Access::Next(tables::CELL)]);
        }
        if target == Some(0) {
            assert!(!accesses.contains(&Access::Value(tables::CELL)));
        }
        assert!(scan.next().is_none());
        assert!(tx.accesses.take().is_empty());

        let mut scan = reader.scan_record(64).unwrap();
        let mut attempts = 0;
        for expected in [Some(100_001), None] {
            let result: golemdb_cells::Result<_> = scan.next_with(
                &mut attempts,
                |n| {
                    *n += 1;
                    Ok(())
                },
                |_, _, bytes| Ok(bytes.len()),
            );
            assert_eq!(result.unwrap(), expected);
        }
        assert_eq!(attempts, 2); // The exhaustion probe is also an attempt.
        tx.accesses.take();
        let result: golemdb_cells::Result<Option<()>> = scan.next_with(
            &mut (),
            |_| panic!("fused scan must not call hooks"),
            |_, _, _| unreachable!(),
        );
        assert_eq!(result.unwrap(), None);
        assert!(scan.next().is_none());
        assert!(tx.accesses.take().is_empty());
    }
}

#[test]
fn user_scans_skip_identity_and_metadata_in_both_tables() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    for name in [b"#key".as_slice(), b"#meta"] {
        let k = key(64, name).encode();
        tx.put(tables::CELL, &k, b"invalid").unwrap();
        tx.put(tables::CELL_HISTORY, &k, b"invalid").unwrap();
    }
    let user = key(64, b"$user");
    let value = CellValue::from_u64(7);
    tx.put(tables::CELL, &user.encode(), value.encoded_bytes())
        .unwrap();
    tx.commit().unwrap();
    let tx = store.begin_read().unwrap();
    for reader in [CellReader::new(&tx), CellReader::at(&tx, 0)] {
        assert_eq!(
            reader
                .scan_user_cells(64)
                .unwrap()
                .collect::<golemdb_cells::Result<Vec<_>>>()
                .unwrap(),
            [(user.clone(), value.clone())]
        );
        assert!(reader.scan_record(64).unwrap().next().unwrap().is_err());
    }
}

#[test]
fn storage_errors_from_either_cursor_or_selected_value_are_terminal() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    let k = key(64, b"a").encode();
    tx.put(tables::CELL, &k, CellValue::from_u64(2).encoded_bytes())
        .unwrap();
    tables::HISTORY
        .record(&mut tx, &k, 1, CellValue::from_u64(1).encoded_bytes())
        .unwrap();
    tx.commit().unwrap();
    for (target, fail) in [
        (0, Fail::Cursor(tables::CELL)),
        (0, Fail::Cursor(tables::CELL_HISTORY)),
        (0, Fail::Value(tables::CELL_CHANGE_SET)),
        (1, Fail::Value(tables::CELL)),
    ] {
        let tx = observed(store.begin_read().unwrap(), Some(fail));
        let mut scan = CellReader::at(&tx, target).scan_record(64).unwrap();
        assert!(matches!(
            scan.next(),
            Some(Err(CellError::Storage(StorageError::Full)))
                | Some(Err(CellError::History(HistoryError::Storage(
                    StorageError::Full
                ))))
        ));
        tx.accesses.take();
        assert!(scan.next().is_none());
        assert!(scan.next().is_none());
        assert!(tx.accesses.take().is_empty());
    }
}

#[test]
fn selected_corruption_fails_lazily_and_shadowed_live_bytes_are_not_decoded() {
    let store = MemoryStore::new();
    let k = key(64, b"b").encode();
    let cs = changeset_key(1, &k);
    let mut tx = store.begin_write().unwrap();
    tx.put(
        tables::CELL,
        &key(64, b"a").encode(),
        CellValue::from_u64(1).encoded_bytes(),
    )
    .unwrap();
    tx.put(tables::CELL, &k, b"broken live").unwrap();
    tables::HISTORY
        .record(&mut tx, &k, 1, CellValue::from_u64(2).encoded_bytes())
        .unwrap();
    tx.put(
        tables::CELL,
        &key(64, b"c").encode(),
        CellValue::from_u64(3).encoded_bytes(),
    )
    .unwrap();
    tx.commit().unwrap();
    let tx = store.begin_read().unwrap();
    let actual = CellReader::at(&tx, 0)
        .scan_record(64)
        .unwrap()
        .collect::<golemdb_cells::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(actual.len(), 3);
    assert_eq!(actual[1].1, CellValue::from_u64(2));
    drop(tx);
    for defect in 0..4 {
        let mut tx = store.begin_write().unwrap();
        match defect {
            0 => {
                tx.delete(tables::CELL_CHANGE_SET, &cs).unwrap();
            }
            1 => tx.put(tables::CELL_CHANGE_SET, &cs, &[0]).unwrap(),
            2 => tx.put(tables::CELL_HISTORY, &k, &[0]).unwrap(),
            3 => tx.put(tables::CELL, &k, b"").unwrap(),
            _ => unreachable!(),
        }
        let mut scan = CellReader::at(&tx, if defect == 3 { 1 } else { 0 })
            .scan_record(64)
            .unwrap();
        assert_eq!(scan.next().unwrap().unwrap().0, key(64, b"a"));
        match (defect, scan.next()) {
            (0, Some(Err(CellError::History(HistoryError::MissingPreimage { .. }))))
            | (1 | 3, Some(Err(CellError::Value(_))))
            | (2, Some(Err(CellError::History(HistoryError::InvalidCommits { .. })))) => (),
            (_, other) => panic!("wrong error for defect {defect}: {other:?}"),
        }
        assert!(scan.next().is_none());
        drop(scan);
        tx.abort();
    }
}
