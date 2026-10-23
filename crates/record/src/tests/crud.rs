use crate::{CellPatch, ReadTarget, RecordCells, RecordError, RecordKey, Records};
use golemdb_branch::Branches;
use golemdb_cells::{
    CellChange, CellKey, CellName, CellNameRef, CellType, CellValue, CellValueRef, Cells, Width,
    reserved, system,
};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_metering::Budget;
use golemdb_storage::{MemoryStore, Store, WriteTransaction};

const KEY: RecordKey = RecordKey([0x42; 32]);
const OTHER: RecordKey = RecordKey([0x43; 32]);

// Publish after acquiring a snapshot to deterministically exercise a concurrent
// head change during get, without timing-dependent thread scheduling.
struct AdvancingReadDb {
    db: MemoryStore,
    after_snapshot: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    reads: std::sync::atomic::AtomicUsize,
}

impl Store for AdvancingReadDb {
    type Read<'db> = <MemoryStore as Store>::Read<'db>;
    type Write<'db> = <MemoryStore as Store>::Write<'db>;

    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let snapshot = self.db.begin_read()?;
        let hook = self.after_snapshot.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        Ok(snapshot)
    }

    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        self.db.begin_write()
    }
}

#[test]
fn head_selection_and_record_read_share_one_snapshot() {
    use std::sync::atomic::Ordering;

    let (db, branches, records) = setup();
    let first = branches.begin().unwrap();
    records
        .create(
            first,
            KEY,
            create_entries(values("before")),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    branches.commit(first).unwrap();
    let next = branches.begin().unwrap();
    records
        .patch(
            next,
            KEY,
            patch_entries([(name(b"name"), CellPatch::Write(text("after", true)))].into()),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    let reader_branches = Branches::new(
        AdvancingReadDb {
            db,
            after_snapshot: std::sync::Mutex::new(None),
            reads: std::sync::atomic::AtomicUsize::new(0),
        },
        Keccak256Hasher,
        None,
    )
    .unwrap();
    let reader = Records::new(reader_branches.clone());
    let writer = branches.clone();
    *reader_branches.store().after_snapshot.lock().unwrap() = Some(Box::new(move || {
        writer.commit(next).unwrap();
    }));
    reader_branches.store().reads.store(0, Ordering::SeqCst);
    let snapshot = reader
        .get(ReadTarget::Head, KEY, None, Budget::Unlimited)
        .result
        .unwrap();
    assert_eq!(snapshot.cells[b"name".as_slice()].as_str(), Some("before"));
    assert_eq!(reader_branches.store().reads.load(Ordering::SeqCst), 1);
    assert_eq!(branches.head().unwrap(), golemdb_branch::CommitId::new(2));
    let latest = reader
        .get(ReadTarget::Head, KEY, None, Budget::Unlimited)
        .result
        .unwrap();
    assert_eq!(latest.cells[b"name".as_slice()].as_str(), Some("after"));
    assert!(matches!(
        reader.get(ReadTarget::Commit(golemdb_branch::CommitId::new(1)), KEY, None, Budget::Unlimited).result,
        Err(RecordError::CommitUnavailable {
            requested, head
        }) if requested.get() == 1 && head.get() == 2
    ));
    assert_eq!(
        reader
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(2)),
                KEY,
                None,
                Budget::Unlimited
            )
            .result
            .unwrap(),
        latest
    );
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

pub(super) fn setup() -> (
    MemoryStore,
    Branches<MemoryStore, Keccak256Hasher>,
    Records<MemoryStore, Keccak256Hasher>,
) {
    let db = MemoryStore::new();
    initialize(&db);
    let branches = Branches::new(db.clone(), Keccak256Hasher, None).unwrap();
    let records = Records::new(branches.clone());
    (db, branches, records)
}
fn snapshot<S: Store>(
    branches: &Branches<S, Keccak256Hasher>,
    b: golemdb_branch::BranchId,
) -> Vec<(CellKey, CellValue)> {
    branches
        .read(b, |cell_reader| {
            cell_reader
                .scan_prefix(&[])?
                .collect::<golemdb_branch::Result<_>>()
        })
        .unwrap()
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

#[test]
fn work_in_progress_is_isolated_and_projection_preserves_identity() {
    let (_, branches, records) = setup();
    let a = branches.begin().unwrap();
    let b = branches.begin().unwrap();
    assert_eq!(
        records
            .create(a, KEY, create_entries(values("Alice")), Budget::Unlimited)
            .result
            .unwrap(),
        KEY
    );
    assert_eq!(id(&branches, a, KEY), 64);
    let full = records
        .get(ReadTarget::Branch(a), KEY, None, Budget::Unlimited)
        .result
        .unwrap();
    assert_eq!(full.cells.len(), 3);
    assert_eq!(full.cells[b"#key".as_slice()].as_bytes32(), Some(KEY.0));
    assert!(!full.cells[b"#key".as_slice()].is_indexable());
    for target in [
        ReadTarget::Branch(b),
        ReadTarget::Commit(golemdb_branch::CommitId::new(0)),
    ] {
        assert!(matches!(
            records.get(target, KEY, None, Budget::Unlimited).result,
            Err(RecordError::NotFound)
        ));
    }
    let projection = [name(b"name"), name(b"missing"), name(b"name")];
    let projected = records
        .get(
            ReadTarget::Branch(a),
            KEY,
            Some(&projection),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(projected.cells, values("Alice"));
    assert!(
        records
            .get(ReadTarget::Branch(a), KEY, Some(&[]), Budget::Unlimited)
            .result
            .unwrap()
            .cells
            .is_empty()
    );
    assert!(matches!(
        records
            .get(ReadTarget::Branch(a), OTHER, Some(&[]), Budget::Unlimited)
            .result,
        Err(RecordError::NotFound)
    ));
    records
        .patch(
            a,
            KEY,
            patch_entries([(name(b"name"), CellPatch::Write(text("Bob", false)))].into()),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(
        records
            .get(ReadTarget::Branch(a), KEY, None, Budget::Unlimited)
            .result
            .unwrap()
            .cells[b"name".as_slice()],
        text("Bob", false)
    );
    records.delete(a, KEY, Budget::Unlimited).result.unwrap();
    assert!(matches!(
        records
            .get(ReadTarget::Branch(a), KEY, None, Budget::Unlimited)
            .result,
        Err(RecordError::NotFound)
    ));
}

#[test]
fn reserved_records_are_readable_but_only_exact_keys_are_protected() {
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    let before = snapshot(&branches, b);
    for reserved in system::ALL {
        let key = RecordKey(reserved.key);
        let result = records
            .get(ReadTarget::Branch(b), key, None, Budget::Unlimited)
            .result
            .unwrap();
        assert_eq!(result.cells[b"#key".as_slice()].as_bytes32(), Some(key.0));
        assert!(result.cells.values().all(|value| !value.is_indexable()));
        assert!(matches!(
            records
                .create(b, key, create_entries(values("x")), Budget::Unlimited)
                .result,
            Err(RecordError::Reserved)
        ));
        assert!(matches!(
            records
                .patch(b, key, patch_entries(Default::default()), Budget::Unlimited)
                .result,
            Err(RecordError::Reserved)
        ));
        assert!(matches!(
            records.delete(b, key, Budget::Unlimited).result,
            Err(RecordError::Reserved)
        ));
    }
    assert_eq!(snapshot(&branches, b), before);
    assert_eq!(branches.branch_info(b).unwrap().version, 0);
    let projection = [name(&system::ALLOC.key)];
    let bindings = records
        .get(
            ReadTarget::Commit(golemdb_branch::CommitId::new(0)),
            RecordKey(system::RECORD_KEYS.key),
            Some(&projection),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(
        bindings.cells[system::ALLOC.key.as_slice()].as_u64(),
        Some(1)
    );
    let mut similar = system::PARAMS.key;
    similar[31] = 1;
    records
        .create(
            b,
            RecordKey(similar),
            create_entries(values("ordinary")),
            Budget::Unlimited,
        )
        .result
        .unwrap();
}

#[test]
fn invalid_operations_leave_cells_allocator_and_version_unchanged() {
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
        .result
        .unwrap();
    let before = snapshot(&branches, b);
    let info = branches.branch_info(b).unwrap();
    assert!(matches!(
        records
            .create(
                b,
                KEY,
                create_entries(values("duplicate")),
                Budget::Unlimited
            )
            .result,
        Err(RecordError::AlreadyExists)
    ));
    for invalid in [
        b"#key".as_slice(),
        b"@admin",
        b"",
        b"9name",
        b"a\0b",
        &[b'a'; 33],
    ] {
        assert!(matches!(
            records
                .create(
                    b,
                    OTHER,
                    create_entries([(name(invalid), text("x", false))].into()),
                    Budget::Unlimited
                )
                .result,
            Err(RecordError::InvalidInput { .. })
        ));
        assert!(matches!(
            records
                .patch(
                    b,
                    KEY,
                    patch_entries([(name(invalid), CellPatch::Remove)].into()),
                    Budget::Unlimited
                )
                .result,
            Err(RecordError::InvalidInput { .. })
        ));
    }
    for value in [
        text(&"s".repeat(65), true),
        field(CellType::Bytes, &[0; 129]),
    ] {
        assert!(matches!(
            records
                .create(
                    b,
                    OTHER,
                    create_entries([(name(b"value"), value.clone())].into()),
                    Budget::Unlimited
                )
                .result,
            Err(RecordError::InvalidInput { .. })
        ));
        assert!(matches!(
            records
                .patch(
                    b,
                    KEY,
                    patch_entries([(name(b"value"), CellPatch::Write(value))].into()),
                    Budget::Unlimited
                )
                .result,
            Err(RecordError::InvalidInput { .. })
        ));
    }
    assert!(matches!(
        records
            .patch(
                b,
                OTHER,
                patch_entries(Default::default()),
                Budget::Unlimited
            )
            .result,
        Err(RecordError::NotFound)
    ));
    assert!(matches!(
        records.delete(b, OTHER, Budget::Unlimited).result,
        Err(RecordError::NotFound)
    ));
    assert_eq!(snapshot(&branches, b), before);
    assert_eq!(branches.branch_info(b).unwrap(), info);
}

#[test]
fn patches_update_cells_and_no_ops_add_no_journal_entries() {
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
        .result
        .unwrap();
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    let info = branches.branch_info(b).unwrap();
    records
        .patch(b, KEY, patch_entries(Default::default()), Budget::Unlimited)
        .result
        .unwrap();
    records
        .patch(
            b,
            KEY,
            patch_entries(
                [
                    (name(b"missing"), CellPatch::Remove),
                    (name(b"name"), CellPatch::Write(text("Alice", true))),
                ]
                .into(),
            ),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(branches.branch_info(b).unwrap(), info);
    records
        .patch(
            b,
            KEY,
            patch_entries(
                [
                    (name(b"name"), CellPatch::Remove),
                    (name(b"replacement"), CellPatch::Write(u64_value(7))),
                ]
                .into(),
            ),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    let record = records
        .get(ReadTarget::Branch(b), KEY, None, Budget::Unlimited)
        .result
        .unwrap();
    assert_eq!(record.cells.len(), 3);
    assert_eq!(record.cells[b"replacement".as_slice()].as_u64(), Some(7));
}

#[test]
fn deletion_counts_live_committed_and_pending_cells_and_rolls_back() {
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(
            b,
            KEY,
            create_entries(
                [
                    (name(b"a"), text("one", true)),
                    (name(b"b"), text("dropped", false)),
                    (name(b"c"), text("old", true)),
                ]
                .into(),
            ),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    records
        .patch(
            b,
            KEY,
            patch_entries(
                [
                    (name(b"b"), CellPatch::Remove),
                    (name(b"c"), CellPatch::Write(text("new", false))),
                    (name(b"d"), CellPatch::Write(text("fresh", true))),
                ]
                .into(),
            ),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    let before = snapshot(&branches, b);
    branches.checkpoint(b).unwrap();

    let deleted = records.delete(b, KEY, Budget::Unlimited);
    deleted.result.unwrap();
    assert_eq!(
        deleted.effects,
        crate::Effects {
            cells_deleted: 3,
            cell_bytes_deleted: 41,
            index_leaves: 2,
            index_bytes_deleted: 14,
            ..crate::Effects::default()
        }
    );
    assert!(
        branches
            .read(b, |reader| reader
                .scan_prefix(&64u64.to_be_bytes())?
                .collect::<golemdb_branch::Result<Vec<_>>>())
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        records
            .get(ReadTarget::Branch(b), KEY, None, Budget::Unlimited)
            .result,
        Err(RecordError::NotFound)
    ));
    branches.rollback(b).unwrap();
    assert_eq!(snapshot(&branches, b), before);
}

#[test]
fn rollback_restores_records_bindings_allocator_and_incarnations() {
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    let genesis = snapshot(&branches, b);
    records
        .create(
            b,
            KEY,
            create_entries(values("original")),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    let original = snapshot(&branches, b);
    branches.checkpoint(b).unwrap();
    records.delete(b, KEY, Budget::Unlimited).result.unwrap();
    records
        .create(
            b,
            KEY,
            create_entries([(name(b"new"), text("new", false))].into()),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(id(&branches, b, KEY), 65);
    assert_eq!(allocated(&branches, b), 66);
    assert!(
        !records
            .get(ReadTarget::Branch(b), KEY, None, Budget::Unlimited)
            .result
            .unwrap()
            .cells
            .contains_key(b"name".as_slice())
    );
    branches.rollback(b).unwrap();
    assert_eq!(snapshot(&branches, b), original);
    branches.rollback(b).unwrap();
    assert_eq!(snapshot(&branches, b), genesis);
    records
        .create(
            b,
            OTHER,
            create_entries(values("reused uncommitted allocation")),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    assert_eq!(id(&branches, b, OTHER), 64);
}

#[test]
fn corrupt_system_state_never_defaults_or_overwrites_records() {
    for bad in [
        None,
        Some(text("wrong type", false)),
        Some(u64_value(0)),
        Some(u64_value(u64::MAX)),
    ] {
        let (_, branches, records) = setup();
        let b = branches.begin().unwrap();
        branches
            .write(b, |cell_writer| {
                let key = CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID);
                match bad {
                    Some(value) => cell_writer.put(key, value),
                    None => cell_writer.delete(key),
                }
                Ok::<_, RecordError>(())
            })
            .unwrap();
        let before = snapshot(&branches, b);
        let info = branches.branch_info(b).unwrap();
        assert!(matches!(
            records
                .create(b, KEY, create_entries(values("x")), Budget::Unlimited)
                .result,
            Err(RecordError::CorruptState(_)) | Err(RecordError::RecordIdExhausted)
        ));
        assert_eq!(snapshot(&branches, b), before);
        assert_eq!(branches.branch_info(b).unwrap(), info);
    }
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(
            b,
            KEY,
            create_entries(values("existing")),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    branches
        .write(b, |cell_writer| {
            cell_writer.put(
                CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID),
                u64_value(64),
            );
            Ok::<_, RecordError>(())
        })
        .unwrap();
    let before = snapshot(&branches, b);
    assert!(matches!(
        records
            .create(b, OTHER, create_entries(values("x")), Budget::Unlimited)
            .result,
        Err(RecordError::CorruptState(_))
    ));
    assert_eq!(snapshot(&branches, b), before);
    branches
        .write(b, |cell_writer| {
            cell_writer.delete(CellKey::new(system::PARAMS.id, reserved::MAX_STR_LEN));
            Ok::<_, RecordError>(())
        })
        .unwrap();
    assert!(matches!(
        records
            .create(b, OTHER, create_entries(values("x")), Budget::Unlimited)
            .result,
        Err(RecordError::CorruptState(_))
    ));
}

#[test]
fn inconsistent_identity_and_reserved_id_aliases_are_rejected() {
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
        .result
        .unwrap();
    for bad_id in [0, 32, 63, 65] {
        branches
            .write(b, |cell_writer| {
                cell_writer.put(binding(OTHER), u64_value(bad_id));
                Ok::<_, RecordError>(())
            })
            .unwrap();
        let before = snapshot(&branches, b);
        assert!(matches!(
            records
                .get(ReadTarget::Branch(b), OTHER, None, Budget::Unlimited)
                .result,
            Err(RecordError::CorruptState(_))
        ));
        assert!(matches!(
            records.delete(b, OTHER, Budget::Unlimited).result,
            Err(RecordError::CorruptState(_))
        ));
        assert_eq!(snapshot(&branches, b), before);
    }
    branches
        .write(b, |cell_writer| {
            cell_writer.put(binding(OTHER), u64_value(64));
            Ok::<_, RecordError>(())
        })
        .unwrap();
    assert!(matches!(
        records
            .get(ReadTarget::Branch(b), OTHER, Some(&[]), Budget::Unlimited)
            .result,
        Err(RecordError::CorruptState(_))
    ));
}

#[test]
fn committed_cell_reader_keeps_one_snapshot_when_head_advances() {
    let (db, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(b, KEY, create_entries(values("before")), Budget::Unlimited)
        .result
        .unwrap();
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    records
        .patch(
            b,
            KEY,
            patch_entries([(name(b"name"), CellPatch::Write(text("after", true)))].into()),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(
        golemdb_branch::read_head(&tx).unwrap(),
        golemdb_branch::CommitId::new(1)
    );
    let cell_reader = golemdb_cells::CellReader::new(&tx);
    branches.commit(b).unwrap();
    assert_eq!(
        cell_reader.get(&CellKey::new(64, name(b"name"))).unwrap(),
        Some(text("before", true))
    );
    let stored = cell_reader
        .scan_record(64)
        .unwrap()
        .collect::<golemdb_cells::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[2].1, text("before", true));
    assert_eq!(
        records
            .get(
                ReadTarget::Commit(golemdb_branch::CommitId::new(2)),
                KEY,
                None,
                Budget::Unlimited
            )
            .result
            .unwrap()
            .cells[b"name".as_slice()],
        text("after", true)
    );
}

#[test]
fn committed_reads_by_id_match_get_and_check_the_commit() {
    let (db, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
        .result
        .unwrap();
    let head = branches.commit(b).unwrap();

    let (tx, at) = crate::committed_snapshot(&db, None).unwrap();
    assert_eq!(at, head);
    let cell_reader = golemdb_cells::CellReader::new(&tx);
    let only_name = [name(b"name")];
    for projection in [None, Some(&only_name[..]), Some(&[][..])] {
        assert_eq!(
            crate::read_committed_by_id(&cell_reader, 64, projection).unwrap(),
            records
                .get(ReadTarget::Head, KEY, projection, Budget::Unlimited)
                .result
                .unwrap()
        );
    }

    assert_eq!(crate::committed_snapshot(&db, Some(head)).unwrap().1, head);
    let older = golemdb_branch::CommitId::new(head.get() - 1);
    assert!(matches!(
        crate::committed_snapshot(&db, Some(older)),
        Err(RecordError::CommitUnavailable { requested, head: got })
            if requested == older && got == head
    ));
}

/// Callers pass IDs they know are live, so a record without a valid `#key`
/// is corrupt state, never an absent record.
#[test]
fn committed_reads_by_id_reject_a_missing_or_invalid_key_cell() {
    let key_cell = CellKey::new(64, reserved::KEY);
    for bad in [
        None,
        Some(text("not a key", false)),
        Some(field(CellType::FixedBytes(Width::W16), &[0x42; 16])),
    ] {
        let (db, branches, records) = setup();
        let b = branches.begin().unwrap();
        records
            .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
            .result
            .unwrap();
        branches
            .write(b, |cell_writer| {
                match bad.clone() {
                    Some(value) => cell_writer.put(key_cell.clone(), value),
                    None => cell_writer.delete(key_cell.clone()),
                }
                Ok::<_, RecordError>(())
            })
            .unwrap();
        branches.commit(b).unwrap();

        let (tx, _) = crate::committed_snapshot(&db, None).unwrap();
        let cell_reader = golemdb_cells::CellReader::new(&tx);
        assert!(
            matches!(
                crate::read_committed_by_id(&cell_reader, 64, None),
                Err(RecordError::CorruptState(_))
            ),
            "{bad:?}"
        );
    }
    let (db, _, _) = setup();
    let (tx, _) = crate::committed_snapshot(&db, None).unwrap();
    assert!(matches!(
        crate::read_committed_by_id(&golemdb_cells::CellReader::new(&tx), 64, None),
        Err(RecordError::CorruptState("record has no #key"))
    ));
}

pub(super) fn create_entries(cells: RecordCells) -> Vec<crate::CreateEntry> {
    cells
        .into_iter()
        .map(|(name, value)| crate::CreateEntry {
            name,
            kind: value.kind(),
            value,
        })
        .collect()
}

pub(super) fn patch_entries(changes: crate::RecordPatch) -> Vec<crate::PatchEntry> {
    use crate::{PatchChange, PatchEntry};
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

#[test]
fn metadata_has_the_specified_encoding_and_equal_size_updates_do_not_rewrite_it() {
    use crate::RecordMeta;
    let (_, branches, records) = setup();
    let b = branches.begin().unwrap();
    records
        .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
        .result
        .unwrap();
    let record = records
        .get(ReadTarget::Branch(b), KEY, None, Budget::Unlimited)
        .result
        .unwrap();
    let expected = RecordMeta {
        cells: 1,
        cell_bytes: 18,
        indexed_cells: 1,
        index_bytes: 11,
    };
    assert_eq!(record.meta(), Some(expected));
    // Literal wire fixture checks field order, byte order and the bytes32 field tag.
    assert_eq!(
        record.cells[b"#meta".as_slice()],
        CellValue::from_bytes32([
            0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 18, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0,
            0, 0, 11,
        ])
    );
    branches.commit(b).unwrap();
    let b = branches.begin().unwrap();
    records
        .patch(
            b,
            KEY,
            patch_entries([(name(b"name"), CellPatch::Write(text("Bobby", true)))].into()),
            Budget::Unlimited,
        )
        .result
        .unwrap();
    // Only the user cell is journaled; an identical put to an untouched #meta
    // would add another undo entry even though its persisted bytes were equal.
    assert_eq!(branches.branch_info(b).unwrap().version, 1);
    assert_eq!(
        records
            .get(ReadTarget::Branch(b), KEY, None, Budget::Unlimited)
            .result
            .unwrap()
            .meta(),
        Some(expected)
    );
}

#[test]
fn reads_return_stored_metadata_while_writes_reject_corruption() {
    use crate::RecordMeta;
    let valid = RecordMeta {
        cells: 1,
        cell_bytes: 18,
        indexed_cells: 1,
        index_bytes: 11,
    };
    for bad in [
        None,
        Some(text("wrong type", false)),
        Some(CellValue::from_bytes(&[0; 32])),
        Some(
            valid
                .encode()
                .with_kind(golemdb_cells::CellKind::Attribute)
                .unwrap(),
        ),
        Some(
            RecordMeta {
                indexed_cells: 2,
                ..valid
            }
            .encode(),
        ),
        Some(
            RecordMeta {
                cell_bytes: 0,
                ..valid
            }
            .encode(),
        ),
    ] {
        let (db, branches, records) = setup();
        let b = branches.begin().unwrap();
        records
            .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
            .result
            .unwrap();
        branches.commit(b).unwrap();
        let b = branches.begin().unwrap();
        branches
            .write(b, |writer| {
                match bad.clone() {
                    Some(value) => writer.put(CellKey::new(64, reserved::META), value),
                    None => writer.delete(CellKey::new(64, reserved::META)),
                }
                Ok::<_, RecordError>(())
            })
            .unwrap();
        let before = snapshot(&branches, b);
        let info = branches.branch_info(b).unwrap();
        let meta_projection = [name(b"#meta")];
        let projections = [None, Some(meta_projection.as_slice())];
        for projection in projections {
            let record = records
                .get(ReadTarget::Branch(b), KEY, projection, Budget::Unlimited)
                .result
                .unwrap();
            assert_eq!(record.cells.get(b"#meta".as_slice()), bad.as_ref());
            assert_eq!(record.meta(), None);
        }
        assert!(matches!(
            records
                .patch(b, KEY, patch_entries(Default::default()), Budget::Unlimited)
                .result,
            Err(RecordError::CorruptState(_))
        ));
        assert!(matches!(
            records.delete(b, KEY, Budget::Unlimited).result,
            Err(RecordError::CorruptState(_))
        ));
        assert_eq!(snapshot(&branches, b), before);
        assert_eq!(branches.branch_info(b).unwrap(), info);
        // Inject committed corruption directly: sealing itself rejects an
        // indexed system name before it can reach committed storage.
        let mut tx = db.begin_write().unwrap();
        let key = CellKey::new(64, reserved::META).encode();
        match &bad {
            Some(value) => tx
                .put(golemdb_cells::tables::CELL, &key, value.encoded_bytes())
                .unwrap(),
            None => {
                tx.delete(golemdb_cells::tables::CELL, &key).unwrap();
            }
        }
        tx.commit().unwrap();
        let (tx, _) = crate::committed_snapshot(&db, None).unwrap();
        let reader = golemdb_cells::CellReader::new(&tx);
        for projection in projections {
            let record = records
                .get(ReadTarget::Head, KEY, projection, Budget::Unlimited)
                .result
                .unwrap();
            assert_eq!(record.cells.get(b"#meta".as_slice()), bad.as_ref());
            assert_eq!(record.meta(), None);
            assert_eq!(
                crate::read_committed_by_id(&reader, 64, projection).unwrap(),
                record
            );
        }
    }
}

#[test]
fn metadata_underflow_and_delete_mismatch_fail_atomically() {
    use crate::RecordMeta;
    for bad in [
        // All are well-formed metadata; the operation discovers disagreement
        // with the actual stored cells. Additions must not mask underflow.
        RecordMeta {
            cells: 1,
            cell_bytes: 1,
            indexed_cells: 1,
            index_bytes: 1,
        },
        RecordMeta {
            cells: 2,
            cell_bytes: 18,
            indexed_cells: 1,
            index_bytes: 11,
        },
    ] {
        let (_, branches, records) = setup();
        let b = branches.begin().unwrap();
        records
            .create(b, KEY, create_entries(values("Alice")), Budget::Unlimited)
            .result
            .unwrap();
        branches
            .write(b, |writer| {
                writer.put(CellKey::new(64, reserved::META), bad.encode());
                Ok::<_, RecordError>(())
            })
            .unwrap();
        let before = snapshot(&branches, b);
        let info = branches.branch_info(b).unwrap();
        if bad.cells == 1 {
            assert!(matches!(
                records
                    .patch(
                        b,
                        KEY,
                        patch_entries(
                            [(name(b"name"), CellPatch::Write(text("Bobby", true)))].into()
                        ),
                        Budget::Unlimited
                    )
                    .result,
                Err(RecordError::CorruptState(_))
            ));
        }
        assert!(matches!(
            records.delete(b, KEY, Budget::Unlimited).result,
            Err(RecordError::CorruptState(_))
        ));
        assert_eq!(snapshot(&branches, b), before);
        assert_eq!(branches.branch_info(b).unwrap(), info);
    }
}
