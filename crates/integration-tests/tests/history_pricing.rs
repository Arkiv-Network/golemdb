//! Pricing on the original per-item history layout, including upfront failures.
use std::{cell::RefCell, collections::BTreeMap};

use golemdb_cells::{CellError, CellKey, CellNameRef, CellReader, CellValue, tables};
use golemdb_history::{CommitSet, ReadOperation, changeset_key};
use golemdb_index::{
    BitmapContainer, ContainerId, IndexError, IndexKey, IndexReader, IndexTerm, TermRange,
    tables as index,
};
use golemdb_metering::{
    Accounting, Budget, Ledger, ReadMeter, Schedule, WRITE_CODE_VERSION, WeightName as W,
};
use golemdb_record::{
    RecordError, RecordMeta,
    historical::{self, LookupRole, RecordPrepayment},
};
use golemdb_storage::{
    MemoryStore, ReadCursor, ReadTransaction, StorageError, Store, Table, WriteTransaction,
};
use golemdb_storage_mdbx::MdbxStore;

fn key(name: &[u8]) -> CellKey {
    CellKey::new(64, CellNameRef::raw(name))
}
fn schedule() -> Schedule {
    Schedule::new(
        WRITE_CODE_VERSION,
        [
            (W::READ_CELL, 1),
            (W::READ_KEY, 2),
            (W::READ_CELL_SCAN, 3),
            (W::READ_COMMIT_SET, 5),
            (W::READ_BYTE, 1),
        ],
    )
    .unwrap()
}
fn counts(values: impl IntoIterator<Item = (W, u64)>) -> BTreeMap<W, u64> {
    values.into_iter().filter(|(_, n)| *n != 0).collect()
}
fn ledger(value: &Ledger) -> BTreeMap<W, u64> {
    value.iter().map(|(w, n)| (w.clone(), n)).collect()
}
fn point<R: ReadTransaction>(
    tx: &R,
    name: &[u8],
    target: u64,
    budget: Budget,
) -> (golemdb_record::Result<Option<CellValue>>, Accounting) {
    let schedule = schedule();
    let mut meter = ReadMeter::new(&schedule, budget);
    let k = key(name);
    let result = historical::with_value(
        &CellReader::at(tx, target),
        &k,
        LookupRole::Cell,
        &mut meter,
        |meter, raw| {
            raw.map(|raw| historical::returned(&k.name().into(), raw, meter))
                .transpose()
        },
    );
    (result, meter.into_accounting())
}
fn scan<R: ReadTransaction>(
    tx: &R,
    target: u64,
    budget: Budget,
) -> (
    golemdb_record::Result<Vec<(CellKey, CellValue)>>,
    Accounting,
) {
    let schedule = schedule();
    let mut meter = ReadMeter::new(&schedule, budget);
    let mut scan = CellReader::at(tx, target).scan_record(64).unwrap();
    let mut rows = vec![];
    let result = loop {
        match historical::next_cell(&mut scan, &mut meter, None) {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => break Ok(rows),
            Err(e) => {
                assert!(scan.next().is_none());
                break Err(e);
            }
        }
    };
    (result, meter.into_accounting())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Advance(Table),
    Lookup(Table),
}
struct Observed<R> {
    inner: R,
    accesses: RefCell<Vec<Access>>,
    fail: Option<Access>,
}
struct Cursor<'a, C> {
    inner: C,
    table: Table,
    accesses: &'a RefCell<Vec<Access>>,
    fail: bool,
}
impl<C: ReadCursor> ReadCursor for Cursor<'_, C> {
    fn next(&mut self) -> golemdb_storage::Result<Option<golemdb_storage::Entry>> {
        panic!("borrow before copying")
    }
    fn prev(&mut self) -> golemdb_storage::Result<Option<golemdb_storage::Entry>> {
        panic!("no rewind")
    }
    fn with_next<T>(
        &mut self,
        inspect: impl FnOnce(Option<(&[u8], &[u8])>) -> T,
    ) -> golemdb_storage::Result<T> {
        self.accesses.borrow_mut().push(Access::Advance(self.table));
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
        panic!("borrow before copying")
    }
    fn with_value<T>(
        &self,
        table: Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> golemdb_storage::Result<T> {
        self.accesses.borrow_mut().push(Access::Lookup(table));
        if self.fail == Some(Access::Lookup(table)) {
            return Err(StorageError::Full);
        }
        self.inner.with_value(table, key, inspect)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        Ok(Cursor {
            inner: self.inner.cursor(table, key)?,
            table,
            accesses: &self.accesses,
            fail: self.fail == Some(Access::Advance(table)),
        })
    }
}
fn observed<R>(inner: R) -> Observed<R> {
    Observed {
        inner,
        accesses: RefCell::new(vec![]),
        fail: None,
    }
}

fn point_contract(store: &impl Store) {
    let value = CellValue::from_u64(7);
    let k = key(b"a").encode();
    let mut tx = store.begin_write().unwrap();
    tx.put(tables::CELL, &k, value.encoded_bytes()).unwrap();
    let (_, missing) = point(&tx, b"a", 0, Budget::Unlimited);
    assert_eq!(
        ledger(&missing.ledger),
        counts([(W::READ_CELL, 2), (W::READ_BYTE, 18)])
    );
    tables::HISTORY
        .record(&mut tx, &k, 10, value.encoded_bytes())
        .unwrap();
    tx.commit().unwrap();
    // The singleton bitmap is 30 bytes. Lookup1 + (flat5 + bytes30) + lookup1
    // + returned bytes18 = 55. Refusal retains only fully admitted packages.
    for budget in 0..=55 {
        let tx = observed(store.begin_read().unwrap());
        let (result, accounting) = point(&tx, b"a", 9, Budget::Limited(budget));
        let (cost, accesses) = match budget {
            0 => (0, vec![]),
            1..=35 => (1, vec![Access::Lookup(tables::CELL_HISTORY)]),
            36 => (36, vec![Access::Lookup(tables::CELL_HISTORY)]),
            37..=55 => (
                if budget == 55 { 55 } else { 37 },
                vec![
                    Access::Lookup(tables::CELL_HISTORY),
                    Access::Lookup(tables::CELL_CHANGE_SET),
                ],
            ),
            _ => unreachable!(),
        };
        assert_eq!(accounting.cost, cost, "budget {budget}");
        assert_eq!(*tx.accesses.borrow(), accesses, "budget {budget}");
        if budget == 55 {
            assert_eq!(result.unwrap(), Some(value.clone()));
        } else {
            assert!(matches!(result, Err(RecordError::Metering(_))));
        }
    }
    for target in [0, 9, 10, u64::MAX] {
        let tx = observed(store.begin_read().unwrap());
        let (result, accounting) = point(&tx, b"a", target, Budget::Unlimited);
        assert_eq!(result.unwrap(), Some(value.clone()));
        assert_eq!(
            ledger(&accounting.ledger),
            counts([
                (W::READ_CELL, 2),
                (W::READ_COMMIT_SET, 1),
                (W::READ_BYTE, 48)
            ])
        );
        let selected = if target < 10 {
            tables::CELL_CHANGE_SET
        } else {
            tables::CELL
        };
        assert_eq!(
            *tx.accesses.borrow(),
            [
                Access::Lookup(tables::CELL_HISTORY),
                Access::Lookup(selected)
            ]
        );
    }
    // The binding's selected lookup pays read.key, with no ordinary cell or
    // returned-byte charge for internal binding materialization.
    let tx = store.begin_read().unwrap();
    let schedule = schedule();
    let mut meter = ReadMeter::new(&schedule, Budget::Unlimited);
    historical::with_value(
        &CellReader::at(&tx, 9),
        &key(b"a"),
        LookupRole::Binding,
        &mut meter,
        |_, raw| Ok(raw.unwrap().len()),
    )
    .unwrap();
    assert_eq!(
        ledger(&meter.into_accounting().ledger),
        counts([
            (W::READ_CELL, 1),
            (W::READ_KEY, 1),
            (W::READ_COMMIT_SET, 1),
            (W::READ_BYTE, 30)
        ])
    );
    drop(tx);
    let mut tx = store.begin_write().unwrap();
    tx.put(tables::CELL_CHANGE_SET, &changeset_key(10, &k), &[])
        .unwrap();
    let (result, accounting) = point(&tx, b"a", 9, Budget::Unlimited);
    assert!(result.unwrap().is_none());
    assert_eq!(accounting.cost, 37);
    tx.delete(tables::CELL_CHANGE_SET, &changeset_key(10, &k))
        .unwrap();
    let (result, accounting) = point(&tx, b"a", 9, Budget::Unlimited);
    assert!(matches!(
        result,
        Err(RecordError::Cells(CellError::History(
            golemdb_history::HistoryError::MissingPreimage { .. }
        )))
    ));
    assert_eq!(accounting.cost, 37);
}
#[test]
fn point_charges_and_every_budget_boundary_on_memory_and_mdbx() {
    point_contract(&MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    point_contract(&MdbxStore::open(dir.path()).unwrap());
}

fn scan_fixture(store: &impl Store) {
    let mut tx = store.begin_write().unwrap();
    let value = CellValue::from_u64(7);
    tx.put(tables::CELL, &key(b"a").encode(), value.encoded_bytes())
        .unwrap();
    tables::HISTORY
        .record(&mut tx, &key(b"b").encode(), 10, value.encoded_bytes())
        .unwrap();
    tx.put(tables::CELL, &key(b"c").encode(), value.encoded_bytes())
        .unwrap();
    tables::HISTORY
        .record(&mut tx, &key(b"c").encode(), 10, b"")
        .unwrap();
    let outside = CellKey::new(65, CellNameRef::raw(b"bad")).encode();
    tx.put(tables::CELL_HISTORY, &outside, b"bad bitmap")
        .unwrap();
    tx.commit().unwrap();
}
fn scan_contract(store: &impl Store) {
    scan_fixture(store);
    let tx = observed(store.begin_read().unwrap());
    let schedule = schedule();
    let mut meter = ReadMeter::new(&schedule, Budget::Unlimited);
    let mut cells = CellReader::at(&tx, 0).scan_record(64).unwrap();
    assert!(tx.accesses.borrow().is_empty());
    assert_eq!(
        historical::next_cell(&mut cells, &mut meter, None)
            .unwrap()
            .unwrap()
            .0,
        key(b"a")
    );
    let first = meter.into_accounting();
    // b's history bitmap was already paid before copying it into lookahead,
    // although the first emitted cell is the live-only a.
    assert_eq!(first.cost, 60);
    assert_eq!(first.ledger.count("read.commit_set"), 1);
    let mut meter = ReadMeter::new(&schedule, Budget::Unlimited);
    assert_eq!(
        historical::next_cell(&mut cells, &mut meter, None)
            .unwrap()
            .unwrap()
            .0,
        key(b"b")
    );
    let second = meter.into_accounting();
    assert_eq!(second.cost, 22);
    assert_eq!(second.ledger.count("read.commit_set"), 0);
    let mut meter = ReadMeter::new(&schedule, Budget::Unlimited);
    assert!(
        historical::next_cell(&mut cells, &mut meter, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(meter.into_accounting().cost, 45);
    assert!(
        !tx.accesses
            .borrow()
            .contains(&Access::Lookup(tables::CELL_HISTORY)),
        "reuse scanned history rows"
    );
    drop(cells);
    drop(tx);
    for budget in 0..=127 {
        let tx = store.begin_read().unwrap();
        let (result, accounting) = scan(&tx, 0, Budget::Limited(budget));
        assert!(accounting.cost <= budget);
        if budget == 127 {
            assert_eq!(result.unwrap().len(), 2);
            assert_eq!(
                ledger(&accounting.ledger),
                counts([
                    (W::READ_CELL, 3),
                    (W::READ_CELL_SCAN, 6),
                    (W::READ_COMMIT_SET, 2),
                    (W::READ_BYTE, 96)
                ])
            );
        } else {
            assert!(matches!(result, Err(RecordError::Metering(_))));
        }
    }
}
#[test]
fn scans_charge_lookahead_once_count_both_cursors_and_skip_boundary_bitmaps() {
    scan_contract(&MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    scan_contract(&MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn every_scan_event_precedes_its_storage_operation_and_rejection_fuses() {
    let store = MemoryStore::new();
    scan_fixture(&store);
    let mut reached_end = false;
    for stop in 0..30 {
        let tx = observed(store.begin_read().unwrap());
        let mut scan = CellReader::at(&tx, 0).scan_record(64).unwrap();
        let mut authorized = vec![];
        let mut seen = 0;
        loop {
            let result = scan
                .next_with(
                    &mut (),
                    |_, event| {
                        if seen == stop {
                            return Err(CellError::Storage(StorageError::Full));
                        }
                        seen += 1;
                        match event {
                            ReadOperation::Advance(t) => authorized.push(Access::Advance(t)),
                            ReadOperation::Lookup(t) => authorized.push(Access::Lookup(t)),
                            ReadOperation::CommitSet { .. } => {}
                        }
                        Ok(())
                    },
                    |_, _, _| Ok(()),
                )
                .unwrap_or(None);
            if result.is_none() {
                break;
            }
        }
        assert_eq!(*tx.accesses.borrow(), authorized, "stop {stop}");
        assert!(scan.next().is_none());
        assert_eq!(*tx.accesses.borrow(), authorized);
        if seen < stop {
            reached_end = true;
            break;
        }
    }
    assert!(reached_end);
}

#[test]
fn processing_and_returned_bytes_are_authorized_before_decode() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    let k = key(b"a").encode();
    tx.put(tables::CELL_HISTORY, &k, &[0]).unwrap();
    let (result, accounting) = point(&tx, b"a", 0, Budget::Limited(1));
    assert!(matches!(result, Err(RecordError::Metering(_))));
    assert_eq!(accounting.cost, 1);
    assert!(matches!(
        point(&tx, b"a", 0, Budget::Unlimited).0,
        Err(RecordError::Cells(CellError::History(_)))
    ));
    // The same malformed row must be refused at the scan's pre-copy checkpoint.
    let (result, accounting) = scan(&tx, 0, Budget::Limited(6));
    assert!(matches!(result, Err(RecordError::Metering(_))));
    assert_eq!(accounting.cost, 6);
    assert!(matches!(
        scan(&tx, 0, Budget::Unlimited).0,
        Err(RecordError::Cells(CellError::History(_)))
    ));
    tx.delete(tables::CELL_HISTORY, &k).unwrap();
    tx.put(tables::CELL, &k, &[0]).unwrap();
    assert!(matches!(
        point(&tx, b"a", 0, Budget::Limited(2)).0,
        Err(RecordError::Metering(_))
    ));
    assert!(matches!(
        point(&tx, b"a", 0, Budget::Unlimited).0,
        Err(RecordError::Cells(CellError::Value(_)))
    ));
}

#[test]
fn storage_failures_keep_only_admitted_charges() {
    let store = MemoryStore::new();
    scan_fixture(&store);
    for (access, cost) in [
        (Access::Lookup(tables::CELL_HISTORY), 1),
        (Access::Lookup(tables::CELL_CHANGE_SET), 37),
    ] {
        let mut tx = observed(store.begin_read().unwrap());
        tx.fail = Some(access);
        let (result, accounting) = point(&tx, b"b", 0, Budget::Unlimited);
        assert!(matches!(result, Err(RecordError::Cells(_))));
        assert_eq!(accounting.cost, cost);
    }
}

#[test]
fn full_record_prepays_only_returned_bytes_and_checks_counters_before_copying() {
    let store = MemoryStore::new();
    scan_fixture(&store);
    let tx = store.begin_read().unwrap();
    let (rows, per_cell) = scan(&tx, 0, Budget::Unlimited);
    let rows = rows.unwrap();
    let bytes = rows
        .iter()
        .map(|(k, v)| (k.encode().len() + v.encoded_bytes().len()) as u64)
        .sum();
    let meta = RecordMeta {
        cells: rows.len() as u64,
        cell_bytes: bytes,
        ..Default::default()
    };
    let schedule = schedule();
    let mut meter = ReadMeter::new(&schedule, Budget::Unlimited);
    let mut prepaid = RecordPrepayment::new(&meta, &mut meter).unwrap();
    let mut cells = CellReader::at(&tx, 0).scan_user_cells(64).unwrap();
    let mut actual = vec![];
    while let Some(row) = historical::next_cell(&mut cells, &mut meter, Some(&mut prepaid)).unwrap()
    {
        actual.push(row);
    }
    prepaid.finish().unwrap();
    assert_eq!(actual, rows);
    let total = meter.into_accounting();
    assert_eq!(total.cost, per_cell.cost + 91);
    assert_eq!(
        total.ledger.count("read.cell_scan"),
        per_cell.ledger.count("read.cell_scan")
    );
    let mut meter = ReadMeter::new(&schedule, Budget::Limited(bytes + 90));
    assert!(RecordPrepayment::new(&meta, &mut meter).is_err());
    assert!(meter.into_accounting().ledger.is_empty());
    let mut meter = ReadMeter::new(&schedule, Budget::Unlimited);
    let mut prepaid = RecordPrepayment::new(&RecordMeta::default(), &mut meter).unwrap();
    let mut cells = CellReader::at(&tx, 0).scan_user_cells(64).unwrap();
    assert!(matches!(
        historical::next_cell(&mut cells, &mut meter, Some(&mut prepaid)),
        Err(RecordError::CorruptState(_))
    ));
    assert!(cells.next().is_none());
}

fn retention_contract(store: &impl Store) {
    let mut tx = store.begin_write().unwrap();
    let k = key(b"a").encode();
    let value = CellValue::from_u64(7);
    for commit in [1, 100, 1000] {
        tables::HISTORY
            .record(&mut tx, &k, commit, value.encoded_bytes())
            .unwrap();
    }
    tables::HISTORY
        .record(&mut tx, &key(b"old").encode(), 100, b"")
        .unwrap();
    tx.put(tables::CELL, &k, value.encoded_bytes()).unwrap();
    tx.commit().unwrap();
    let old = store.begin_read().unwrap();
    let (old_value, old_point) = point(&old, b"a", 500, Budget::Unlimited);
    let (old_rows, old_scan) = scan(&old, 500, Budget::Unlimited);
    // Test-only cleanup below T, using the unchanged history keys/encoding.
    let mut tx = store.begin_write().unwrap();
    for commit in [1, 100] {
        tx.delete(tables::CELL_CHANGE_SET, &changeset_key(commit, &k))
            .unwrap();
    }
    tx.put(
        tables::CELL_HISTORY,
        &k,
        &[1000].into_iter().collect::<CommitSet>().encode().unwrap(),
    )
    .unwrap();
    tx.delete(tables::CELL_HISTORY, &key(b"old").encode())
        .unwrap();
    tx.delete(
        tables::CELL_CHANGE_SET,
        &changeset_key(100, &key(b"old").encode()),
    )
    .unwrap();
    tx.commit().unwrap();
    let tx = store.begin_read().unwrap();
    let (value, new_point) = point(&tx, b"a", 500, Budget::Unlimited);
    let (rows, new_scan) = scan(&tx, 500, Budget::Unlimited);
    assert_eq!(value.unwrap(), old_value.unwrap());
    assert_eq!(rows.unwrap(), old_rows.unwrap());
    assert_eq!(old_point.ledger.count("read.commit_set"), 1);
    assert_eq!(new_point.ledger.count("read.commit_set"), 1);
    assert_eq!(
        old_point.ledger.count("read.byte") - new_point.ledger.count("read.byte"),
        4
    );
    assert!(old_scan.cost > new_scan.cost);
    assert!(old_scan.ledger.count("read.cell_scan") > new_scan.ledger.count("read.cell_scan"));
    assert!(scan(&tx, 500, Budget::Limited(new_scan.cost)).0.is_ok());
    assert!(scan(&old, 500, Budget::Limited(new_scan.cost)).0.is_err());
}
#[test]
fn retention_changes_quantities_and_totals_but_not_the_flat_processing_unit() {
    retention_contract(&MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    retention_contract(&MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn index_point_container_and_term_hooks_authorize_each_operation() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    let term = IndexTerm::new("color", golemdb_cells::CellType::Str, b"blue").unwrap();
    for hi in 0..2 {
        let k = IndexKey {
            term: term.clone(),
            container_id: ContainerId::new(hi).unwrap(),
        };
        let bytes = BitmapContainer::from_values(k.container_id, [1])
            .encode()
            .unwrap();
        index::HISTORY
            .record(&mut tx, &k.encode(), 10, &bytes)
            .unwrap();
    }
    tx.commit().unwrap();
    let k = IndexKey {
        term: term.clone(),
        container_id: ContainerId::new(0).unwrap(),
    };
    let tx = observed(store.begin_read().unwrap());
    let mut events = vec![];
    IndexReader::at(&tx, 0)
        .with_encoded_container_with_callbacks(
            &k,
            &mut events,
            |events, event| {
                events.push(event);
                Ok::<_, IndexError>(())
            },
            |_, raw| Ok(raw.unwrap().len()),
        )
        .unwrap();
    assert_eq!(
        events,
        [
            ReadOperation::Lookup(index::INDEX_HISTORY),
            ReadOperation::CommitSet { encoded_bytes: 30 },
            ReadOperation::Lookup(index::INDEX_CHANGE_SET)
        ]
    );
    // Rejection at every generic event, including term assembly lookahead.
    for stop in 0..12 {
        let tx = observed(store.begin_read().unwrap());
        let mut terms = IndexReader::at(&tx, 0).terms(TermRange::all()).unwrap();
        let mut authorized = vec![];
        let mut seen = 0;
        let result = terms.next_with(
            &mut (),
            |_, event| {
                if seen == stop {
                    return Err(IndexError::Storage(StorageError::Full));
                }
                seen += 1;
                match event {
                    ReadOperation::Lookup(t) => authorized.push(Access::Lookup(t)),
                    ReadOperation::Advance(t) => authorized.push(Access::Advance(t)),
                    ReadOperation::CommitSet { .. } => {}
                }
                Ok(())
            },
            |_, _, _| Ok(()),
        );
        assert_eq!(*tx.accesses.borrow(), authorized);
        match result {
            Ok(row) => assert_eq!(row.unwrap().1.treemap().len(), 2),
            Err(_) => {
                assert!(terms.next().is_none());
                assert_eq!(*tx.accesses.borrow(), authorized);
            }
        }
    }
    let mut terms = IndexReader::at(&tx, 0).terms(TermRange::all()).unwrap();
    assert!(
        terms
            .next_with(
                &mut (),
                |_, _| Ok(()),
                |_, _, _| Err(IndexError::Storage(StorageError::Full))
            )
            .is_err()
    );
    assert!(terms.next().is_none());
}
