#[path = "immutable_storage/contract.rs"]
mod contract;

use golemdb_cells::{CellType, CellValue, Width};
use golemdb_immutable_data::{metadata, *};
use golemdb_storage::{ReadTransaction, Store, Table, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;

fn config() -> ImmutableDataConfig {
    ImmutableDataConfig::new(
        3,
        vec![ImmutableDataSegment {
            name: "events".into(),
            columns: vec![
                ImmutableDataColumn::new("height").with_type(CellType::Uint(Width::W8)),
                ImmutableDataColumn::new("payload"),
            ],
            compression: ImmutableDataCompression::Zstd,
        }],
    )
}
fn row(height: u64) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![
        CellValue::from_u64(height),
        CellValue::from_bytes(b"event"),
    ])
}

#[test]
fn caller_publishes_key_and_range_metadata_and_rolls_back_files_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let kv = dir.path().join("kv");
    let files = dir.path().join("files");
    let options = ImmutableDataReadOptions::columns(["payload", "height"]);
    {
        let store = MdbxStore::open(&kv).unwrap();
        let mut storage = ImmutableDataStorage::open(config(), [9; 32], &files).unwrap();
        for height in 10..12 {
            storage.append("events", 1, &row(height)).unwrap();
        }
        storage.sync().unwrap();
        let mut tx = store.begin_write().unwrap();
        storage
            .bind_key(&mut tx, "events", ImmutableDataKey([1; 32]), 0)
            .unwrap();
        storage
            .record_commit_range(&mut tx, "events", 1, 0..2)
            .unwrap();
        tx.put(Table("CallerState"), b"published", &1u64.to_be_bytes())
            .unwrap();
        tx.commit().unwrap();
        let old = store.begin_read().unwrap();
        let original_end = storage.end_ordinal("events").unwrap();
        // A caller may append into several files before publication fails.
        storage.append("events", 2, &row(12)).unwrap();
        storage.append("events", 7, &row(13)).unwrap();
        storage.sync().unwrap();
        let mut tx = store.begin_write().unwrap();
        storage
            .bind_key(&mut tx, "events", ImmutableDataKey([2; 32]), 2)
            .unwrap();
        storage
            .record_commit_range(&mut tx, "events", 2, 2..3)
            .unwrap();
        storage
            .record_commit_range(&mut tx, "events", 7, 3..4)
            .unwrap();
        tx.put(Table("CallerState"), b"published", &7u64.to_be_bytes())
            .unwrap();
        drop(tx); // simulate publication abort
        assert_eq!(
            metadata::ordinal_for_key(&old, 0, ImmutableDataKey([2; 32])).unwrap(),
            None
        );
        assert_eq!(metadata::commit_range(&old, 0, 2).unwrap(), None);
        assert_eq!(
            old.get(Table("CallerState"), b"published").unwrap(),
            Some(1u64.to_be_bytes().to_vec())
        );
        // Physical rows remain readable until the caller rolls them back.
        assert_eq!(
            storage
                .get("events", 3, &options)
                .unwrap()
                .get::<u64>("height")
                .unwrap(),
            13
        );
        storage.truncate("events", original_end).unwrap();
        storage.sync().unwrap();
        assert!(matches!(
            storage.get("events", 2, &options),
            Err(ImmutableDataStorageError::NotFound)
        ));
        assert_eq!(
            storage.rows_of(&old, "events", 1, &options).unwrap().len(),
            2
        );
        assert!(old.get(Table("Superblock"), b"head").unwrap().is_none());
    }
    {
        let store = MdbxStore::open(&kv).unwrap();
        let mut storage = ImmutableDataStorage::open(config(), [9; 32], &files).unwrap();
        let tx = store.begin_read().unwrap();
        assert_eq!(storage.end_ordinal("events").unwrap(), 2);
        assert_eq!(
            storage
                .get_by_key(&tx, "events", ImmutableDataKey([1; 32]), &options)
                .unwrap()
                .get::<u64>("height")
                .unwrap(),
            10
        );
        assert_eq!(
            storage.rows_of(&tx, "events", 1, &options).unwrap().len(),
            2
        );
        assert!(
            storage
                .rows_of(&tx, "events", 2, &options)
                .unwrap()
                .is_empty()
        );
        drop(tx);
        assert_eq!(storage.append("events", 2, &row(22)).unwrap(), 2);
        storage.sync().unwrap();
        let mut tx = store.begin_write().unwrap();
        storage
            .bind_key(&mut tx, "events", ImmutableDataKey([2; 32]), 2)
            .unwrap();
        storage
            .record_commit_range(&mut tx, "events", 2, 2..3)
            .unwrap();
        tx.commit().unwrap();
    }
    let store = MdbxStore::open(&kv).unwrap();
    let tx = store.begin_read().unwrap();
    let storage = ImmutableDataStorage::open(config(), [9; 32], &files).unwrap();
    assert_eq!(
        storage
            .get_by_key(&tx, "events", ImmutableDataKey([2; 32]), &options)
            .unwrap()
            .get::<u64>("height")
            .unwrap(),
        22
    );
    assert_eq!(metadata::commit_range(&tx, 0, 2).unwrap(), Some(2..3));
}
