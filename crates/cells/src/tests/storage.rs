use crate::CELL_BRANCH_DOMAIN;
use std::collections::BTreeMap;

use crate::{
    CELL_TRIE_PATH_BYTES, CellChange, CellError, CellKey, CellNameRef, CellType, CellValue,
    CellValueChange, CellValueParseError, CellValueRef, Cells, tables,
};
use golemdb_merkle::{HashProvider, Keccak256Hasher, LeafRef, RootRef, Trie};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction, scan_prefix};
use proptest::prelude::*;

const HASH: Keccak256Hasher = Keccak256Hasher;
const HEAD: Table = Table("TestHead");

fn key(record: u64, name: &[u8]) -> CellKey {
    CellKey::new(record, CellNameRef::raw(name))
}
fn value(bytes: &[u8]) -> CellValue {
    CellValueRef::new(CellType::Str, bytes, false)
        .unwrap()
        .into()
}
fn put(record: u64, name: &[u8], bytes: &[u8]) -> CellChange {
    CellChange::Put {
        key: key(record, name),
        value: value(bytes),
    }
}
fn delete(record: u64, name: &[u8]) -> CellChange {
    CellChange::Delete {
        key: key(record, name),
    }
}
fn rows(tx: &impl ReadTransaction, table: Table) -> Vec<golemdb_storage::Entry> {
    scan_prefix(tx, table, vec![])
        .unwrap()
        .collect::<golemdb_storage::Result<_>>()
        .unwrap()
}
fn leaf(key: &CellKey, value: &CellValue) -> LeafRef<CELL_TRIE_PATH_BYTES> {
    // Independently spell out the spec's key and leaf preimages.
    let mut routing = key.record_id().to_be_bytes().to_vec();
    routing.extend_from_slice(key.name().as_bytes());
    let path = HASH.hash(&routing);
    let mut preimage = vec![0];
    preimage.extend_from_slice(&path);
    preimage.push(value.metadata());
    preimage.extend_from_slice(value.value());
    LeafRef::new(path, HASH.hash(&preimage))
}

fn transitions_and_tagged_commitments(db: &impl Store) {
    let cells = Cells::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    assert_eq!(cells.get(&tx, &key(42, b"status")).unwrap(), None);
    cells.validate_root(&tx, RootRef::Empty).unwrap();
    let first = cells
        .apply(&mut tx, 0, RootRef::Empty, [put(42, b"status", b"ready")])
        .unwrap();
    assert_eq!(
        first.root,
        RootRef::Leaf(leaf(&key(42, b"status"), &value(b"ready")))
    );
    assert_eq!(
        rows(&tx, tables::CELL),
        vec![(key(42, b"status").encode(), b"\x02ready".to_vec())]
    );
    assert!(rows(&tx, tables::CELL_TRIE.table()).is_empty());
    cells.validate_root(&tx, first.root).unwrap();
    // The same payload under a different kind/type has a different commitment.
    let attr: CellValue = CellValueRef::new(CellType::Str, b"ready", true)
        .unwrap()
        .into();
    let changed = cells
        .apply(
            &mut tx,
            0,
            first.root,
            [CellChange::Put {
                key: key(42, b"status"),
                value: attr.clone(),
            }],
        )
        .unwrap();
    assert_ne!(changed.root, first.root);
    assert_eq!(
        changed.root,
        RootRef::Leaf(leaf(&key(42, b"status"), &attr))
    );
    let bytes: CellValue = CellValueRef::new(CellType::Bytes, b"ready", false)
        .unwrap()
        .into();
    let retyped = cells
        .apply(
            &mut tx,
            0,
            changed.root,
            [CellChange::Put {
                key: key(42, b"status"),
                value: bytes.clone(),
            }],
        )
        .unwrap();
    assert_ne!(retyped.root, first.root);
    assert_eq!(
        retyped.root,
        RootRef::Leaf(leaf(&key(42, b"status"), &bytes))
    );
    let branch = cells
        .apply(&mut tx, 0, retyped.root, [put(43, b"status", b"ready")])
        .unwrap()
        .root;
    assert!(matches!(branch, RootRef::Branch(_)));
    cells.validate_root(&tx, branch).unwrap();
    let collapsed = cells
        .apply(&mut tx, 0, branch, [delete(43, b"status")])
        .unwrap()
        .root;
    assert_eq!(collapsed, retyped.root);
    // The collapsed branch's row was written by this same commit, so it is
    // deleted; a singleton is recovered from the flat table.
    cells.validate_root(&tx, collapsed).unwrap();
    let empty = cells
        .apply(&mut tx, 0, collapsed, [delete(42, b"status")])
        .unwrap()
        .root;
    assert_eq!(empty, RootRef::Empty);
    assert!(rows(&tx, tables::CELL).is_empty());
    // Every write here is commit 0, so no superseded branch row remains.
    assert!(rows(&tx, tables::CELL_TRIE.table()).is_empty());
    cells.validate_root(&tx, empty).unwrap();
    tx.commit().unwrap();
}

fn record_scans_preserve_raw_names_and_own_results(db: &impl Store) {
    let cells = Cells::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let names = [
        b"".as_slice(),
        b"\0\xff",
        b"#key",
        b"$owner",
        b"@weight",
        b"Price",
        b"price",
        b"\xff",
    ];
    let records = [0, 42, 43, u64::MAX];
    let changes = records.into_iter().flat_map(|id| {
        names
            .into_iter()
            .rev()
            .map(move |name| put(id, name, b"data"))
    });
    cells.apply(&mut tx, 0, RootRef::Empty, changes).unwrap();
    tx.commit().unwrap();
    let result = {
        let tx = db.begin_read().unwrap();
        for id in records {
            let got = cells
                .scan_record(&tx, id)
                .unwrap()
                .collect::<crate::Result<Vec<_>>>()
                .unwrap();
            let expected: Vec<_> = names
                .iter()
                .map(|name| (key(id, name), value(b"data")))
                .collect();
            assert_eq!(got, expected);
        }
        let mut absent = cells.scan_record(&tx, 41).unwrap();
        assert!(absent.next().is_none());
        assert!(absent.next().is_none());
        cells.get(&tx, &key(42, b"Price")).unwrap().unwrap()
    };
    assert_eq!(result.as_str(), Some("data"));
}

fn batches_report_original_and_final_values_and_skip_noops(db: &impl Store) {
    let cells = Cells::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let first = cells
        .apply(
            &mut tx,
            0,
            RootRef::Empty,
            [put(42, b"a", b"old"), put(42, b"b", b"same")],
        )
        .unwrap();
    let branch_rows = rows(&tx, tables::CELL_TRIE.table());
    let noops = cells
        .apply(
            &mut tx,
            0,
            first.root,
            [
                put(42, b"a", b"intermediate"),
                put(42, b"a", b"old"),
                put(42, b"b", b"same"),
                put(99, b"temporary", b"x"),
                delete(99, b"temporary"),
                delete(42, b"absent"),
            ],
        )
        .unwrap();
    assert_eq!(noops.root, first.root);
    assert!(noops.changed_cells.is_empty());
    assert_eq!(rows(&tx, tables::CELL_TRIE.table()), branch_rows);
    let changed = cells
        .apply(
            &mut tx,
            0,
            first.root,
            [
                put(43, b"a", b"inserted"),
                put(42, b"a", b"middle"),
                delete(42, b"b"),
                delete(42, b"a"),
                put(42, b"a", b"final"),
            ],
        )
        .unwrap();
    assert_eq!(
        changed.changed_cells,
        vec![
            CellValueChange {
                key: key(42, b"a"),
                before: Some(value(b"old")),
                after: Some(value(b"final"))
            },
            CellValueChange {
                key: key(42, b"b"),
                before: Some(value(b"same")),
                after: None
            },
            CellValueChange {
                key: key(43, b"a"),
                before: None,
                after: Some(value(b"inserted"))
            },
        ]
    );
    assert!(cells.get(&tx, &key(42, b"b")).unwrap().is_none());
    assert_eq!(
        cells.get(&tx, &key(42, b"a")).unwrap(),
        Some(value(b"final"))
    );
    let empty_batch = cells.apply(&mut tx, 0, changed.root, []).unwrap();
    assert_eq!(empty_batch.root, changed.root);
    assert!(empty_batch.changed_cells.is_empty());
}

fn snapshots_abort_and_old_branches(db: &impl Store) {
    let cells = Cells::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    let root = cells
        .apply(
            &mut tx,
            0,
            RootRef::Empty,
            [put(42, b"a", b"one"), put(42, b"b", b"two")],
        )
        .unwrap()
        .root;
    tx.put(HEAD, b"root", &root.hash(&HASH)).unwrap();
    tx.commit().unwrap();
    let snapshot = db.begin_read().unwrap();
    let old_branches = rows(&snapshot, tables::CELL_TRIE.table());
    // Later batches are later commits; earlier commits' rows are never rewritten.
    let mut tx = db.begin_write().unwrap();
    let next = cells
        .apply(
            &mut tx,
            1,
            root,
            [put(42, b"a", b"three"), put(43, b"new", b"four")],
        )
        .unwrap()
        .root;
    tx.put(HEAD, b"root", &next.hash(&HASH)).unwrap();
    assert_eq!(
        cells.get(&snapshot, &key(42, b"a")).unwrap(),
        Some(value(b"one"))
    );
    tx.abort();
    {
        let mut tx = db.begin_write().unwrap();
        cells
            .apply(
                &mut tx,
                1,
                root,
                [put(42, b"a", b"discarded"), put(44, b"x", b"y")],
            )
            .unwrap();
        // Drop is also an abort.
    }
    let read = db.begin_read().unwrap();
    assert_eq!(rows(&read, tables::CELL_TRIE.table()), old_branches);
    assert_eq!(
        read.get(HEAD, b"root").unwrap(),
        Some(root.hash(&HASH).to_vec())
    );
    cells.validate_root(&read, root).unwrap();
    let mut tx = db.begin_write().unwrap();
    let next = cells
        .apply(&mut tx, 1, root, [put(42, b"a", b"three")])
        .unwrap()
        .root;
    tx.put(HEAD, b"root", &next.hash(&HASH)).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        cells.get(&snapshot, &key(42, b"a")).unwrap(),
        Some(value(b"one"))
    );
    let latest = db.begin_read().unwrap();
    assert_eq!(
        cells.get(&latest, &key(42, b"a")).unwrap(),
        Some(value(b"three"))
    );
    for (key, bytes) in old_branches {
        assert_eq!(
            latest.get(tables::CELL_TRIE.table(), &key).unwrap(),
            Some(bytes)
        );
    }
    let trie = Trie::<_, CELL_TRIE_PATH_BYTES>::new(tables::CELL_TRIE, CELL_BRANCH_DOMAIN, &HASH);
    let mut expected = vec![
        leaf(&key(42, b"a"), &value(b"one")),
        leaf(&key(42, b"b"), &value(b"two")),
    ];
    expected.sort_by_key(|leaf| leaf.path);
    assert_eq!(
        trie.walk(&latest, root)
            .collect::<golemdb_merkle::Result<Vec<_>>>()
            .unwrap(),
        expected
    );
}

fn root_mismatches_and_corrupt_rows_are_errors(db: &impl Store) {
    let cells = Cells::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    assert!(matches!(
        cells.validate_root(&tx, RootRef::Leaf(LeafRef::new([7; 32], [7; 32]))),
        Err(CellError::RootMismatch)
    ));
    let first = cells
        .apply(&mut tx, 0, RootRef::Empty, [put(42, b"a", b"one")])
        .unwrap()
        .root;
    assert!(matches!(
        cells.validate_root(&tx, RootRef::Empty),
        Err(CellError::RootMismatch)
    ));
    assert!(matches!(
        cells.validate_root(&tx, RootRef::Leaf(LeafRef::new([7; 32], [7; 32]))),
        Err(CellError::RootMismatch)
    ));
    // No-op writes must still detect a mismatched before-value commitment.
    tx.put(
        tables::CELL,
        &key(42, b"a").encode(),
        value(b"other").encoded_bytes(),
    )
    .unwrap();
    assert!(matches!(
        cells.apply(&mut tx, 0, first, [put(42, b"a", b"other")]),
        Err(CellError::RootMismatch)
    ));
    tx.abort();
    let mut tx = db.begin_write().unwrap();
    let root = cells
        .apply(
            &mut tx,
            0,
            RootRef::Empty,
            [put(42, b"a", b"one"), put(42, b"b", b"two")],
        )
        .unwrap()
        .root;
    tx.commit().unwrap();
    let mut tx = db.begin_write().unwrap();
    // Never treat a missing branch row as a singleton when multiple cells exist.
    tx.delete(tables::CELL_TRIE.table(), &root_key(&root))
        .unwrap();
    assert!(matches!(
        cells.validate_root(&tx, root),
        Err(CellError::Merkle(
            golemdb_merkle::MerkleError::MissingBranch(_)
        ))
    ));
    tx.abort();
    let mut tx = db.begin_write().unwrap();
    tx.put(tables::CELL_TRIE.table(), &root_key(&root), b"broken")
        .unwrap();
    assert!(matches!(
        cells.validate_root(&tx, root),
        Err(CellError::Merkle(_))
    ));
    tx.abort();
    let mut tx = db.begin_write().unwrap();
    tx.put(tables::CELL, &key(42, b"a").encode(), &[0]).unwrap();
    assert!(matches!(
        cells.get(&tx, &key(42, b"a")),
        Err(CellError::Value(CellValueParseError::AbsentTag))
    ));
    assert!(matches!(
        cells.apply(&mut tx, 0, root, [delete(42, b"a")]),
        Err(CellError::Value(_))
    ));
    let mut scan = cells.scan_record(&tx, 42).unwrap();
    assert!(matches!(scan.next(), Some(Err(CellError::Value(_)))));
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
    drop(scan);
    tx.abort();
}

fn malformed_singleton_key_is_rejected(db: &impl Store) {
    let cells = Cells::new(&HASH);
    let mut tx = db.begin_write().unwrap();
    tx.put(tables::CELL, b"short", b"\x02value").unwrap();
    assert!(matches!(
        cells.validate_root(&tx, RootRef::Leaf(LeafRef::new([7; 32], [7; 32]))),
        Err(CellError::Key(_))
    ));
}

/// Row key of a branch root: its commit and the empty route.
fn root_key(root: &RootRef<CELL_TRIE_PATH_BYTES>) -> Vec<u8> {
    let RootRef::Branch(branch) = root else {
        panic!("not a branch root")
    };
    Trie::<_, CELL_TRIE_PATH_BYTES>::new(tables::CELL_TRIE, CELL_BRANCH_DOMAIN, &HASH)
        .node_key(branch.commit, &[])
}

fn memory_db() -> ((), MemoryStore) {
    ((), MemoryStore::new())
}

macro_rules! suite {
    ($module:ident, $setup:ident, $($case:ident),+ $(,)?) => {
        mod $module {
            $(#[test] fn $case() {
                let (_dir, db) = super::$setup();
                super::$case(&db);
            })+
        }
    };
}
suite!(
    memory,
    memory_db,
    transitions_and_tagged_commitments,
    record_scans_preserve_raw_names_and_own_results,
    batches_report_original_and_final_values_and_skip_noops,
    snapshots_abort_and_old_branches,
    root_mismatches_and_corrupt_rows_are_errors,
    malformed_singleton_key_is_rejected,
);

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn batches_match_a_flat_model_and_a_fresh_commitment(
        operations in prop::collection::vec((0u8..12, any::<u8>(), any::<bool>()), 0..80)
    ) {
        let db = MemoryStore::new();
        let cells = Cells::new(&HASH);
        let mut tx = db.begin_write().unwrap();
        let mut root = RootRef::Empty;
        let mut model = BTreeMap::new();
        for batch in operations.chunks(8) {
            let before = model.clone();
            let changes: Vec<_> = batch.iter().map(|&(id, byte, remove)| {
                let key = key(u64::from(id), b"value");
                if remove {
                    model.remove(&key);
                    CellChange::Delete { key }
                } else {
                    let value: CellValue = CellValueRef::new(CellType::Bytes, &[byte], false).unwrap().into();
                    model.insert(key.clone(), value.clone());
                    CellChange::Put { key, value }
                }
            }).collect();
            let update = cells.apply(&mut tx, 0, root, changes).unwrap();
            root = update.root;
            let mut expected_changes = BTreeMap::new();
            for key in before.keys().chain(model.keys()) {
                if before.get(key) != model.get(key) {
                    expected_changes.insert(key.clone(), CellValueChange { key: key.clone(), before: before.get(key).cloned(), after: model.get(key).cloned() });
                }
            }
            prop_assert_eq!(update.changed_cells, expected_changes.into_values().collect::<Vec<_>>());
            let got: BTreeMap<_, _> = rows(&tx, tables::CELL).into_iter().map(|(k,v)| (CellKey::decode(&k).unwrap(), CellValue::parse(v).unwrap())).collect();
            prop_assert_eq!(&got, &model);
            cells.validate_root(&tx, root).unwrap();
            // Rebuild in the opposite order in a separate store. Same final
            // state must have the same root despite different write history.
            let fresh = MemoryStore::new();
            let mut fresh_tx = fresh.begin_write().unwrap();
            let mut fresh_root = RootRef::Empty;
            for (key, value) in model.iter().rev() {
                fresh_root = cells.apply(&mut fresh_tx, 0, fresh_root, [CellChange::Put { key: key.clone(), value: value.clone() }]).unwrap().root;
            }
            prop_assert_eq!(root, fresh_root);
        }
    }
}
