use std::panic::{AssertUnwindSafe, catch_unwind};

use golemdb_cells::{CellChange, CellKey, CellNameRef, CellType, CellValueRef};
use golemdb_merkle::{Hash, HashProvider, Keccak256Hasher};
use golemdb_storage::{Database, MemoryDatabase, ReadTransaction, WriteTransaction, scan_prefix};

use crate::{
    BranchError, Branches, OperationError, genesis,
    head::{Head, write_format, write_head},
    publication_tests::ReadOnly,
    read_head,
    tables::{self, SUPERBLOCK},
};

const HASH: Keccak256Hasher = Keccak256Hasher;

fn snapshot(db: &impl Database) -> Vec<Vec<golemdb_storage::Entry>> {
    let tx = db.begin_read().unwrap();
    tables::ENGINE
        .into_iter()
        .map(|table| {
            scan_prefix(&tx, table, vec![])
                .unwrap()
                .collect::<golemdb_storage::Result<_>>()
                .unwrap()
        })
        .collect()
}

struct PanicHash;
impl HashProvider for PanicHash {
    fn hash_parts(&self, _: &[&[u8]]) -> Hash {
        panic!("unexpected hashing");
    }
}

#[test]
fn opening_fresh_format_does_not_write_or_hash() {
    let db = MemoryDatabase::new();
    genesis::create(&db, &HASH, []).unwrap();
    let before = snapshot(&db);
    let branches = Branches::new(ReadOnly::new(db.clone()), PanicHash).unwrap();
    assert_eq!(branches.head().unwrap(), 0);
    assert_eq!(read_head(&db.begin_read().unwrap()).unwrap(), 0);
    assert_eq!(snapshot(&db), before);
}

#[test]
fn head_reads_validate_format_in_their_own_snapshot() {
    let db = MemoryDatabase::new();
    let branches = Branches::create(db.clone(), HASH, []).unwrap();
    let snapshot = db.begin_read().unwrap();
    assert_eq!(
        snapshot.get(SUPERBLOCK, b"format").unwrap(),
        Some(1u32.to_be_bytes().to_vec())
    );
    assert_eq!(snapshot.get(SUPERBLOCK, b"format-version").unwrap(), None);
    assert_eq!(branches.head().unwrap(), 0);
    for bytes in [
        vec![1],
        vec![],
        2u32.to_be_bytes().to_vec(),
        vec![0, 0, 0, 1, 0],
    ] {
        let mut tx = db.begin_write().unwrap();
        tx.put(SUPERBLOCK, b"format", &bytes).unwrap();
        tx.commit().unwrap();
        assert_eq!(read_head(&snapshot).unwrap(), 0);
        assert!(matches!(
            read_head(&db.begin_read().unwrap()),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
        assert!(matches!(
            Branches::new(db.clone(), HASH),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
    }
    let mut tx = db.begin_write().unwrap();
    tx.delete(SUPERBLOCK, b"format").unwrap();
    tx.put(SUPERBLOCK, b"format-version", &[1]).unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        Branches::new(db, HASH),
        Err(BranchError::UnsupportedDatabaseFormat)
    ));
}

#[test]
fn legacy_head_including_commit_zero_is_rejected_without_touching_history() {
    for commit_id in [0, 50] {
        let db = MemoryDatabase::new();
        let mut tx = db.begin_write().unwrap();
        let empty = HASH.hash(&[]);
        write_head(
            &mut tx,
            &Head {
                commit_id,
                state_root: empty,
                index_root: empty,
            },
        )
        .unwrap();
        // Admission must not scan this malformed legacy row or try to migrate it.
        tx.put(golemdb_cells::tables::CELL_TRIE, b"legacy-key", b"old-data")
            .unwrap();
        tx.commit().unwrap();
        let before = snapshot(&db);
        assert!(matches!(
            Branches::new(ReadOnly::new(db.clone()), PanicHash),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
        assert!(matches!(
            genesis::create(&db, &PanicHash, []),
            Err(BranchError::DatabaseNotEmpty(_))
        ));
        assert_eq!(snapshot(&db), before);
    }
}

#[test]
fn creation_rejects_every_nonempty_engine_table_and_unknown_format() {
    for table in tables::ENGINE {
        let db = MemoryDatabase::new();
        let mut tx = db.begin_write().unwrap();
        tx.put(table, b"orphan", b"existing-data").unwrap();
        tx.commit().unwrap();
        let before = snapshot(&db);
        assert!(matches!(genesis::create(&db, &PanicHash, []),
            Err(BranchError::DatabaseNotEmpty(found)) if found == table));
        assert_eq!(snapshot(&db), before);
    }
    let db = MemoryDatabase::new();
    genesis::create(&db, &HASH, []).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"format", &2u32.to_be_bytes()).unwrap();
    tx.commit().unwrap();
    let before = snapshot(&db);
    assert!(matches!(
        Branches::new(ReadOnly::new(db.clone()), PanicHash),
        Err(BranchError::UnsupportedDatabaseFormat)
    ));
    assert_eq!(snapshot(&db), before);
}

#[test]
fn invalid_genesis_aborts_cell_writes_and_all_metadata_then_allows_retry() {
    let db = MemoryDatabase::new();
    let change = CellChange::Put {
        key: CellKey::new(64, CellNameRef::raw(b"\xff")),
        value: CellValueRef::new(CellType::Str, b"x", true).unwrap().into(),
    };
    assert!(matches!(
        genesis::create(&db, &HASH, [change]),
        Err(BranchError::Term(_))
    ));
    assert!(snapshot(&db).iter().all(Vec::is_empty));
    genesis::create(&db, &HASH, []).unwrap();
    Branches::new(db, HASH).unwrap();
}

#[test]
fn panicking_genesis_does_not_publish_or_poison_the_writer() {
    let db = MemoryDatabase::new();
    assert!(catch_unwind(AssertUnwindSafe(|| genesis::create(&db, &PanicHash, []))).is_err());
    assert!(snapshot(&db).iter().all(Vec::is_empty));
    db.begin_write().unwrap();
    genesis::create(&db, &HASH, []).unwrap();
    Branches::new(db, HASH).unwrap();
}

#[test]
fn missing_format_after_open_aborts_implicit_and_explicit_publication() {
    for explicit in [false, true] {
        let db = MemoryDatabase::new();
        genesis::create(&db, &HASH, []).unwrap();
        let branches = Branches::new(db.clone(), HASH).unwrap();
        let branch = branches.begin().unwrap();
        if explicit {
            branches.seal(branch).unwrap();
        }
        let mut tx = db.begin_write().unwrap();
        tx.delete(SUPERBLOCK, b"format").unwrap();
        tx.commit().unwrap();
        let before = snapshot(&db);
        assert!(matches!(
            branches.commit(branch),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
        assert_eq!(snapshot(&db), before);
        assert!(matches!(
            read_head(&db.begin_read().unwrap()),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
        assert!(matches!(
            branches.head(),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
        assert!(matches!(
            branches.begin(),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
        assert!(matches!(
            branches.branch_info(branch),
            Err(BranchError::UnsupportedDatabaseFormat)
        ));
        assert!(matches!(
            branches.read(branch, |_| -> Result<(), ()> {
                panic!("invalid format admitted a read")
            }),
            Err(OperationError::Branch(
                BranchError::UnsupportedDatabaseFormat
            ))
        ));
        assert!(matches!(
            branches.write(branch, |_| -> Result<(), ()> {
                panic!("invalid format admitted a write")
            }),
            Err(OperationError::Branch(
                BranchError::UnsupportedDatabaseFormat
            ))
        ));
        let mut tx = db.begin_write().unwrap();
        write_format(&mut tx, 0).unwrap();
        tx.commit().unwrap();
        assert_eq!(branches.head().unwrap(), 0);
        assert_eq!(branches.branch_info(branch).unwrap().sealed, explicit);
        assert_eq!(branches.commit(branch).unwrap(), 1);
    }
}

#[test]
fn unimplemented_retention_is_rejected_on_open_and_seal() {
    let db = MemoryDatabase::new();
    let branches = Branches::create(db.clone(), HASH, []).unwrap();
    assert_eq!(
        db.begin_read()
            .unwrap()
            .get(SUPERBLOCK, b"retention")
            .unwrap(),
        Some(0u64.to_be_bytes().to_vec())
    );
    let branch = branches.begin().unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(SUPERBLOCK, b"retention", &5u64.to_be_bytes())
        .unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        Branches::new(db.clone(), HASH),
        Err(BranchError::UnsupportedRetention(5))
    ));
    assert!(matches!(
        branches.seal(branch),
        Err(BranchError::UnsupportedRetention(5))
    ));
    let mut tx = db.begin_write().unwrap();
    tx.delete(SUPERBLOCK, b"retention").unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        Branches::new(db, HASH),
        Err(BranchError::UnsupportedDatabaseFormat)
    ));
}
