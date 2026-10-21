use golemdb_branch::{BranchError, Branches};
use golemdb_cells::{
    CellChange, CellKey, CellName, CellNameRef, CellType, CellValue, CellValueRef, Cells, Width,
    reserved, system,
};
use golemdb_index::{Index, IndexTerm};
use golemdb_merkle::{HashProvider, Keccak256Hasher, RootRef};
use golemdb_record::{CellPatch, ReadTarget, RecordCells, RecordError, RecordKey, Records};
use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction};

const KEY: RecordKey = RecordKey([0x42; 32]);
const OTHER: RecordKey = RecordKey([0x43; 32]);

fn name(bytes: &[u8]) -> CellName {
    CellNameRef::raw(bytes).into()
}
fn field(ty: CellType, bytes: &[u8]) -> CellValue {
    CellValueRef::new(ty, bytes, false).unwrap().into()
}
fn text(s: &str, indexed: bool) -> CellValue {
    CellValueRef::new(CellType::Str, s.as_bytes(), indexed)
        .unwrap()
        .into()
}
fn u64_value(n: u64) -> CellValue {
    field(CellType::Uint(Width::W8), &n.to_be_bytes())
}
fn values(s: &str) -> RecordCells {
    [(name(b"name"), text(s, true))].into()
}
fn binding(key: RecordKey) -> CellKey {
    CellKey::new(system::RECORD_KEYS.id, CellNameRef::raw(&key.0))
}

// Explicit initialized state, as supplied by the future connection/genesis layer.
// Build the real cell trie so seal/commit exercise production root updates.
fn initialize(db: &impl Store) {
    let mut changes = Vec::new();
    let mut put = |id, name: CellNameRef<'_>, value| {
        changes.push(CellChange::Put {
            key: CellKey::new(id, name),
            value,
        });
    };
    for record in system::ALL {
        put(
            record.id,
            reserved::KEY,
            field(CellType::FixedBytes(Width::W32), &record.key),
        );
        put(
            system::RECORD_KEYS.id,
            CellNameRef::raw(&record.key),
            u64_value(record.id),
        );
    }
    for (name, value) in [
        (reserved::MAX_CELL_NAME_LEN, 32u32),
        (reserved::MAX_STR_LEN, 64),
        (reserved::MAX_BYTES_LEN, 128),
    ] {
        put(
            system::PARAMS.id,
            name,
            field(CellType::Uint(Width::W4), &value.to_be_bytes()),
        );
    }
    put(system::ALLOC.id, reserved::NEXT_RECORD_ID, u64_value(64));
    let hash = Keccak256Hasher;
    let mut tx = db.begin_write().unwrap();
    let update = Cells::new(&hash)
        .apply(&mut tx, RootRef::Empty, changes)
        .unwrap();
    tx.put(
        Table("Superblock"),
        b"head",
        &[
            0u64.to_be_bytes().as_slice(),
            &update.root.hash(&hash),
            &hash.hash(&[]),
        ]
        .concat(),
    )
    .unwrap();
    tx.commit().unwrap();
}

fn allocated<S: Store>(
    branches: &Branches<S, Keccak256Hasher>,
    b: golemdb_branch::BranchId,
) -> u64 {
    branches
        .read(b, |cell_reader| {
            cell_reader.get(&CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID))
        })
        .unwrap()
        .unwrap()
        .as_u64()
        .unwrap()
}
fn id<S: Store>(
    branches: &Branches<S, Keccak256Hasher>,
    b: golemdb_branch::BranchId,
    key: RecordKey,
) -> u64 {
    branches
        .read(b, |cell_reader| cell_reader.get(&binding(key)))
        .unwrap()
        .unwrap()
        .as_u64()
        .unwrap()
}

fn lifecycle(db: impl Store + Clone) {
    initialize(&db);
    let branches = Branches::new(db.clone(), Keccak256Hasher, None).unwrap();
    let records = Records::new(branches.clone());
    let b = branches.begin().unwrap();
    let competitor = branches.begin().unwrap();
    records.create(b, KEY, values("Alice")).unwrap();
    records.create(competitor, OTHER, values("loser")).unwrap();
    assert_eq!(id(&branches, competitor, OTHER), 64);
    branches.seal(b).unwrap();
    assert_eq!(
        records.get(ReadTarget::Branch(b), KEY, None).unwrap().cells[b"name".as_slice()].as_str(),
        Some("Alice")
    );
    assert!(matches!(
        records.delete(b, KEY),
        Err(RecordError::Branch(BranchError::Sealed))
    ));
    assert_eq!(
        branches.commit(b).unwrap(),
        golemdb_branch::CommitId::new(1)
    );
    assert!(matches!(
        records.get(ReadTarget::Branch(competitor), OTHER, None),
        Err(RecordError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        records.create(b, OTHER, values("consumed")),
        Err(RecordError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        records.get(ReadTarget::Commit(golemdb_branch::CommitId::new(0)), KEY, None),
        Err(RecordError::CommitUnavailable {
            requested, head }) if requested.get() == 0 && head.get() == 1
    ));
    assert!(matches!(
        records.get(ReadTarget::Commit(golemdb_branch::CommitId::new(2)), KEY, None),
        Err(RecordError::CommitUnavailable {
            requested, head }) if requested.get() == 2 && head.get() == 1
    ));
    assert_eq!(
        records
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(1)),
                KEY,
                None
            )
            .unwrap()
            .cells[b"name".as_slice()],
        text("Alice", true)
    );
    let term = IndexTerm::new("name", CellType::Str, b"Alice").unwrap();
    assert_eq!(
        Index::new(&Keccak256Hasher)
            .bitmap(&db.begin_read().unwrap(), &term)
            .unwrap()
            .unwrap()
            .treemap()
            .iter()
            .collect::<Vec<_>>(),
        vec![64]
    );
    // Changing kind/type removes the old posting. Another indexed cell is
    // deleted during recreation; the new incarnation must use only its own ID.
    let b = branches.begin().unwrap();
    records
        .patch(
            b,
            KEY,
            [
                (name(b"name"), CellPatch::Write(u64_value(9))),
                (name(b"old"), CellPatch::Write(text("indexed", true))),
            ]
            .into(),
        )
        .unwrap();
    branches.commit(b).unwrap();
    assert!(
        Index::new(&Keccak256Hasher)
            .bitmap(&db.begin_read().unwrap(), &term)
            .unwrap()
            .is_none()
    );
    let b = branches.begin().unwrap();
    records.delete(b, KEY).unwrap();
    records.create(b, KEY, values("Alice")).unwrap();
    assert_eq!(id(&branches, b, KEY), 65);
    branches.commit(b).unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(
        Index::new(&Keccak256Hasher)
            .bitmap(&tx, &term)
            .unwrap()
            .unwrap()
            .treemap()
            .iter()
            .collect::<Vec<_>>(),
        vec![65]
    );
    assert!(
        Index::new(&Keccak256Hasher)
            .bitmap(
                &tx,
                &IndexTerm::new("old", CellType::Str, b"indexed").unwrap()
            )
            .unwrap()
            .is_none()
    );
    drop(tx);
    let reopened = Branches::new(db, Keccak256Hasher, None).unwrap();
    let records = Records::new(reopened.clone());
    let b = reopened.begin().unwrap();
    assert_eq!(allocated(&reopened, b), 66);
    assert_eq!(id(&reopened, b, KEY), 65);
    assert_eq!(
        records
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(3)),
                KEY,
                None
            )
            .unwrap()
            .cells
            .len(),
        2
    );
    assert!(
        reopened
            .read(b, |cell_reader| cell_reader
                .scan_prefix(&64u64.to_be_bytes())?
                .collect::<golemdb_branch::Result<Vec<_>>>())
            .unwrap()
            .is_empty()
    );
    records.delete(b, KEY).unwrap();
    reopened.commit(b).unwrap();
    assert!(matches!(
        records.get(
            ReadTarget::Commit(golemdb_branch::CommitId::new(4)),
            KEY,
            None
        ),
        Err(RecordError::NotFound)
    ));
}

#[test]
fn memory_commit_reopen_and_index_lifecycle() {
    lifecycle(MemoryStore::new());
}

#[test]
fn mdbx_commit_and_database_reopen() {
    let dir = tempfile::tempdir().unwrap();
    lifecycle(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
    let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
    let branches = Branches::new(db, Keccak256Hasher, None).unwrap();
    let records = Records::new(branches.clone());
    let b = branches.begin().unwrap();
    assert_eq!(allocated(&branches, b), 66);
    assert!(matches!(
        records.get(
            ReadTarget::Commit(golemdb_branch::CommitId::new(4)),
            KEY,
            None
        ),
        Err(RecordError::NotFound)
    ));
    records.create(b, KEY, values("after restart")).unwrap();
    assert_eq!(id(&branches, b, KEY), 66);
    branches.commit(b).unwrap();
}
