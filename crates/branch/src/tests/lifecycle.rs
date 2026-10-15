use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use crate::{BranchError, BranchId, BranchInfo, Branches, OperationError, tables::SUPERBLOCK};
use golemdb_cells::{CellKey, CellNameRef, CellValue, tables};
use golemdb_merkle::{HashProvider, Keccak256Hasher};
use golemdb_storage::{MemoryStore, ReadTransaction, StorageError, Store, Table, WriteTransaction};

fn key() -> CellKey {
    CellKey::new(64, CellNameRef::raw(b"name"))
}

fn value(text: &str) -> CellValue {
    CellValue::parse([b"\x02".as_slice(), text.as_bytes()].concat()).unwrap()
}

// Simulate external publication independently of the branch reader under test.
// Placeholder roots suffice here: these tests do not seal or commit.
fn publish(db: &impl Store, commit: u64, text: &str) {
    let mut row = commit.to_be_bytes().to_vec();
    row.extend_from_slice(&[0x11; 32]);
    row.extend_from_slice(&[0x22; 32]);
    let mut tx = db.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"head", &row).unwrap();
    crate::head::write_format(&mut tx, 0).unwrap();
    tx.put(tables::CELL, &key().encode(), value(text).encoded_bytes())
        .unwrap();
    tx.commit().unwrap();
}

fn get<S: Store>(branches: &Branches<S, impl HashProvider>, handle: BranchId) -> Option<CellValue> {
    branches.read(handle, |cells| cells.get(&key())).unwrap()
}

fn put<S: Store>(branches: &Branches<S, impl HashProvider>, handle: BranchId, text: &str) {
    branches
        .write(handle, |cells| {
            cells.put(key(), value(text));
            Ok::<_, BranchError>(())
        })
        .unwrap();
}

fn assert_invalid<S: Store>(branches: &Branches<S, impl HashProvider>, handle: BranchId) {
    assert!(matches!(
        branches.branch_info(handle),
        Err(BranchError::HandleInvalid)
    ));
    // Invalid calls must never invoke user callbacks, even for overlay hits.
    assert!(matches!(
        branches.read(handle, |_| -> Result<(), ()> {
            panic!("invalid read admitted")
        }),
        Err(OperationError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        branches.write(handle, |_| -> Result<(), ()> {
            panic!("invalid write admitted")
        }),
        Err(OperationError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        branches.checkpoint(handle),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.rollback(handle),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.discard(handle),
        Err(BranchError::HandleInvalid)
    ));
}

#[test]
fn branch_info_counts_retained_undo_entries_instead_of_callbacks_or_net_changes() {
    let db = MemoryStore::new();
    publish(&db, 9, "origin");
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    let id = branches.begin().unwrap();
    let info = BranchInfo {
        commit_id: crate::CommitId::new(9),
        branch_id: id,
        version: 0,
        sealed: false,
    };
    assert_eq!(branches.branch_info(id).unwrap(), info);
    branches
        .write(id, |cells| {
            cells.put(key(), value("first"));
            cells.put(key(), value("second"));
            cells.delete(CellKey::new(64, CellNameRef::raw(b"other")));
            Ok::<_, BranchError>(())
        })
        .unwrap();
    // One callback, two distinct cell addresses, three undo entries.
    assert_eq!(
        branches.branch_info(id).unwrap(),
        BranchInfo { version: 3, ..info }
    );
    branches.checkpoint(id).unwrap();
    get(&branches, id);
    put(&branches, id, "second"); // Already the overlay value: no new undo entry.
    assert_eq!(branches.branch_info(id).unwrap().version, 3);
    put(&branches, id, "third");
    assert_eq!(branches.branch_info(id).unwrap().version, 4);
    let error = branches.write(id, |cells| {
        cells.delete(key());
        cells.put(key(), value("temporary"));
        Err::<(), _>("reject")
    });
    assert!(matches!(error, Err(OperationError::Operation("reject"))));
    assert_eq!(branches.branch_info(id).unwrap().version, 4);
    assert!(
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = branches.write(id, |cells| -> Result<(), ()> {
                cells.delete(key());
                panic!("abort operation");
            });
        }))
        .is_err()
    );
    assert_eq!(branches.branch_info(id).unwrap().version, 4);
    branches.rollback(id).unwrap();
    assert_eq!(branches.branch_info(id).unwrap().version, 3);
    branches.rollback(id).unwrap();
    assert_eq!(branches.branch_info(id).unwrap(), info);
    put(&branches, id, "new work");
    assert_eq!(branches.branch_info(id).unwrap().version, 1);
    branches.discard(id).unwrap();
    assert_invalid(&branches, id);
}

#[test]
fn each_operation_independently_detects_a_stale_head() {
    let db = MemoryStore::new();
    publish(&db, 0, "origin");
    let branches = Branches::new(db.clone(), Keccak256Hasher).unwrap();
    let handles: Vec<_> = (0..6).map(|_| branches.begin().unwrap()).collect();
    for &handle in &handles {
        put(&branches, handle, "staged");
    }
    publish(&db, 1, "new head");
    assert!(matches!(
        branches.read(handles[0], |_| -> Result<(), ()> { panic!() }),
        Err(OperationError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        branches.write(handles[1], |_| -> Result<(), ()> { panic!() }),
        Err(OperationError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        branches.checkpoint(handles[2]),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.rollback(handles[3]),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.discard(handles[4]),
        Err(BranchError::HandleInvalid)
    ));
    assert!(matches!(
        branches.branch_info(handles[5]),
        Err(BranchError::HandleInvalid)
    ));
    for handle in handles {
        assert_invalid(&branches, handle);
    }
}

#[test]
fn missing_and_malformed_heads_are_errors_not_genesis() {
    let db = MemoryStore::new();
    assert!(matches!(
        Branches::new(db.clone(), Keccak256Hasher),
        Err(BranchError::MissingHead)
    ));
    assert_eq!(
        db.begin_read().unwrap().get(SUPERBLOCK, b"head").unwrap(),
        None
    );
    for len in [0, 7, 8, 71, 73, 100] {
        let mut tx = db.begin_write().unwrap();
        tx.put(SUPERBLOCK, b"head", &vec![0; len]).unwrap();
        tx.commit().unwrap();
        assert!(
            matches!(Branches::new(db.clone(), Keccak256Hasher), Err(BranchError::InvalidHead { actual }) if actual == len)
        );
    }
    for number in [0, 0x0102_0304_0506_0708, u64::MAX] {
        publish(&db, number, "origin");
        let branches = Branches::new(db.clone(), Keccak256Hasher).unwrap();
        assert_eq!(branches.head().unwrap(), crate::CommitId::new(number));
        assert_eq!(
            branches
                .branch_info(branches.begin().unwrap())
                .unwrap()
                .commit_id,
            crate::CommitId::new(number)
        );
    }
}

#[test]
fn every_existing_handle_operation_rejects_bad_head_before_callback_or_mutation() {
    let db = MemoryStore::new();
    publish(&db, 0, "origin");
    let branches = Branches::new(db.clone(), Keccak256Hasher).unwrap();
    let b = branches.begin().unwrap();
    put(&branches, b, "keep");
    let mut tx = db.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"head", &[0; 8]).unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        branches.head(),
        Err(BranchError::InvalidHead { .. })
    ));
    assert!(matches!(
        branches.begin(),
        Err(BranchError::InvalidHead { .. })
    ));
    assert!(matches!(
        branches.branch_info(b),
        Err(BranchError::InvalidHead { .. })
    ));
    assert!(matches!(
        branches.read(b, |_| -> Result<(), ()> { panic!() }),
        Err(OperationError::Branch(BranchError::InvalidHead { .. }))
    ));
    assert!(matches!(
        branches.write(b, |_| -> Result<(), ()> { panic!() }),
        Err(OperationError::Branch(BranchError::InvalidHead { .. }))
    ));
    assert!(matches!(
        branches.checkpoint(b),
        Err(BranchError::InvalidHead { .. })
    ));
    assert!(matches!(
        branches.rollback(b),
        Err(BranchError::InvalidHead { .. })
    ));
    assert!(matches!(
        branches.discard(b),
        Err(BranchError::InvalidHead { .. })
    ));
    // Repair the fixture's head: failed admission did not consume the branch.
    publish(&db, 0, "origin");
    assert_eq!(get(&branches, b), Some(value("keep")));
    branches.rollback(b).unwrap();
    assert_eq!(get(&branches, b), Some(value("origin")));
}

#[test]
fn handles_are_unique_across_managers_and_clones_share_ownership() {
    let db = MemoryStore::new();
    publish(&db, 0, "origin");
    let first = Branches::new(db.clone(), Keccak256Hasher).unwrap();
    let second = Branches::new(db.clone(), Keccak256Hasher).unwrap();
    let a = first.begin().unwrap();
    let b = second.begin().unwrap();
    assert_ne!(a, b);
    assert_invalid(&second, a);
    assert_invalid(&first, b);
    assert_invalid(&first, crate::BranchId::new(u64::MAX));
    put(&first.clone(), a, "shared");
    assert_eq!(get(&first, a), Some(value("shared")));
    drop(first);
    let reopened = Branches::new(db, Keccak256Hasher).unwrap();
    let c = reopened.begin().unwrap();
    assert!(c > b);
    assert_invalid(&reopened, a);
    assert_eq!(get(&reopened, c), Some(value("origin")));
}

#[test]
fn callback_failures_and_panics_preserve_state_and_leave_manager_usable() {
    let db = MemoryStore::new();
    publish(&db, 0, "origin");
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    let b = branches.begin().unwrap();
    put(&branches, b, "keep");
    let error = branches.write(b, |cells| {
        cells.delete(key());
        Err::<(), _>("record rejected")
    });
    assert!(matches!(
        error,
        Err(OperationError::Operation("record rejected"))
    ));
    assert_eq!(get(&branches, b), Some(value("keep")));
    assert!(
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = branches.write(b, |cells| -> Result<(), ()> {
                cells.put(key(), value("temporary"));
                panic!("record operation panic");
            });
        }))
        .is_err()
    );
    assert_eq!(get(&branches, b), Some(value("keep")));
    assert!(
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = branches.read(b, |_| -> Result<(), ()> { panic!("read panic") });
        }))
        .is_err()
    );
    branches.rollback(b).unwrap();
    assert_eq!(get(&branches, b), Some(value("origin")));
}

#[test]
fn head_check_and_cell_reads_use_one_snapshot_even_when_head_moves_mid_call() {
    let db = MemoryStore::new();
    publish(&db, 12, "old");
    let branches = Branches::new(db.clone(), Keccak256Hasher).unwrap();
    let b = branches.begin().unwrap();
    let reads = branches
        .read(b, |cells| {
            let before = cells.get(&key())?;
            publish(&db, 13, "new");
            let after = cells.get(&key())?;
            let scan = cells
                .scan_prefix(&64u64.to_be_bytes())?
                .collect::<crate::Result<Vec<_>>>()?;
            Ok::<_, BranchError>((before, after, scan))
        })
        .unwrap();
    assert_eq!(
        reads,
        (
            Some(value("old")),
            Some(value("old")),
            vec![(key(), value("old"))]
        )
    );
    assert_invalid(&branches, b);
    assert_eq!(
        get(&branches, branches.begin().unwrap()),
        Some(value("new"))
    );
}

#[test]
fn independent_branches_do_not_wait_for_another_branch_callback() {
    let db = MemoryStore::new();
    publish(&db, 0, "origin");
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    let a = branches.begin().unwrap();
    let b = branches.begin().unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let first = branches.clone();
        let worker = scope.spawn(move || {
            first
                .write(a, |cells| {
                    cells.put(key(), value("first"));
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok::<_, BranchError>(())
                })
                .unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        put(&branches, b, "second");
        assert_eq!(get(&branches, b), Some(value("second")));
        release_tx.send(()).unwrap();
        worker.join().unwrap();
    });
    assert_eq!(get(&branches, a), Some(value("first")));
}

#[test]
fn concurrent_operations_on_one_branch_are_atomic_and_do_not_lose_updates() {
    let db = MemoryStore::new();
    publish(&db, 0, "origin");
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    let b = branches.begin().unwrap();
    put(&branches, b, "0");
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let branches = &branches;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..10 {
                    branches
                        .write(b, |cells| {
                            let number: u32 = cells
                                .get(&key())?
                                .unwrap()
                                .as_str()
                                .unwrap()
                                .parse()
                                .unwrap();
                            std::thread::yield_now();
                            cells.put(key(), value(&(number + 1).to_string()));
                            Ok::<_, BranchError>(())
                        })
                        .unwrap();
                }
            });
        }
    });
    assert_eq!(get(&branches, b), Some(value("80")));
    branches.rollback(b).unwrap();
    assert_eq!(get(&branches, b), Some(value("origin")));
}

#[derive(Clone)]
struct FaultDatabase {
    inner: MemoryStore,
    fail_open: Arc<AtomicBool>,
    fail_head: Arc<AtomicBool>,
}

struct FaultRead {
    inner: <MemoryStore as Store>::Read<'static>,
    fail_head: Arc<AtomicBool>,
}

impl Store for FaultDatabase {
    type Read<'a> = FaultRead;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        if self.fail_open.load(Ordering::Relaxed) {
            return Err(StorageError::Poisoned("injected open failure"));
        }
        Ok(FaultRead {
            inner: self.inner.begin_read()?,
            fail_head: Arc::clone(&self.fail_head),
        })
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        panic!("branch lifecycle must not open a storage writer")
    }
}

impl ReadTransaction for FaultRead {
    type Cursor<'a> = <<MemoryStore as Store>::Read<'static> as ReadTransaction>::Cursor<'a>;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if table == SUPERBLOCK && self.fail_head.load(Ordering::Relaxed) {
            return Err(StorageError::Poisoned("injected head read failure"));
        }
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.inner.cursor(table, key)
    }
}

#[test]
fn storage_admission_failures_preserve_branch_and_do_not_run_callbacks() {
    let db = FaultDatabase {
        inner: MemoryStore::new(),
        fail_open: Arc::new(AtomicBool::new(false)),
        fail_head: Arc::new(AtomicBool::new(false)),
    };
    publish(&db.inner, 0, "origin");
    let branches = Branches::new(db.clone(), Keccak256Hasher).unwrap();
    let b = branches.begin().unwrap();
    put(&branches, b, "keep");
    for failure in [&db.fail_open, &db.fail_head] {
        failure.store(true, Ordering::Relaxed);
        assert!(matches!(branches.head(), Err(BranchError::Storage(_))));
        assert!(matches!(branches.begin(), Err(BranchError::Storage(_))));
        assert!(matches!(
            branches.branch_info(b),
            Err(BranchError::Storage(_))
        ));
        assert!(matches!(
            branches.read(b, |_| -> Result<(), ()> { panic!() }),
            Err(OperationError::Branch(BranchError::Storage(_)))
        ));
        assert!(matches!(
            branches.write(b, |_| -> Result<(), ()> { panic!() }),
            Err(OperationError::Branch(BranchError::Storage(_)))
        ));
        assert!(matches!(
            branches.checkpoint(b),
            Err(BranchError::Storage(_))
        ));
        assert!(matches!(branches.rollback(b), Err(BranchError::Storage(_))));
        assert!(matches!(branches.discard(b), Err(BranchError::Storage(_))));
        failure.store(false, Ordering::Relaxed);
        assert_eq!(get(&branches, b), Some(value("keep")));
    }
    branches.rollback(b).unwrap();
    assert_eq!(get(&branches, b), Some(value("origin")));
    branches.discard(b).unwrap();
}

#[test]
fn prefix_scans_match_encoded_keys_through_both_public_views() {
    use std::collections::BTreeMap;

    let db = MemoryStore::new();
    publish(&db, 0, "original");
    let cell = |id, name: &[u8]| CellKey::new(id, CellNameRef::raw(name));
    let mut expected = BTreeMap::from([(key(), value("original"))]);
    let mut tx = db.begin_write().unwrap();
    for key in [
        cell(0, b""),
        cell(64, b"na"),
        cell(64, b"name-long"),
        cell(64, b"nb"),
        cell(255, b"\xff"),
        cell(256, b""),
        cell(u64::MAX, b""),
        cell(u64::MAX, b"\xff\xff"),
    ] {
        tx.put(tables::CELL, &key.encode(), value("base").encoded_bytes())
            .unwrap();
        expected.insert(key, value("base"));
    }
    tx.commit().unwrap();
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    let branch = branches.begin().unwrap();
    let deleted = cell(64, b"na");
    let inserted = cell(64, b"name-new");
    let boundary_writes = [
        cell(0, b"\x00"),
        cell(255, b"\xff\xff"),
        cell(256, b"\x00"),
        cell(u64::MAX, b"\xff"),
        cell(u64::MAX, b"\xff\xff\xff"),
    ];
    for key in &boundary_writes {
        expected.insert(key.clone(), value("boundary"));
    }
    expected.remove(&deleted);
    expected.insert(key(), value("replacement"));
    expected.insert(inserted.clone(), value("inserted"));
    let mut prefixes = vec![vec![], vec![0xff], vec![0xff; 9], vec![0xff; 11]];
    // Every prefix length, including partial IDs, complete keys, and binary names.
    for key in expected.keys().chain([&deleted]) {
        let bytes = key.encode();
        prefixes.extend((1..=bytes.len()).map(|len| bytes[..len].to_vec()));
    }
    prefixes.push(cell(64, b"missing").encode());
    let matching = |prefix: &[u8]| {
        expected
            .iter()
            .filter(|(key, _)| key.encode().starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>()
    };
    branches
        .write(branch, |cells| {
            cells.delete(deleted);
            cells.put(key(), value("replacement"));
            cells.put(inserted, value("inserted"));
            for key in boundary_writes {
                cells.put(key, value("boundary"));
            }
            for prefix in &prefixes {
                let actual = cells
                    .scan_prefix(prefix)?
                    .collect::<crate::Result<Vec<_>>>()?;
                assert_eq!(actual, matching(prefix), "write prefix {prefix:?}");
            }
            Ok::<_, BranchError>(())
        })
        .unwrap();
    branches
        .read(branch, |cells| {
            for prefix in &prefixes {
                let actual = cells
                    .scan_prefix(prefix)?
                    .collect::<crate::Result<Vec<_>>>()?;
                assert_eq!(actual, matching(prefix), "read prefix {prefix:?}");
            }
            Ok::<_, BranchError>(())
        })
        .unwrap();
}
