use std::{cell::RefCell, collections::BTreeSet};

use golemdb_cells::{
    CellChange, CellError, CellKey, CellKind, CellNameRef, CellReader, CellType, CellValue,
    CellValueChange, Cells, history as cell_history, tables as cell_tables,
};
use golemdb_history::{CommitSet, HistoryError, changeset_key};
use golemdb_index::{
    BitmapContainer, BitmapError, ContainerId, ContainerValueChange, Index, IndexError, IndexKey,
    IndexReader, IndexTerm, PostingChange, history as index_history, tables as index_tables,
};
use golemdb_merkle::{Keccak256Hasher, RootRef};
use golemdb_storage::{MemoryStore, ReadTransaction, StorageError, Store, Table, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;
use proptest::prelude::*;

const HASH: Keccak256Hasher = Keccak256Hasher;

fn cell_key(name: &[u8]) -> CellKey {
    CellKey::new(u64::MAX, CellNameRef::raw(name))
}

fn index_key(container_id: ContainerId) -> IndexKey {
    IndexKey {
        term: IndexTerm::new("color", CellType::Str, b"blue").unwrap(),
        container_id,
    }
}

fn put(key: &CellKey, value: Option<CellValue>) -> CellChange {
    match value {
        Some(value) => CellChange::Put {
            key: key.clone(),
            value,
        },
        None => CellChange::Delete { key: key.clone() },
    }
}

fn posting(key: &IndexKey, offset: u16, present: bool) -> PostingChange {
    let term = key.term.clone();
    let record_id = key.container_id.record_id(offset);
    if present {
        PostingChange::Add { term, record_id }
    } else {
        PostingChange::Remove { term, record_id }
    }
}

fn offsets(reader: &IndexReader<'_, impl ReadTransaction>, key: &IndexKey) -> Vec<u32> {
    reader
        .container(key)
        .unwrap()
        .map_or_else(Vec::new, |c| c.bitmap().iter().collect())
}

// Commit 60 deliberately changes then restores both items within the batch.
fn cell_states() -> Vec<Option<CellValue>> {
    let field = CellValue::from_str("blue");
    vec![
        None,
        Some(field.clone()),
        Some(field.clone().with_kind(CellKind::Attribute).unwrap()),
        Some(
            CellValue::from_u64(7)
                .with_kind(CellKind::Attribute)
                .unwrap(),
        ),
        None,
        Some(field.clone()),
        Some(field),
    ]
}

fn check_timeline(tx: &impl ReadTransaction) {
    let key = cell_key(b"\0\xff");
    let first = index_key(ContainerId::new(0).unwrap());
    let last = index_key(ContainerId::MAX);
    let stable = index_key(ContainerId::new(1).unwrap());
    let states = cell_states();
    let expected_offsets = [
        vec![],
        vec![1, 2],
        vec![2, 3],
        vec![1, 2],
        vec![],
        vec![1, 2],
        vec![1, 2],
    ];
    for target in 0..=60 {
        let step = target as usize / 10;
        let cells = CellReader::at(tx, target);
        assert_eq!(cells.get(&key).unwrap(), states[step], "cell at {target}");
        assert_eq!(
            cells
                .with_encoded_value(&key, |bytes| bytes.map(<[u8]>::to_vec))
                .unwrap(),
            states[step].as_ref().map(|v| v.encoded_bytes().to_vec()),
        );
        assert_eq!(
            cells.get(&cell_key(b"stable")).unwrap(),
            Some(CellValue::from_bool(true))
        );
        let index = IndexReader::at(tx, target);
        assert_eq!(
            offsets(&index, &first),
            expected_offsets[step],
            "container at {target}"
        );
        assert_eq!(
            offsets(&index, &last),
            if target < 10 { vec![] } else { vec![65535] }
        );
        assert_eq!(offsets(&index, &stable), vec![42]);
        // Presence means this container, not the whole term, existed.
        assert_eq!(
            index
                .with_encoded_container(&first, |bytes| bytes.is_some())
                .unwrap(),
            !expected_offsets[step].is_empty(),
        );
    }
    for (tables, key, commits) in [
        (cell_tables::HISTORY, key.encode(), vec![10, 20, 30, 40, 50]),
        (
            index_tables::HISTORY,
            first.encode(),
            vec![10, 20, 30, 40, 50],
        ),
        (index_tables::HISTORY, last.encode(), vec![10]),
    ] {
        assert_eq!(
            tables.read_commits(tx, &key).unwrap(),
            commits.into_iter().collect::<CommitSet>()
        );
        assert_eq!(
            tx.get(tables.changeset, &changeset_key(10, &key)).unwrap(),
            Some(vec![])
        );
        assert_eq!(
            tx.get(tables.changeset, &changeset_key(60, &key)).unwrap(),
            None
        );
    }
}

fn timeline(db: &impl Store) {
    let cells = Cells::new(&HASH);
    let index = Index::new(&HASH);
    let key = cell_key(b"\0\xff");
    let first = index_key(ContainerId::new(0).unwrap());
    let last = index_key(ContainerId::MAX);
    let stable = index_key(ContainerId::new(1).unwrap());
    let states = cell_states();
    let mut tx = db.begin_write().unwrap();
    let mut cell_root = cells
        .apply(
            &mut tx,
            0,
            RootRef::Empty,
            [put(&cell_key(b"stable"), Some(CellValue::from_bool(true)))],
        )
        .unwrap()
        .root;
    let mut index_root = index
        .apply(&mut tx, 0, RootRef::Empty, [posting(&stable, 42, true)])
        .unwrap()
        .root;
    tx.commit().unwrap();
    // Genesis-like baseline rows have no artificial history and use live fallback.
    let old = db.begin_read().unwrap();
    let batches = [
        vec![
            posting(&last, 65535, true),
            posting(&first, 2, true),
            posting(&first, 1, true),
        ],
        vec![posting(&first, 1, false), posting(&first, 3, true)],
        vec![posting(&first, 1, true), posting(&first, 3, false)],
        vec![posting(&first, 1, false), posting(&first, 2, false)],
        vec![posting(&first, 1, true), posting(&first, 2, true)],
        vec![
            posting(&first, 1, false),
            posting(&first, 1, true),
            posting(&first, 9, true),
            posting(&first, 9, false),
        ],
    ];
    for (step, postings) in batches.into_iter().enumerate() {
        let commit = (step as u64 + 1) * 10;
        let mut tx = db.begin_write().unwrap();
        let cell_update = cells
            .apply(
                &mut tx,
                commit,
                cell_root,
                [
                    put(&key, Some(CellValue::from_str("intermediate"))),
                    put(&key, states[step + 1].clone()),
                ],
            )
            .unwrap();
        let before_first = tx.get(index_tables::INDEX, &first.encode()).unwrap();
        let update = index.apply(&mut tx, commit, index_root, postings).unwrap();
        assert_eq!(
            update.changed_containers.len(),
            if step == 0 {
                2
            } else if step == 5 {
                0
            } else {
                1
            }
        );
        assert_eq!(
            update.changed_terms,
            if step == 5 {
                vec![]
            } else {
                vec![first.term.clone()]
            }
        );
        if let Some(change) = update.changed_containers.first() {
            assert_eq!(change.key, first);
            assert_eq!(change.before, before_first);
            assert_eq!(
                change.after,
                tx.get(index_tables::INDEX, &first.encode()).unwrap()
            );
        }
        if step == 5 {
            assert!(cell_update.changed_cells.is_empty());
            assert_eq!(cell_update.root, cell_root);
            assert_eq!(update.root, index_root);
        }
        cell_history::record_changes(&mut tx, commit, &cell_update.changed_cells).unwrap();
        index_history::record_changes(&mut tx, commit, &update.changed_containers).unwrap();
        cells.validate_root(&tx, cell_update.root).unwrap();
        index.validate_root(&tx, update.root).unwrap();
        cell_root = cell_update.root;
        index_root = update.root;
        tx.commit().unwrap();
    }
    assert!(CellReader::at(&old, 0).get(&key).unwrap().is_none());
    assert!(
        IndexReader::at(&old, 0)
            .container(&first)
            .unwrap()
            .is_none()
    );
    assert_eq!(offsets(&IndexReader::at(&old, 0), &stable), vec![42]);
    check_timeline(&db.begin_read().unwrap());
}

#[test]
fn historical_cells_and_containers_on_memory() {
    timeline(&MemoryStore::new());
}

#[test]
fn historical_cells_and_containers_on_mdbx_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = MdbxStore::open(dir.path()).unwrap();
        timeline(&db);
    }
    let db = MdbxStore::open(dir.path()).unwrap();
    check_timeline(&db.begin_read().unwrap());
}

#[test]
fn malformed_selected_payloads_fail_and_only_empty_preimages_mean_absence() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    let cell = cell_key(b"value");
    let key = index_key(ContainerId::MAX);
    let encoded = BitmapContainer::from_values(key.container_id, [1])
        .encode()
        .unwrap();
    let cs = changeset_key(20, &cell.encode());
    let is = changeset_key(20, &key.encode());
    cell_tables::HISTORY
        .record(&mut tx, &cell.encode(), 20, None)
        .unwrap();
    index_tables::HISTORY
        .record(&mut tx, &key.encode(), 20, None)
        .unwrap();
    assert!(CellReader::at(&tx, 10).get(&cell).unwrap().is_none());
    assert!(IndexReader::at(&tx, 10).container(&key).unwrap().is_none());
    for bad in [vec![0], vec![0x8d], vec![0x82, 0xff]] {
        tx.put(cell_tables::CELL_CHANGE_SET, &cs, &bad).unwrap();
        assert!(matches!(
            CellReader::at(&tx, 10).get(&cell),
            Err(CellError::Value(_))
        ));
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    for bad in [vec![0], vec![0x3a, 0x30, 0, 0, 0, 0, 0, 0], trailing] {
        tx.put(index_tables::INDEX_CHANGE_SET, &is, &bad).unwrap();
        assert!(matches!(
            IndexReader::at(&tx, 10).container(&key),
            Err(IndexError::Bitmap(_))
        ));
    }
    tx.delete(cell_tables::CELL_CHANGE_SET, &cs).unwrap();
    tx.delete(index_tables::INDEX_CHANGE_SET, &is).unwrap();
    assert!(matches!(
        CellReader::at(&tx, 10).with_encoded_value(&cell, |_| panic!("missing pre-image")),
        Err(CellError::History(HistoryError::MissingPreimage { .. }))
    ));
    assert!(matches!(
        IndexReader::at(&tx, 10).with_encoded_container(&key, |_| panic!("missing pre-image")),
        Err(IndexError::History(HistoryError::MissingPreimage { .. }))
    ));
    tx.put(cell_tables::CELL_HISTORY, &cell.encode(), &[0])
        .unwrap();
    tx.put(index_tables::INDEX_HISTORY, &key.encode(), &[0])
        .unwrap();
    assert!(matches!(
        CellReader::at(&tx, 10).get(&cell),
        Err(CellError::History(HistoryError::InvalidCommits { .. }))
    ));
    assert!(matches!(
        IndexReader::at(&tx, 10).container(&key),
        Err(IndexError::History(HistoryError::InvalidCommits { .. }))
    ));
    // Live empty rows are corrupt, including when selected by historical fallback.
    tx.delete(cell_tables::CELL_HISTORY, &cell.encode())
        .unwrap();
    tx.delete(index_tables::INDEX_HISTORY, &key.encode())
        .unwrap();
    tx.put(cell_tables::CELL, &cell.encode(), &[]).unwrap();
    tx.put(index_tables::INDEX, &key.encode(), &[]).unwrap();
    for target in [None, Some(10)] {
        let cells = target.map_or_else(|| CellReader::new(&tx), |t| CellReader::at(&tx, t));
        let index = target.map_or_else(|| IndexReader::new(&tx), |t| IndexReader::at(&tx, t));
        assert!(matches!(cells.get(&cell), Err(CellError::Value(_))));
        assert!(matches!(index.container(&key), Err(IndexError::Bitmap(_))));
    }
}

struct Observed<R> {
    inner: R,
    reads: RefCell<Vec<(&'static str, Table)>>,
    fail: Option<Table>,
}

impl<R: ReadTransaction> ReadTransaction for Observed<R> {
    type Cursor<'a>
        = R::Cursor<'a>
    where
        Self: 'a;

    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        self.reads.borrow_mut().push(("owned", table));
        self.inner.get(table, key)
    }

    fn with_value<T>(
        &self,
        table: Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> golemdb_storage::Result<T> {
        self.reads.borrow_mut().push(("borrowed", table));
        if self.fail == Some(table) {
            return Err(StorageError::Full);
        }
        self.inner.with_value(table, key, inspect)
    }

    fn cursor(&self, _: Table, _: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        panic!("point reads must not open a cursor")
    }
}

#[test]
fn reader_accesses_are_borrowed_and_head_paths_do_not_consult_history() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    let cell = cell_key(b"value");
    let key = index_key(ContainerId::MAX);
    let value = CellValue::from_bytes(&[7; 100_000]);
    let container = BitmapContainer::from_values(key.container_id, [1, 2]);
    tx.put(cell_tables::CELL, &cell.encode(), value.encoded_bytes())
        .unwrap();
    tx.put(
        index_tables::INDEX,
        &key.encode(),
        &container.encode().unwrap(),
    )
    .unwrap();
    cell_tables::HISTORY
        .record(&mut tx, &cell.encode(), 20, Some(value.encoded_bytes()))
        .unwrap();
    index_tables::HISTORY
        .record(
            &mut tx,
            &key.encode(),
            20,
            Some(&container.encode().unwrap()),
        )
        .unwrap();
    tx.commit().unwrap();
    let mut tx = Observed {
        inner: db.begin_read().unwrap(),
        reads: RefCell::new(vec![]),
        fail: None,
    };
    assert_eq!(
        CellReader::new(&tx).get(&cell).unwrap(),
        Some(value.clone())
    );
    assert_eq!(tx.reads.take(), [("owned", cell_tables::CELL)]);
    assert_eq!(
        CellReader::new(&tx)
            .with_encoded_value(&cell, |b| b.unwrap().len())
            .unwrap(),
        100_001
    );
    assert_eq!(tx.reads.take(), [("borrowed", cell_tables::CELL)]);
    assert_eq!(
        IndexReader::new(&tx).container(&key).unwrap(),
        Some(container.clone())
    );
    assert_eq!(tx.reads.take(), [("borrowed", index_tables::INDEX)]);
    assert_eq!(CellReader::at(&tx, 10).get(&cell).unwrap(), Some(value));
    assert_eq!(
        tx.reads.take(),
        [
            ("borrowed", cell_tables::CELL_HISTORY),
            ("borrowed", cell_tables::CELL_CHANGE_SET)
        ]
    );
    assert_eq!(
        IndexReader::at(&tx, 10).container(&key).unwrap(),
        Some(container)
    );
    assert_eq!(
        tx.reads.take(),
        [
            ("borrowed", index_tables::INDEX_HISTORY),
            ("borrowed", index_tables::INDEX_CHANGE_SET)
        ]
    );
    CellReader::at(&tx, 20)
        .with_encoded_value(&cell, |_| ())
        .unwrap();
    assert_eq!(
        tx.reads.take(),
        [
            ("borrowed", cell_tables::CELL_HISTORY),
            ("borrowed", cell_tables::CELL)
        ]
    );
    IndexReader::at(&tx, 20)
        .with_encoded_container(&key, |_| ())
        .unwrap();
    assert_eq!(
        tx.reads.take(),
        [
            ("borrowed", index_tables::INDEX_HISTORY),
            ("borrowed", index_tables::INDEX)
        ]
    );
    for (table, target) in [
        (cell_tables::CELL_HISTORY, 10),
        (cell_tables::CELL_CHANGE_SET, 10),
        (cell_tables::CELL, 20),
    ] {
        tx.fail = Some(table);
        assert!(
            CellReader::at(&tx, target)
                .with_encoded_value(&cell, |_| panic!("failed read"))
                .is_err()
        );
    }
    for (table, target) in [
        (index_tables::INDEX_HISTORY, 10),
        (index_tables::INDEX_CHANGE_SET, 10),
        (index_tables::INDEX, 20),
    ] {
        tx.fail = Some(table);
        assert!(
            IndexReader::at(&tx, target)
                .with_encoded_container(&key, |_| panic!("failed read"))
                .is_err()
        );
    }
}

#[test]
fn recorders_skip_noops_reject_duplicate_commits_and_preserve_absence_encoding() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    let cell = CellValueChange {
        key: cell_key(b"x"),
        before: Some(CellValue::from_u64(1)),
        after: Some(CellValue::from_u64(2)),
    };
    let key = index_key(ContainerId::new(0).unwrap());
    let encoded = BitmapContainer::from_values(key.container_id, [1])
        .encode()
        .unwrap();
    let container = ContainerValueChange {
        key,
        before: Some(encoded.clone()),
        after: None,
    };
    cell_history::record_changes(
        &mut tx,
        20,
        &[CellValueChange {
            after: cell.before.clone(),
            ..cell.clone()
        }],
    )
    .unwrap();
    index_history::record_changes(
        &mut tx,
        20,
        &[ContainerValueChange {
            after: container.before.clone(),
            ..container.clone()
        }],
    )
    .unwrap();
    assert!(tx.is_empty().unwrap());
    let bad = ContainerValueChange {
        after: Some(vec![]),
        ..container.clone()
    };
    assert!(matches!(
        index_history::record_changes(&mut tx, 20, &[bad]),
        Err(IndexError::Bitmap(BitmapError::EmptyContainer))
    ));
    assert!(tx.is_empty().unwrap());
    cell_history::record_changes(&mut tx, 20, std::slice::from_ref(&cell)).unwrap();
    index_history::record_changes(&mut tx, 20, std::slice::from_ref(&container)).unwrap();
    assert!(matches!(
        cell_history::record_changes(&mut tx, 20, std::slice::from_ref(&cell)),
        Err(CellError::History(HistoryError::NonIncreasingCommit { .. }))
    ));
    assert!(matches!(
        index_history::record_changes(&mut tx, 20, std::slice::from_ref(&container)),
        Err(IndexError::History(
            HistoryError::NonIncreasingCommit { .. }
        ))
    ));
    assert_eq!(
        tx.get(
            cell_tables::CELL_CHANGE_SET,
            &changeset_key(20, &cell.key.encode())
        )
        .unwrap(),
        cell.before.map(CellValue::into_bytes)
    );
    assert_eq!(
        tx.get(
            index_tables::INDEX_CHANGE_SET,
            &changeset_key(20, &container.key.encode())
        )
        .unwrap(),
        Some(encoded)
    );
    tx.abort();
    assert!(db.begin_write().unwrap().is_empty().unwrap());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn adapters_match_snapshots_and_history_does_not_change_roots(
        operations in prop::collection::vec((0usize..3, prop::option::of(any::<u8>()), 0u16..5, any::<bool>()), 1..40)
    ) {
        let db = MemoryStore::new();
        let control = MemoryStore::new();
        let cells = Cells::new(&HASH);
        let index = Index::new(&HASH);
        let ids = [ContainerId::new(0).unwrap(), ContainerId::new(1).unwrap(), ContainerId::MAX];
        let mut cell_state = vec![None; 3];
        let mut index_state = vec![BTreeSet::new(); 3];
        let mut snapshots = vec![(cell_state.clone(), index_state.clone())];
        let mut cell_root = RootRef::Empty;
        let mut index_root = RootRef::Empty;
        for (step, &(slot, value, offset, present)) in operations.iter().enumerate() {
            let commit = (step as u64 + 1) * 2;
            let value = value.map(|v| if v % 2 == 0 {
                CellValue::from_u64(u64::from(v)).with_kind(CellKind::Attribute).unwrap()
            } else { CellValue::from_bytes(&[v]) });
            let mut tx = db.begin_write().unwrap();
            let mut reference = control.begin_write().unwrap();
            let change = put(&cell_key(&[slot as u8]), value.clone());
            let cu = cells.apply(&mut tx, commit, cell_root, [change.clone()]).unwrap();
            let cr = cells.apply(&mut reference, commit, cell_root, [change]).unwrap();
            prop_assert_eq!(&cu, &cr);
            let key = index_key(ids[slot]);
            let change = posting(&key, offset, present);
            let iu = index.apply(&mut tx, commit, index_root, [change.clone()]).unwrap();
            let ir = index.apply(&mut reference, commit, index_root, [change]).unwrap();
            prop_assert_eq!(&iu, &ir);
            let changed = if present { index_state[slot].insert(u32::from(offset)) } else { index_state[slot].remove(&u32::from(offset)) };
            prop_assert_eq!(iu.changed_containers.len(), usize::from(changed));
            prop_assert_eq!(cu.changed_cells.len(), usize::from(cell_state[slot] != value));
            cell_history::record_changes(&mut tx, commit, &cu.changed_cells).unwrap();
            index_history::record_changes(&mut tx, commit, &iu.changed_containers).unwrap();
            cell_root = cu.root;
            index_root = iu.root;
            tx.commit().unwrap();
            reference.commit().unwrap();
            cell_state[slot] = value;
            snapshots.push((cell_state.clone(), index_state.clone()));
        }
        let tx = db.begin_read().unwrap();
        for target in 0..=operations.len() as u64 * 2 {
            let (expected_cells, expected_index) = &snapshots[target as usize / 2];
            for slot in 0..3 {
                prop_assert_eq!(CellReader::at(&tx, target).get(&cell_key(&[slot as u8])).unwrap(), expected_cells[slot].clone());
                prop_assert_eq!(offsets(&IndexReader::at(&tx, target), &index_key(ids[slot])), expected_index[slot].iter().copied().collect::<Vec<_>>());
            }
        }
    }
}
