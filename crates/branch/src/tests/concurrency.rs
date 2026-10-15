use std::sync::mpsc;

use crate::{
    BranchError, Branches, OperationError,
    test_sync::{TIMEOUT, pause_after_lock, signal_contention},
};
use golemdb_cells::{CellKey, CellNameRef, CellValue};
use golemdb_merkle::{HashProvider, Keccak256Hasher};
use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction};

fn key(name: &[u8]) -> CellKey {
    CellKey::new(64, CellNameRef::raw(name))
}

fn value(text: &str) -> CellValue {
    CellValue::parse([b"\x02".as_slice(), text.as_bytes()].concat()).unwrap()
}

fn publish_empty_head(db: &MemoryStore, commit: u64) {
    let root = Keccak256Hasher.hash(&[]);
    let mut tx = db.begin_write().unwrap();
    tx.put(
        Table("Superblock"),
        b"head",
        &[commit.to_be_bytes().as_slice(), &root, &root].concat(),
    )
    .unwrap();
    tx.commit().unwrap();
}

fn database() -> MemoryStore {
    let db = MemoryStore::new();
    publish_empty_head(&db, 0);
    db
}

#[test]
fn reader_cannot_observe_a_partial_multi_cell_write() {
    let branches = Branches::new(database(), Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(b"first"), value("old"));
            cells.put(key(b"second"), value("old"));
            Ok::<_, ()>(())
        })
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (contended_tx, contended_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let branches = &branches;
        let writer = scope.spawn(move || {
            branches.write(branch, |cells| {
                cells.put(key(b"first"), value("new"));
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(TIMEOUT).unwrap();
                cells.put(key(b"second"), value("new"));
                Ok::<_, ()>(())
            })
        });
        entered_rx.recv_timeout(TIMEOUT).unwrap();
        let reader = scope.spawn(move || {
            signal_contention(contended_tx, || {
                branches.read(branch, |cells| {
                    Ok::<_, BranchError>((cells.get(&key(b"first"))?, cells.get(&key(b"second"))?))
                })
            })
        });
        // The reader encountered the held lock while exactly one cell was new.
        contended_rx.recv_timeout(TIMEOUT).unwrap();
        release_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(
            reader.join().unwrap().unwrap(),
            (Some(value("new")), Some(value("new")))
        );
    });
}

#[test]
fn operation_waiting_for_branch_validates_head_after_acquiring_lock() {
    let db = database();
    let branches = Branches::new(db.clone(), Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (contended_tx, contended_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let branches = &branches;
        let writer = scope.spawn(move || {
            branches.write(branch, |cells| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(TIMEOUT).unwrap();
                cells.put(key(b"name"), value("old branch"));
                Ok::<_, ()>(())
            })
        });
        entered_rx.recv_timeout(TIMEOUT).unwrap();
        let reader = scope.spawn(move || {
            signal_contention(contended_tx, || {
                branches.read(branch, |_| -> Result<(), ()> {
                    panic!("stale callback ran")
                })
            })
        });
        // Do not advance head until the reader has reached the held branch lock.
        contended_rx.recv_timeout(TIMEOUT).unwrap();
        publish_empty_head(&db, 1);
        release_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        assert!(matches!(
            reader.join().unwrap(),
            Err(OperationError::Branch(BranchError::HandleInvalid))
        ));
    });
    assert!(matches!(
        branches.branch_info(branch),
        Err(BranchError::HandleInvalid)
    ));
}

#[derive(Clone, Copy)]
enum Consume {
    Commit,
    Discard,
}

fn queued_access_is_rejected(consume: Consume) {
    // Exercise reads and writes separately against the same consumption path.
    for write in [false, true] {
        let branches = Branches::new(database(), Keccak256Hasher, None).unwrap();
        let branch = branches.begin().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (contended_tx, contended_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let branches = &branches;
            let consumer = scope.spawn(move || {
                pause_after_lock(entered_tx, release_rx, || match consume {
                    Consume::Commit => branches.commit(branch).map(Some),
                    Consume::Discard => branches.discard(branch).map(|()| None),
                })
            });
            entered_rx.recv_timeout(TIMEOUT).unwrap();
            let queued = scope.spawn(move || {
                signal_contention(contended_tx, || {
                    if write {
                        branches.write(branch, |_| -> Result<(), ()> {
                            panic!("write callback ran on consumed branch")
                        })
                    } else {
                        branches.read(branch, |_| -> Result<(), ()> {
                            panic!("read callback ran on consumed branch")
                        })
                    }
                })
            });
            // The queued caller already holds the registry entry. Removing the
            // registry key alone must not let that caller revive the branch.
            contended_rx.recv_timeout(TIMEOUT).unwrap();
            release_tx.send(()).unwrap();
            let committed = consumer.join().unwrap().unwrap();
            assert_eq!(
                committed,
                match consume {
                    Consume::Commit => Some(crate::CommitId::new(1)),
                    Consume::Discard => None,
                }
            );
            assert!(matches!(
                queued.join().unwrap(),
                Err(OperationError::Branch(BranchError::HandleInvalid))
            ));
        });
        assert!(matches!(
            branches.branch_info(branch),
            Err(BranchError::HandleInvalid)
        ));
    }
}

#[test]
fn queued_reads_and_writes_reject_a_discarded_branch() {
    queued_access_is_rejected(Consume::Discard);
}

#[test]
fn queued_reads_and_writes_reject_a_committed_branch() {
    queued_access_is_rejected(Consume::Commit);
}

#[test]
fn concurrent_commits_of_one_branch_publish_only_once() {
    let branches = Branches::new(database(), Keccak256Hasher, None).unwrap();
    let branch = branches.begin().unwrap();
    branches
        .write(branch, |cells| {
            cells.put(key(b"name"), value("once"));
            Ok::<_, ()>(())
        })
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (contended_tx, contended_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let branches = &branches;
        let first = scope
            .spawn(move || pause_after_lock(entered_tx, release_rx, || branches.commit(branch)));
        entered_rx.recv_timeout(TIMEOUT).unwrap();
        let second =
            scope.spawn(move || signal_contention(contended_tx, || branches.commit(branch)));
        contended_rx.recv_timeout(TIMEOUT).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), crate::CommitId::new(1));
        assert!(matches!(
            second.join().unwrap(),
            Err(BranchError::HandleInvalid)
        ));
    });
    assert_eq!(branches.head().unwrap(), crate::CommitId::new(1));
}
