use golemdb_api::Budget;
use golemdb_branch::{BranchError, Branches};
use golemdb_cells::{
    CellChange, CellKey, CellLimits, CellName, CellNameRef, CellType, CellValue, CellValueRef,
    Cells, Width, reserved, system,
};
use golemdb_index::{Index, IndexTerm};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_record::{
    CellPatch, ReadTarget, RecordCells, RecordError, RecordKey, RecordLimits, Records, WritePricing,
};
use golemdb_storage::{MemoryStore, Store, WriteTransaction};

const KEY: RecordKey = RecordKey([0x42; 32]);
const OTHER: RecordKey = RecordKey([0x43; 32]);

fn zero_pricing() -> WritePricing {
    let limits = CellLimits {
        max_cell_name_len: 32,
        max_str_len: 64,
        max_bytes_len: 128,
    };
    let record_limits = RecordLimits {
        max_cells: 100,
        max_indexed_cells: 100,
    };
    let store = MemoryStore::new();
    let cfg = golemdb_api::OpenConfig::new(golemdb_api::Genesis::new(
        golemdb_api::HashAlgorithm::Keccak256,
        limits,
        record_limits,
        None,
        golemdb_api::PricingConfig::zero(),
    ));
    golemdb_api::Database::from_store(store.clone(), &cfg).unwrap();
    let tx = store.begin_read().unwrap();
    let provider =
        golemdb_metering::PricingProvider::load(&golemdb_cells::CellReader::new(&tx), 0, &[])
            .unwrap();
    WritePricing::new(std::sync::Arc::new(provider), limits, record_limits)
}

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
    for name in [
        reserved::MAX_RECORD_CELLS,
        reserved::MAX_RECORD_INDEXED_CELLS,
    ] {
        put(system::PARAMS.id, name, u64_value(100));
    }
    put(system::ALLOC.id, reserved::NEXT_RECORD_ID, u64_value(64));
    let hash = Keccak256Hasher;
    let mut tx = db.begin_write().unwrap();
    let update = Cells::new(&hash)
        .apply(&mut tx, 0, RootRef::Empty, changes)
        .unwrap();
    let roots = golemdb_branch::Roots {
        state: update.root,
        index: RootRef::Empty,
    };
    golemdb_branch::init_head(&mut tx, &roots, &hash).unwrap();
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
    let branches = Branches::new(db.clone(), Keccak256Hasher, None, None).unwrap();
    let records = Records::new(branches.clone(), zero_pricing());
    let b = branches.begin().unwrap();
    let competitor = branches.begin().unwrap();
    records
        .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
        .result
        .unwrap();
    records
        .create(
            competitor,
            OTHER,
            create_entries(values("loser")),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(id(&branches, competitor, OTHER), 64);
    branches.seal(b).unwrap();
    assert_eq!(
        records
            .get(ReadTarget::Branch(b), KEY, None, Budget::Unlimited)
            .result
            .unwrap()
            .cells[b"name".as_slice()]
        .as_str(),
        Some("Alice")
    );
    assert!(matches!(
        records.delete(b, KEY, Budget::Unlimited).result,
        Err(RecordError::Branch(BranchError::Sealed))
    ));
    assert_eq!(
        branches.commit(b).unwrap(),
        golemdb_branch::CommitId::new(1)
    );
    assert!(matches!(
        records
            .get(
                ReadTarget::Branch(competitor),
                OTHER,
                None,
                Budget::Unlimited
            )
            .result,
        Err(RecordError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        records
            .create(
                b,
                OTHER,
                create_entries(values("consumed")),
                Budget::Unlimited
            )
            .result,
        Err(RecordError::Branch(BranchError::HandleInvalid))
    ));
    assert!(matches!(
        records.get(ReadTarget::Commit(golemdb_branch::CommitId::new(0)), KEY, None, Budget::Unlimited).result,
        Err(RecordError::CommitUnavailable {
            requested, head }) if requested.get() == 0 && head.get() == 1
    ));
    assert!(matches!(
        records.get(ReadTarget::Commit(golemdb_branch::CommitId::new(2)), KEY, None, Budget::Unlimited).result,
        Err(RecordError::CommitUnavailable {
            requested, head }) if requested.get() == 2 && head.get() == 1
    ));
    assert_eq!(
        records
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(1)),
                KEY,
                None,
                Budget::Unlimited
            )
            .result
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
            patch_entries(
                [
                    (name(b"name"), CellPatch::Write(u64_value(9))),
                    (name(b"old"), CellPatch::Write(text("indexed", true))),
                ]
                .into(),
            ),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    branches.commit(b).unwrap();
    assert!(
        Index::new(&Keccak256Hasher)
            .bitmap(&db.begin_read().unwrap(), &term)
            .unwrap()
            .is_none()
    );
    let b = branches.begin().unwrap();
    records.delete(b, KEY, Budget::Unlimited).result.unwrap();
    records
        .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
        .result
        .unwrap();
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
    let reopened = Branches::new(db, Keccak256Hasher, None, None).unwrap();
    let records = Records::new(reopened.clone(), zero_pricing());
    let b = reopened.begin().unwrap();
    assert_eq!(allocated(&reopened, b), 66);
    assert_eq!(id(&reopened, b, KEY), 65);
    assert_eq!(
        records
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(3)),
                KEY,
                None,
                Budget::Unlimited
            )
            .result
            .unwrap()
            .cells
            .len(),
        3
    );
    assert!(
        reopened
            .read(b, |cell_reader| cell_reader
                .scan_prefix(&64u64.to_be_bytes())?
                .collect::<golemdb_branch::Result<Vec<_>>>())
            .unwrap()
            .is_empty()
    );
    records.delete(b, KEY, Budget::Unlimited).result.unwrap();
    reopened.commit(b).unwrap();
    assert!(matches!(
        records
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(4)),
                KEY,
                None,
                Budget::Unlimited
            )
            .result,
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
    let branches = Branches::new(db, Keccak256Hasher, None, None).unwrap();
    let records = Records::new(branches.clone(), zero_pricing());
    let b = branches.begin().unwrap();
    assert_eq!(allocated(&branches, b), 66);
    assert!(matches!(
        records
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(4)),
                KEY,
                None,
                Budget::Unlimited
            )
            .result,
        Err(RecordError::NotFound)
    ));
    records
        .create(
            b,
            KEY,
            create_entries(values("after restart")),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(id(&branches, b, KEY), 66);
    branches.commit(b).unwrap();
}

fn create_entries(cells: RecordCells) -> Vec<golemdb_record::CreateEntry> {
    cells
        .into_iter()
        .map(|(name, value)| golemdb_record::CreateEntry {
            name,
            kind: value.kind(),
            value,
        })
        .collect()
}

fn patch_entries(changes: golemdb_record::RecordPatch) -> Vec<golemdb_record::PatchEntry> {
    use golemdb_record::{PatchChange, PatchEntry};
    changes
        .into_iter()
        .map(|(name, change)| PatchEntry {
            name,
            change: match change {
                CellPatch::Write(value) => PatchChange::Write {
                    kind: value.kind(),
                    value,
                },
                CellPatch::Set(value) => PatchChange::Set(value),
                CellPatch::Remove => PatchChange::Remove,
            },
        })
        .collect()
}
