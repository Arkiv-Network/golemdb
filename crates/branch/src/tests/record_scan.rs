use crate::{BranchError, CellRead, overlay::CellOverlay};
use golemdb_cells::{CellKey, CellNameRef, CellValue, tables};
use golemdb_storage::{
    Entry, MemoryStore, ReadCursor, ReadTransaction, Store, Table, WriteTransaction,
};
use proptest::prelude::*;
use std::{cell::Cell, rc::Rc};

fn key(record: u64, n: u16) -> CellKey {
    CellKey::new(record, CellNameRef::raw(format!("a{n:04}").as_bytes()))
}
fn seed(count: u16) -> MemoryStore {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    for n in 0..count {
        tx.put(
            tables::CELL,
            &key(64, n).encode(),
            CellValue::from_u32(n.into()).encoded_bytes(),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    store
}

proptest! {
    #[test]
    fn bounded_scans_match_the_existing_merge_through_edits_and_undo(
        actions in prop::collection::vec((0u8..6, 0u16..16), 1..150)
    ) {
        let store = seed(8);
        let tx = store.begin_read().unwrap();
        let mut overlay = CellOverlay::new();
        for (action, n) in actions {
            match action {
                0 => overlay.write(&tx, |w| {
                    w.put(key(64, n), CellValue::from_u32(1000));
                    Ok::<_, BranchError>(())
                }).unwrap(),
                1 => overlay.write(&tx, |w| {
                    let location = w.as_read().locate_cell(&key(64, n))?;
                    w.delete_at(location);
                    Ok::<_, BranchError>(())
                }).unwrap(),
                2 => overlay.checkpoint(),
                3 => { let _ = overlay.rollback(); },
                4 => {
                    let result = overlay.write(&tx, |w| {
                        let location = w.as_read().locate_cell(&key(64, n))?;
                        w.delete_at(location);
                        w.put(key(64, (n+1)%16), CellValue::from_u32(999));
                        Err::<(), _>(BranchError::Sealed)
                    });
                    prop_assert!(result.is_err());
                },
                _ => overlay.write(&tx, |w| {
                    // Splitting and then rejoining an origin-hidden interval.
                    w.put(key(64, n), CellValue::from_u32(5));
                    let location = w.as_read().locate_cell(&key(64, n))?;
                    w.delete_at(location);
                    Ok::<_, BranchError>(())
                }).unwrap(),
            }
            let reader = CellRead::new(&overlay, &tx);
            let expected = reader.scan_prefix(&64u64.to_be_bytes()).unwrap().collect::<crate::Result<Vec<_>>>().unwrap();
            let actual = reader.scan_user_cells(64).unwrap().collect::<crate::Result<Vec<_>>>().unwrap();
            prop_assert_eq!(actual, expected);
        }
    }
}

struct CountRead<R> {
    inner: R,
    steps: Rc<Cell<usize>>,
}
struct CountCursor<C> {
    inner: C,
    steps: Rc<Cell<usize>>,
}
impl<R: ReadTransaction> ReadTransaction for CountRead<R> {
    type Cursor<'a>
        = CountCursor<R::Cursor<'a>>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        Ok(CountCursor {
            inner: self.inner.cursor(table, key)?,
            steps: self.steps.clone(),
        })
    }
}
impl<C: ReadCursor> ReadCursor for CountCursor<C> {
    fn next(&mut self) -> golemdb_storage::Result<Option<Entry>> {
        self.steps.set(self.steps.get() + 1);
        self.inner.next()
    }
    fn prev(&mut self) -> golemdb_storage::Result<Option<Entry>> {
        self.steps.set(self.steps.get() + 1);
        self.inner.prev()
    }
}

#[test]
fn long_origin_and_overlay_tombstone_runs_take_bounded_scan_work() {
    let store = seed(128);
    let steps = Rc::new(Cell::new(0));
    let tx = CountRead {
        inner: store.begin_read().unwrap(),
        steps: steps.clone(),
    };
    let mut overlay = CellOverlay::new();
    overlay
        .write(&tx, |w| {
            // Leave only the last origin cell alive; add many origin-absent deletes.
            for n in (0..127).chain(128..1024) {
                let location = w.as_read().locate_cell(&key(64, n))?;
                w.delete_at(location);
            }
            Ok::<_, BranchError>(())
        })
        .unwrap();
    let reader = CellRead::new(&overlay, &tx);
    steps.set(0);
    let mut scan = reader.scan_user_cells(64).unwrap();
    assert_eq!(steps.get(), 0);
    assert_eq!(scan.next().unwrap().unwrap().0, key(64, 127));
    assert!(steps.get() <= 2, "{} origin rows", steps.get());
    steps.set(0);
    assert!(scan.next().is_none());
    assert!(steps.get() <= 1);
    assert!(scan.next().is_none());
}

#[test]
fn raw_tombstones_are_not_silently_scanned_by_the_bounded_interface() {
    let store = seed(2);
    let tx = store.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    overlay
        .write(&tx, |w| {
            w.delete(key(64, 0));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    assert!(matches!(
        CellRead::new(&overlay, &tx).scan_user_cells(64),
        Err(BranchError::InvalidScanState(_))
    ));
    overlay.rollback().unwrap();
    assert_eq!(
        CellRead::new(&overlay, &tx)
            .scan_user_cells(64)
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn scan_stays_in_its_record_and_handles_the_last_record_id() {
    let store = seed(2);
    let mut tx = store.begin_write().unwrap();
    for id in [63, 65, u64::MAX] {
        tx.put(
            tables::CELL,
            &key(id, 0).encode(),
            CellValue::from_u32(7).encoded_bytes(),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let tx = store.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    overlay
        .write(&tx, |w| {
            w.put(key(65, 1), CellValue::from_u32(8));
            let location = w.as_read().locate_cell(&key(u64::MAX, 0))?;
            w.delete_at(location);
            Ok::<_, BranchError>(())
        })
        .unwrap();
    let reader = CellRead::new(&overlay, &tx);
    assert_eq!(reader.scan_user_cells(64).unwrap().count(), 2);
    assert_eq!(reader.scan_user_cells(65).unwrap().count(), 2);
    assert_eq!(reader.scan_user_cells(u64::MAX).unwrap().count(), 0);
}
