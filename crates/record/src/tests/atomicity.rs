use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{CellPatch, RecordKey, Records};
use golemdb_branch::Branches;
use golemdb_cells::{
    CellKey, CellNameRef, CellType, CellValue, CellValueRef, Width, reserved, system, tables,
};
use golemdb_merkle::Keccak256Hasher;
use golemdb_storage::{MemoryStore, ReadTransaction, StorageError, Store, Table, WriteTransaction};

struct FaultDatabase {
    inner: MemoryStore,
    fail: Arc<AtomicBool>,
}
struct FaultRead<R> {
    inner: R,
    fail: Arc<AtomicBool>,
}
impl Store for FaultDatabase {
    type Read<'a> = FaultRead<<MemoryStore as Store>::Read<'a>>;
    type Write<'a> = <MemoryStore as Store>::Write<'a>;
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        Ok(FaultRead {
            inner: self.inner.begin_read()?,
            fail: self.fail.clone(),
        })
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        self.inner.begin_write()
    }
}
impl<R: ReadTransaction> ReadTransaction for FaultRead<R> {
    type Cursor<'a>
        = R::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if self.fail.load(Ordering::Relaxed)
            && table == tables::CELL
            && key == CellKey::new(64, CellNameRef::raw(b"z")).encode()
        {
            return Err(StorageError::Backend(
                "injected late patch read failure".into(),
            ));
        }
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.inner.cursor(table, key)
    }
}

fn value(ty: CellType, bytes: &[u8]) -> CellValue {
    CellValueRef::new(ty, bytes, false).unwrap().into()
}

#[test]
fn late_storage_failure_restores_earlier_patch_writes_and_checkpoint_state() {
    late_patch_failure(false);
    late_patch_failure(true);
}

fn late_patch_failure(remove: bool) {
    let db = MemoryStore::new();
    // Minimal read-only origin fixture; this test never seals or commits it.
    let mut tx = db.begin_write().unwrap();
    tx.put(Table("Superblock"), b"head", &[0; 72]).unwrap();
    for name in [
        reserved::MAX_STR_LEN,
        reserved::MAX_BYTES_LEN,
        reserved::MAX_CELL_NAME_LEN,
    ] {
        tx.put(
            tables::CELL,
            &CellKey::new(system::PARAMS.id, name).encode(),
            value(CellType::Uint(Width::W4), &64u32.to_be_bytes()).encoded_bytes(),
        )
        .unwrap();
    }
    let key = RecordKey([7; 32]);
    for (address, value) in [
        (
            CellKey::new(system::RECORD_KEYS.id, CellNameRef::raw(&key.0)),
            value(CellType::Uint(Width::W8), &64u64.to_be_bytes()),
        ),
        (
            CellKey::new(64, reserved::KEY),
            value(CellType::FixedBytes(Width::W32), &key.0),
        ),
        (
            CellKey::new(64, CellNameRef::raw(b"a")),
            value(CellType::Str, b"original"),
        ),
    ] {
        tx.put(tables::CELL, &address.encode(), value.encoded_bytes())
            .unwrap();
    }
    tx.commit().unwrap();

    let fail = Arc::new(AtomicBool::new(false));
    let branches = Branches::new(
        FaultDatabase {
            inner: db,
            fail: fail.clone(),
        },
        Keccak256Hasher,
        None,
    )
    .unwrap();
    let records = Records::new(branches.clone());
    let b = branches.begin().unwrap();
    branches.checkpoint(b).unwrap();
    branches.rollback(b).unwrap();
    let before = branches.branch_info(b).unwrap();
    fail.store(true, Ordering::Relaxed);
    let changes = [(b"a", b"changed".as_slice()), (b"z", b"new".as_slice())]
        .into_iter()
        .map(|(name, bytes)| {
            (
                CellNameRef::raw(name).into(),
                if remove {
                    CellPatch::Remove
                } else {
                    CellPatch::Set(value(CellType::Str, bytes))
                },
            )
        })
        .collect();
    assert!(matches!(
        records.patch(b, key, changes),
        Err(crate::RecordError::Branch(
            golemdb_branch::BranchError::Storage(_)
        ))
    ));
    fail.store(false, Ordering::Relaxed);
    assert_eq!(branches.branch_info(b).unwrap(), before);
    let read = records
        .get(crate::ReadTarget::Branch(b), key, None)
        .unwrap();
    assert_eq!(read.cells[b"a".as_slice()].as_str(), Some("original"));
    assert!(!read.cells.contains_key(b"z".as_slice()));
    // Failed patch did not reactivate the rolled-back frame: one rollback now
    // consumes the previous frame, then there are no frames left.
    branches.rollback(b).unwrap();
    assert!(matches!(
        branches.rollback(b),
        Err(golemdb_branch::BranchError::NoFrameToRollback)
    ));
}
