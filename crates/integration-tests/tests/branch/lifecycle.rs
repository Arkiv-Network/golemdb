use golemdb_branch::{BranchError, BranchId, Branches, OperationError};
use golemdb_cells::{CellKey, CellNameRef, CellValue, tables};
use golemdb_merkle::{HashProvider, Keccak256Hasher};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction};

const SUPERBLOCK: Table = Table("Superblock");

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

fn lifecycle(db: impl Store + Clone) {
    // 1. Two branches start independently over the same published head.
    publish(&db, 7, "origin");
    let branches = Branches::new(db.clone(), Keccak256Hasher, None, None).unwrap();
    assert_eq!(branches.head().unwrap(), golemdb_branch::CommitId::new(7));
    let edited = branches.begin().unwrap();
    let sibling = branches.begin().unwrap();
    assert_eq!(
        branches.branch_info(edited).unwrap().commit_id,
        golemdb_branch::CommitId::new(7)
    );
    assert_eq!(
        branches.branch_info(sibling).unwrap().commit_id,
        golemdb_branch::CommitId::new(7)
    );
    assert!(sibling > edited);

    // 2. Writes and checkpoints in one branch are invisible to its sibling.
    put(&branches, edited, "one");
    branches.checkpoint(edited).unwrap();
    put(&branches, edited, "two");
    assert_eq!(get(&branches, edited), Some(value("two")));
    assert_eq!(get(&branches, sibling), Some(value("origin")));

    // 3. Rollback undoes each frame until the original cell is restored.
    branches.rollback(edited).unwrap();
    assert_eq!(get(&branches, edited), Some(value("one")));
    branches.rollback(edited).unwrap();
    assert_eq!(get(&branches, edited), Some(value("origin")));
    assert!(matches!(
        branches.rollback(edited),
        Err(BranchError::NoFrameToRollback)
    ));

    // 4. Prefix scans observe the restored state within the validated snapshot.
    let rows = branches
        .read(edited, |cells| {
            cells
                .scan_prefix(&64u64.to_be_bytes())?
                .collect::<golemdb_branch::Result<Vec<_>>>()
        })
        .unwrap();
    assert_eq!(rows, vec![(key(), value("origin"))]);

    // 5. Staging and discarding writes leave storage and the sibling untouched.
    put(&branches, edited, "staged");
    assert_eq!(
        db.begin_read()
            .unwrap()
            .get(tables::CELL, &key().encode())
            .unwrap(),
        Some(value("origin").into_bytes())
    );
    branches.discard(edited).unwrap();
    assert_invalid(&branches, edited);
    assert_eq!(get(&branches, sibling), Some(value("origin")));

    // 6. A replacement gets a fresh ID and starts from the published state.
    let replacement = branches.begin().unwrap();
    assert!(replacement > sibling);
    assert_eq!(get(&branches, replacement), Some(value("origin")));

    // 7. External publication invalidates old branches, even for overlay hits.
    put(&branches, sibling, "must not escape");
    publish(&db, 8, "new head");
    assert_eq!(branches.head().unwrap(), golemdb_branch::CommitId::new(8));
    assert_invalid(&branches, sibling);
    assert_invalid(&branches, replacement);

    // 8. A new branch sees the new head; lifecycle calls have not altered its roots.
    let fresh = branches.begin().unwrap();
    assert_eq!(
        branches.branch_info(fresh).unwrap().commit_id,
        golemdb_branch::CommitId::new(8)
    );
    assert_eq!(get(&branches, fresh), Some(value("new head")));
    let head = db
        .begin_read()
        .unwrap()
        .get(SUPERBLOCK, b"head")
        .unwrap()
        .unwrap();
    assert_eq!(&head[..8], &8u64.to_be_bytes());
    assert_eq!(&head[8..40], &[0x11; 32]);
    assert_eq!(&head[40..], &[0x22; 32]);
}

#[test]
fn memory_lifecycle() {
    lifecycle(MemoryStore::new());
}

#[test]
fn mdbx_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    lifecycle(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
}
