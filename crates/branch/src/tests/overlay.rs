use std::{cell::Cell, collections::BTreeMap, panic::AssertUnwindSafe};

use crate::{BranchError, overlay::CellOverlay};
use golemdb_cells::{CellChange, CellKey, CellNameRef, CellType, CellValue, CellValueRef, tables};
use golemdb_storage::{
    Entry, MemoryStore, ReadCursor, ReadTransaction, StorageError, Store, Table, WriteTransaction,
};

fn key(record: u64, name: &[u8]) -> CellKey {
    CellKey::new(record, CellNameRef::raw(name))
}

fn value(bytes: &[u8]) -> CellValue {
    CellValueRef::new(CellType::Str, bytes, false)
        .unwrap()
        .into()
}

fn seed(rows: &[(CellKey, CellValue)]) -> MemoryStore {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    for (key, value) in rows {
        tx.put(tables::CELL, &key.encode(), value.encoded_bytes())
            .unwrap();
    }
    tx.commit().unwrap();
    db
}

fn put(overlay: &mut CellOverlay, origin: &impl ReadTransaction, name: &[u8], bytes: &[u8]) {
    overlay
        .write(origin, |cells| {
            cells.put(key(64, name), value(bytes));
            Ok::<_, BranchError>(())
        })
        .unwrap();
}

#[test]
fn reads_merge_cells_without_writing_storage_or_sharing_overlays() {
    let name = key(64, b"name");
    let db = seed(&[(name.clone(), value(b"Alice"))]);
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    let other = CellOverlay::new();
    assert_eq!(overlay.get(&origin, &name).unwrap(), Some(value(b"Alice")));

    let returned = overlay
        .write(&origin, |cells| {
            cells.put(name.clone(), value(b"Bob"));
            assert_eq!(cells.get(&name)?, Some(value(b"Bob")));
            cells.delete(name.clone());
            assert_eq!(cells.get(&name)?, None);
            cells.put(name.clone(), value(b"Carol"));
            Ok::<_, BranchError>(17)
        })
        .unwrap();
    assert_eq!(returned, 17);
    assert_eq!(overlay.get(&origin, &name).unwrap(), Some(value(b"Carol")));
    assert_eq!(other.get(&origin, &name).unwrap(), Some(value(b"Alice")));
    assert_eq!(
        db.begin_read()
            .unwrap()
            .get(tables::CELL, &name.encode())
            .unwrap(),
        Some(value(b"Alice").into_bytes())
    );
    overlay.rollback().unwrap();
    assert_eq!(overlay.changes().count(), 0);
    assert_eq!(overlay.get(&origin, &name).unwrap(), Some(value(b"Alice")));
}

#[test]
fn scans_merge_in_order_hide_tombstones_and_stay_within_record() {
    let db = seed(&[
        (key(63, b"z"), value(b"previous record")),
        (key(64, b"a"), value(b"old")),
        (key(64, b"c"), value(b"remove")),
        (key(64, b"e"), value(b"keep")),
        (key(65, b"a"), value(b"next record")),
        (key(u64::MAX, b"a"), value(b"last")),
    ]);
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    overlay
        .write(&origin, |cells| {
            cells.put(key(64, b"a"), value(b"replace"));
            cells.put(key(64, b"b"), value(b"insert"));
            cells.delete(key(64, b"c"));
            cells.delete(key(64, b"d"));
            cells.put(key(64, b"f"), value(b"append"));
            cells.put(key(u64::MAX, b"\0\xff"), value(b"raw"));
            assert_eq!(
                cells
                    .scan_prefix(&64u64.to_be_bytes())?
                    .collect::<crate::Result<Vec<_>>>()?,
                vec![
                    (key(64, b"a"), value(b"replace")),
                    (key(64, b"b"), value(b"insert")),
                    (key(64, b"e"), value(b"keep")),
                    (key(64, b"f"), value(b"append")),
                ]
            );
            Ok::<_, BranchError>(())
        })
        .unwrap();
    assert_eq!(
        overlay
            .scan_prefix(&origin, &u64::MAX.to_be_bytes())
            .unwrap()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap(),
        vec![
            (key(u64::MAX, b"\0\xff"), value(b"raw")),
            (key(u64::MAX, b"a"), value(b"last")),
        ]
    );
    let mut empty = overlay.scan_prefix(&origin, &66u64.to_be_bytes()).unwrap();
    assert!(empty.next().is_none());
    assert!(empty.next().is_none());
    overlay.rollback().unwrap();
    assert_eq!(
        overlay
            .scan_prefix(&origin, &64u64.to_be_bytes())
            .unwrap()
            .count(),
        3
    );
}

#[test]
fn repeated_rollback_steps_back_and_new_writes_reuse_the_current_frame() {
    let db = MemoryStore::new();
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    put(&mut overlay, &origin, b"x", b"one");
    overlay.checkpoint();
    put(&mut overlay, &origin, b"x", b"two");
    overlay.checkpoint();
    put(&mut overlay, &origin, b"x", b"three");
    overlay.rollback().unwrap();
    assert_eq!(
        overlay.get(&origin, &key(64, b"x")).unwrap(),
        Some(value(b"two"))
    );
    put(&mut overlay, &origin, b"x", b"replacement");
    overlay.rollback().unwrap();
    assert_eq!(
        overlay.get(&origin, &key(64, b"x")).unwrap(),
        Some(value(b"two"))
    );
    overlay.rollback().unwrap();
    assert_eq!(
        overlay.get(&origin, &key(64, b"x")).unwrap(),
        Some(value(b"one"))
    );
    overlay.rollback().unwrap();
    assert_eq!(overlay.get(&origin, &key(64, b"x")).unwrap(), None);
    assert!(matches!(
        overlay.rollback(),
        Err(BranchError::NoFrameToRollback)
    ));
    put(&mut overlay, &origin, b"x", b"after exhaustion");
    overlay.rollback().unwrap();
    assert_eq!(overlay.changes().count(), 0);
}

#[test]
fn empty_frames_are_distinct_and_checkpoint_after_rollback_opens_one() {
    let db = MemoryStore::new();
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    put(&mut overlay, &origin, b"x", b"one");
    overlay.checkpoint();
    overlay.checkpoint();
    for _ in 0..2 {
        overlay.rollback().unwrap();
        assert_eq!(
            overlay.get(&origin, &key(64, b"x")).unwrap(),
            Some(value(b"one"))
        );
    }
    overlay.checkpoint();
    overlay.rollback().unwrap();
    assert_eq!(
        overlay.get(&origin, &key(64, b"x")).unwrap(),
        Some(value(b"one"))
    );
    overlay.rollback().unwrap();
    assert!(matches!(
        overlay.rollback(),
        Err(BranchError::NoFrameToRollback)
    ));
    overlay.checkpoint();
    overlay.rollback().unwrap();
    assert!(matches!(
        overlay.rollback(),
        Err(BranchError::NoFrameToRollback)
    ));
}

#[test]
fn failed_groups_restore_values_tombstones_and_fallthrough_without_consuming_frames() {
    let db = seed(&[(key(64, b"base"), value(b"origin"))]);
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    overlay
        .write(&origin, |cells| {
            cells.delete(key(64, b"deleted"));
            cells.put(key(64, b"present"), value(b"survives"));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    let before = overlay.changes().collect::<Vec<_>>();
    let result = overlay.write(&origin, |cells| {
        cells.put(key(64, b"deleted"), value(b"temporary"));
        cells.delete(key(64, b"present"));
        cells.delete(key(64, b"base"));
        cells.put(key(64, b"new"), value(b"temporary"));
        cells.put(key(64, b"new"), value(b"again"));
        Err::<(), _>("record validation failed")
    });
    assert_eq!(result, Err("record validation failed"));
    assert_eq!(overlay.changes().collect::<Vec<_>>(), before);
    assert_eq!(
        overlay.get(&origin, &key(64, b"base")).unwrap(),
        Some(value(b"origin"))
    );
    overlay.rollback().unwrap();
    assert_eq!(overlay.changes().count(), 0);
    assert!(matches!(
        overlay.rollback(),
        Err(BranchError::NoFrameToRollback)
    ));
}

#[test]
fn failed_and_readonly_groups_leave_rollback_status_but_noop_writes_reactivate_frame() {
    let db = MemoryStore::new();
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    put(&mut overlay, &origin, b"x", b"one");
    overlay.checkpoint();
    overlay.rollback().unwrap();
    assert_eq!(
        overlay.write(&origin, |cells| {
            cells.delete(key(64, b"x"));
            Err::<(), _>("failed")
        }),
        Err("failed")
    );
    overlay
        .write(&origin, |cells| cells.get(&key(64, b"x")))
        .unwrap();
    // An explicit no-op put reactivates the retained empty frame after rollback.
    put(&mut overlay, &origin, b"x", b"one");
    overlay.rollback().unwrap();
    assert_eq!(
        overlay.get(&origin, &key(64, b"x")).unwrap(),
        Some(value(b"one"))
    );
    overlay.rollback().unwrap();
    assert_eq!(overlay.changes().count(), 0);
    assert!(matches!(
        overlay.rollback(),
        Err(BranchError::NoFrameToRollback)
    ));
}

#[test]
fn failure_in_a_fresh_empty_frame_does_not_roll_back_the_previous_frame() {
    let db = MemoryStore::new();
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    put(&mut overlay, &origin, b"x", b"previous frame");
    overlay.checkpoint();
    assert_eq!(
        overlay.write(&origin, |_| Err::<(), _>("validation failed")),
        Err("validation failed")
    );
    overlay.rollback().unwrap();
    assert_eq!(
        overlay.get(&origin, &key(64, b"x")).unwrap(),
        Some(value(b"previous frame"))
    );
    overlay.rollback().unwrap();
    assert_eq!(overlay.changes().count(), 0);
}

#[test]
fn failed_reactivation_restores_rollback_status_including_noops_and_panics() {
    let db = MemoryStore::new();
    let origin = db.begin_read().unwrap();
    for noop in [false, true] {
        for panic in [false, true] {
            let mut overlay = CellOverlay::new();
            put(&mut overlay, &origin, b"x", b"one");
            overlay.checkpoint();
            put(&mut overlay, &origin, b"x", b"two");
            overlay.rollback().unwrap();

            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                overlay.write(&origin, |cells| {
                    cells.put(
                        key(64, b"x"),
                        value(if noop { b"one" } else { b"temporary" }),
                    );
                    assert!(!panic, "operation panic after frame reactivation");
                    Err::<(), _>("operation failed")
                })
            }));
            if panic {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), Err("operation failed"));
            }
            assert_eq!(
                overlay.get(&origin, &key(64, b"x")).unwrap(),
                Some(value(b"one"))
            );
            // Reads do not reactivate the rolled-back frame either.
            overlay
                .write(&origin, |cells| cells.get(&key(64, b"x")))
                .unwrap();
            // Go directly to frame 0: failed reactivation must not leave an
            // active empty frame in the way, even when no undo entry was added.
            overlay.rollback().unwrap();
            assert_eq!(overlay.changes().count(), 0);
            assert!(matches!(
                overlay.rollback(),
                Err(BranchError::NoFrameToRollback)
            ));
        }
    }
}

#[test]
fn unwinding_a_group_restores_the_overlay() {
    let db = MemoryStore::new();
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    put(&mut overlay, &origin, b"x", b"keep");
    let before = overlay.changes().collect::<Vec<_>>();
    let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _: Result<(), ()> = overlay.write(&origin, |cells| {
            cells.delete(key(64, b"x"));
            cells.put(key(64, b"new"), value(b"discard"));
            panic!("operation panic");
        });
    }));
    assert!(panic.is_err());
    assert_eq!(overlay.changes().collect::<Vec<_>>(), before);
    overlay.rollback().unwrap();
    assert_eq!(overlay.changes().count(), 0);
}

#[test]
fn changes_are_final_ordered_values_and_preserve_tag_and_reserved_cells() {
    let indexed: CellValue = CellValueRef::new(CellType::Str, b"same", true)
        .unwrap()
        .into();
    let db = seed(&[(key(64, b"x"), value(b"same"))]);
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    let alloc = key(1, b"#nextRecordID");
    let binding = key(3, &[0xff; 32]);
    overlay
        .write(&origin, |cells| {
            cells.put(key(64, b"x"), value(b"intermediate"));
            cells.put(key(64, b"x"), indexed.clone());
            cells.put(
                alloc.clone(),
                CellValue::parse([vec![0x0d], 65u64.to_be_bytes().to_vec()].concat())?,
            );
            cells.put(
                binding.clone(),
                CellValue::parse([vec![0x0d], 64u64.to_be_bytes().to_vec()].concat())?,
            );
            cells.put(key(64, b"gone"), value(b"temporary"));
            cells.delete(key(64, b"gone"));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    let changes = overlay.changes().collect::<Vec<_>>();
    assert_eq!(changes.len(), 4);
    assert!(matches!(&changes[0], CellChange::Put { key, .. } if key == &alloc));
    assert!(matches!(&changes[1], CellChange::Put { key, .. } if key == &binding));
    assert_eq!(
        changes[2],
        CellChange::Delete {
            key: key(64, b"gone")
        }
    );
    assert_eq!(
        changes[3],
        CellChange::Put {
            key: key(64, b"x"),
            value: indexed
        }
    );
    overlay.rollback().unwrap();
    assert_eq!(overlay.changes().count(), 0);
    assert_eq!(overlay.get(&origin, &alloc).unwrap(), None);
    assert_eq!(overlay.get(&origin, &binding).unwrap(), None);
}

#[test]
fn scans_report_bad_visible_values_once_but_skip_shadowed_values() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    tx.put(tables::CELL, &key(64, b"a").encode(), &[0]).unwrap();
    tx.put(
        tables::CELL,
        &key(64, b"b").encode(),
        value(b"valid").encoded_bytes(),
    )
    .unwrap();
    tx.commit().unwrap();
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    let mut scan = overlay.scan_prefix(&origin, &64u64.to_be_bytes()).unwrap();
    assert!(matches!(scan.next(), Some(Err(BranchError::Value(_)))));
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
    drop(scan);
    overlay
        .write(&origin, |cells| {
            cells.delete(key(64, b"a"));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    assert_eq!(
        overlay
            .scan_prefix(&origin, &64u64.to_be_bytes())
            .unwrap()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap(),
        vec![(key(64, b"b"), value(b"valid"))]
    );
    overlay.rollback().unwrap();
    assert!(matches!(
        overlay.get(&origin, &key(64, b"a")),
        Err(BranchError::Value(_))
    ));
}

struct FaultRead<R> {
    origin: R,
    fail: Cell<bool>,
    reads: Cell<usize>,
}

struct FaultCursor<'a, C> {
    inner: C,
    fail: &'a Cell<bool>,
    reads: &'a Cell<usize>,
}

fn check_fault(fail: &Cell<bool>, reads: &Cell<usize>) -> golemdb_storage::Result<()> {
    reads.set(reads.get() + 1);
    if fail.get() {
        Err(StorageError::Poisoned("injected failure"))
    } else {
        Ok(())
    }
}

impl<R: ReadTransaction> ReadTransaction for FaultRead<R> {
    type Cursor<'a>
        = FaultCursor<'a, R::Cursor<'a>>
    where
        Self: 'a;

    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        check_fault(&self.fail, &self.reads)?;
        self.origin.get(table, key)
    }

    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        check_fault(&self.fail, &self.reads)?;
        Ok(FaultCursor {
            inner: self.origin.cursor(table, key)?,
            fail: &self.fail,
            reads: &self.reads,
        })
    }
}

impl<C: ReadCursor> ReadCursor for FaultCursor<'_, C> {
    fn next(&mut self) -> golemdb_storage::Result<Option<Entry>> {
        check_fault(self.fail, self.reads)?;
        self.inner.next()
    }
    fn prev(&mut self) -> golemdb_storage::Result<Option<Entry>> {
        check_fault(self.fail, self.reads)?;
        self.inner.prev()
    }
}

#[test]
fn read_failure_aborts_only_its_group_and_rollback_never_reads_storage() {
    let db = MemoryStore::new();
    let origin = FaultRead {
        origin: db.begin_read().unwrap(),
        fail: Cell::new(false),
        reads: Cell::new(0),
    };
    let mut overlay = CellOverlay::new();
    put(&mut overlay, &origin, b"x", b"keep");
    overlay.checkpoint();
    put(&mut overlay, &origin, b"y", b"keep until rollback");
    let before = overlay.changes().collect::<Vec<_>>();
    origin.fail.set(true);
    let result = overlay.write(&origin, |cells| {
        cells.delete(key(64, b"x"));
        cells.get(&key(64, b"missing"))
    });
    assert!(matches!(result, Err(BranchError::Storage(_))));
    assert_eq!(overlay.changes().collect::<Vec<_>>(), before);
    let reads = origin.reads.get();
    assert_eq!(
        overlay.get(&origin, &key(64, b"x")).unwrap(),
        Some(value(b"keep"))
    );
    overlay.rollback().unwrap();
    overlay.rollback().unwrap();
    assert_eq!(origin.reads.get(), reads);
    assert_eq!(overlay.changes().count(), 0);
}

#[test]
fn scans_are_lazy_and_fuse_on_storage_error() {
    let db = seed(&[(key(64, b"x"), value(b"origin"))]);
    let origin = FaultRead {
        origin: db.begin_read().unwrap(),
        fail: Cell::new(false),
        reads: Cell::new(0),
    };
    let overlay = CellOverlay::new();
    let mut scan = overlay.scan_prefix(&origin, &64u64.to_be_bytes()).unwrap();
    assert_eq!(origin.reads.get(), 1); // Cursor opened, no rows consumed.
    origin.fail.set(true);
    assert!(matches!(scan.next(), Some(Err(BranchError::Storage(_)))));
    let reads = origin.reads.get();
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
    assert_eq!(origin.reads.get(), reads);
}

#[test]
fn record_delete_and_recreate_cells_can_be_undone_together() {
    let old = [
        (key(64, b"#key"), value(b"identity")),
        (key(64, b"name"), value(b"old")),
    ];
    let db = seed(&old);
    let origin = db.begin_read().unwrap();
    let mut overlay = CellOverlay::new();
    overlay
        .write(&origin, |cells| {
            let old = cells
                .scan_prefix(&64u64.to_be_bytes())?
                .collect::<crate::Result<Vec<_>>>()?;
            for (key, _) in old {
                cells.delete(key);
            }
            cells.put(key(65, b"#key"), value(b"identity"));
            cells.put(key(65, b"name"), value(b"new"));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    assert_eq!(
        overlay
            .scan_prefix(&origin, &64u64.to_be_bytes())
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        overlay
            .scan_prefix(&origin, &65u64.to_be_bytes())
            .unwrap()
            .count(),
        2
    );
    overlay.rollback().unwrap();
    assert_eq!(
        overlay
            .scan_prefix(&origin, &64u64.to_be_bytes())
            .unwrap()
            .collect::<crate::Result<BTreeMap<_, _>>>()
            .unwrap(),
        BTreeMap::from(old)
    );
    assert_eq!(
        overlay
            .scan_prefix(&origin, &65u64.to_be_bytes())
            .unwrap()
            .count(),
        0
    );
}
