//! Public immutable-storage contracts across memory and file backends.
use golemdb_cells::{CellType, CellValue, Width};
use golemdb_immutable_data::{metadata, *};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, Table, WriteTransaction};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn config(compression: ImmutableDataCompression) -> ImmutableDataConfig {
    ImmutableDataConfig::new(
        4,
        vec![ImmutableDataSegment {
            name: "events_v1".into(),
            columns: vec![
                ImmutableDataColumn::new("number").with_type(CellType::Uint(Width::W8)),
                ImmutableDataColumn::new("body"),
            ],
            compression,
        }],
    )
}
fn row(n: u64) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![
        CellValue::from_u64(n),
        CellValue::from_bytes(if n == 0 { b"" } else { b"data" }),
    ])
}
fn path(dir: &Path, first: u64) -> PathBuf {
    dir.join(format!("segment-events_v1-{first}-{}.jar", first + 4))
}
fn number(storage: &ImmutableDataStorage, ordinal: u64) -> u64 {
    storage
        .get(
            "events_v1",
            ordinal,
            &ImmutableDataReadOptions::columns(["number"]),
        )
        .unwrap()
        .get("number")
        .unwrap()
}
fn for_backends(mut run: impl FnMut(ImmutableDataStorage)) {
    run(ImmutableDataStorage::in_memory(config(ImmutableDataCompression::None), [7; 32]).unwrap());
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        let dir = tempfile::tempdir().unwrap();
        run(ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap());
    }
}

#[test]
fn unsynced_append_does_not_corrupt_synced_rows_after_reopen() {
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        // Exercise recovery by a read, by an append, and before file rotation.
        for (read_first, next_commit) in [(true, 0), (false, 0), (false, 4)] {
            let dir = tempfile::tempdir().unwrap();
            let open =
                || ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
            {
                let mut storage = open();
                storage.append("events_v1", 0, &row(1)).unwrap();
                storage.sync().unwrap();
                storage.append("events_v1", 0, &row(2)).unwrap();
                // Drop can flush payload bytes without committing offsets/config.
            }
            let options = ImmutableDataReadOptions::all();
            {
                let mut storage = open();
                assert_eq!(storage.end_ordinal("events_v1").unwrap(), 1);
                assert!(matches!(
                    storage.get("events_v1", 1, &options),
                    Err(ImmutableDataStorageError::NotFound)
                ));
                if read_first {
                    let recovered = storage.get("events_v1", 0, &options).unwrap();
                    // Include the final bytes column: NippyJar reads it up to EOF.
                    assert_eq!(recovered.values(), row(1).values());
                }
                assert_eq!(
                    storage.append("events_v1", next_commit, &row(3)).unwrap(),
                    1
                );
                storage.sync().unwrap();
            }
            let storage = open();
            assert_eq!(storage.end_ordinal("events_v1").unwrap(), 2);
            for (ordinal, expected) in [(0, row(1)), (1, row(3))] {
                let recovered = storage.get("events_v1", ordinal, &options).unwrap();
                assert_eq!(recovered.values(), expected.values());
            }
        }
    }
}

#[test]
fn unsynced_tail_allows_commit_range_reads_and_truncation_after_reopen() {
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        for read_first in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let open =
                || ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
            let store = MemoryStore::new();
            {
                let mut storage = open();
                storage.append("events_v1", 0, &row(1)).unwrap();
                storage.append("events_v1", 0, &row(2)).unwrap();
                storage.sync().unwrap();
                let mut tx = store.begin_write().unwrap();
                storage
                    .record_commit_range(&mut tx, "events_v1", 0, 0..2)
                    .unwrap();
                tx.commit().unwrap();
                storage.append("events_v1", 0, &row(3)).unwrap();
            }
            let options = ImmutableDataReadOptions::all();
            {
                let mut storage = open();
                if read_first {
                    let tx = store.begin_read().unwrap();
                    let recovered = storage.rows_of(&tx, "events_v1", 0, &options).unwrap();
                    assert_eq!(recovered.len(), 2);
                    for (actual, expected) in recovered.iter().zip([row(1), row(2)]) {
                        assert_eq!(actual.values(), expected.values());
                    }
                }
                // Remove metadata before truncating rows it refers to.
                let mut tx = store.begin_write().unwrap();
                tx.delete(metadata::COMMIT_RANGES, &[0; 12]).unwrap(); // segment 0, commit 0
                tx.commit().unwrap();
                storage.truncate("events_v1", 1).unwrap();
                assert_eq!(storage.append("events_v1", 0, &row(4)).unwrap(), 1);
                storage.sync().unwrap();
            }
            let storage = open();
            assert_eq!(storage.end_ordinal("events_v1").unwrap(), 2);
            for (ordinal, expected) in [(0, row(1)), (1, row(4))] {
                assert_eq!(
                    storage
                        .get("events_v1", ordinal, &options)
                        .unwrap()
                        .values(),
                    expected.values()
                );
            }
        }
    }
}

#[test]
fn append_reads_keys_ranges_and_empty_commits_are_independent_of_head() {
    for_backends(|mut storage| {
        let store = MemoryStore::new();
        let options = ImmutableDataReadOptions::columns(["body", "number"]);
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 0);
        // Many rows in one commit; first append need not be commit zero.
        for n in 0..3 {
            assert_eq!(storage.append("events_v1", 5, &row(n)).unwrap(), n);
        }
        let mut tx = store.begin_write().unwrap();
        storage
            .bind_key(&mut tx, "events_v1", ImmutableDataKey([1; 32]), 1)
            .unwrap();
        storage
            .record_commit_range(&mut tx, "events_v1", 5, 0..3)
            .unwrap();
        assert!(tx.get(Table("Superblock"), b"head").unwrap().is_none());
        storage.sync().unwrap();
        tx.commit().unwrap();
        // No calls for empty commits or empty file spans.
        assert_eq!(storage.append("events_v1", 14, &row(3)).unwrap(), 3);
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 4);
        let tx = store.begin_read().unwrap();
        let selected = storage
            .get_by_key(&tx, "events_v1", ImmutableDataKey([1; 32]), &options)
            .unwrap();
        assert_eq!(selected.get::<u64>("number").unwrap(), 1);
        assert_eq!(
            selected
                .iter()
                .unwrap()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["body", "number"]
        );
        assert_eq!(
            metadata::key_for_ordinal(&tx, 0, 1).unwrap(),
            Some(ImmutableDataKey([1; 32]))
        );
        let rows = storage.rows_of(&tx, "events_v1", 5, &options).unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.get::<u64>("number").unwrap())
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(
            storage
                .rows_of(&tx, "events_v1", 6, &options)
                .unwrap()
                .is_empty()
        );
        for n in 0..4 {
            assert_eq!(number(&storage, n), n);
        }
        assert!(matches!(
            storage.get("events_v1", 4, &options),
            Err(ImmutableDataStorageError::NotFound)
        ));
        assert!(matches!(
            storage.get_by_key(&tx, "events_v1", ImmutableDataKey([2; 32]), &options),
            Err(ImmutableDataStorageError::NotFound)
        ));
        storage.sync().unwrap();
    });
}

#[test]
fn physical_tail_truncation_crosses_files_and_reuses_ordinals() {
    for_backends(|mut storage| {
        for (commit, n) in [(1, 0), (1, 1), (5, 2), (5, 3), (13, 4), (13, 5)] {
            storage.append("events_v1", commit, &row(n)).unwrap();
        }
        assert!(storage.truncate("events_v1", 7).is_err());
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 6);
        storage.truncate("events_v1", 6).unwrap();
        storage.truncate("events_v1", 3).unwrap();
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 3);
        assert_eq!(number(&storage, 2), 2);
        assert_eq!(storage.append("events_v1", 5, &row(30)).unwrap(), 3);
        storage.truncate("events_v1", 2).unwrap(); // exact file boundary
        assert_eq!(storage.append("events_v1", 1, &row(20)).unwrap(), 2);
        assert_eq!(number(&storage, 2), 20);
        storage.truncate("events_v1", 0).unwrap();
        storage.truncate("events_v1", 0).unwrap();
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 0);
        assert_eq!(storage.append("events_v1", 0, &row(99)).unwrap(), 0);
        storage.sync().unwrap();
        assert_eq!(number(&storage, 0), 99);
    });
}

#[test]
fn independent_segments_and_key_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(ImmutableDataCompression::None);
    let mut another = cfg.segments[0].clone();
    another.name = "other".into();
    cfg.segments.insert(0, another);
    let mut storage = ImmutableDataStorage::open(cfg, [7; 32], dir.path()).unwrap();
    assert_eq!(storage.append("events_v1", 100, &row(1)).unwrap(), 0);
    assert_eq!(storage.append("other", 1, &row(2)).unwrap(), 0);
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    let key = ImmutableDataKey([3; 32]);
    for name in ["other", "events_v1"] {
        storage.bind_key(&mut tx, name, key, 0).unwrap();
    }
    assert!(matches!(
        storage.bind_key(&mut tx, "events_v1", key, 0),
        Err(ImmutableDataStorageError::AlreadyExists)
    ));
    assert!(matches!(
        storage.bind_key(&mut tx, "events_v1", ImmutableDataKey([4; 32]), 0),
        Err(ImmutableDataStorageError::AlreadyExists)
    ));
    assert_eq!(
        storage
            .get_by_key(&tx, "other", key, &ImmutableDataReadOptions::all())
            .unwrap()
            .get::<u64>("number")
            .unwrap(),
        2
    );
    storage.truncate("events_v1", 0).unwrap();
    assert_eq!(storage.end_ordinal("other").unwrap(), 1);
    assert!(matches!(
        storage.get_by_key(&tx, "events_v1", key, &ImmutableDataReadOptions::all()),
        Err(ImmutableDataStorageError::Corrupt(_))
    ));
    storage.sync().unwrap();
}

#[test]
fn invalid_rows_ranges_and_old_spans_do_not_change_end() {
    for_backends(|mut storage| {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        for bad in [
            ImmutableDataRow::new(vec![]),
            ImmutableDataRow::new(vec![
                CellValue::from_bytes(b"wrong"),
                CellValue::from_bytes(b""),
            ]),
        ] {
            assert!(storage.append("events_v1", 1, &bad).is_err());
            assert_eq!(storage.end_ordinal("events_v1").unwrap(), 0);
        }
        assert!(storage.append("events_v1", u64::MAX, &row(0)).is_err());
        storage.append("events_v1", 5, &row(0)).unwrap();
        assert!(storage.append("events_v1", 3, &row(1)).is_err());
        assert!(
            storage
                .bind_key(&mut tx, "events_v1", ImmutableDataKey([1; 32]), 1)
                .is_err()
        );
        for (commit, range) in [(5, 0..0), (5, 0..2), (1, 0..1)] {
            assert!(
                storage
                    .record_commit_range(&mut tx, "events_v1", commit, range)
                    .is_err()
            );
        }
        storage
            .record_commit_range(&mut tx, "events_v1", 5, 0..1)
            .unwrap();
        assert!(matches!(
            storage.record_commit_range(&mut tx, "events_v1", 5, 0..1),
            Err(ImmutableDataStorageError::AlreadyExists)
        ));
        assert!(
            storage
                .get(
                    "events_v1",
                    0,
                    &ImmutableDataReadOptions::columns(["unknown"])
                )
                .is_err()
        );
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 1);
        storage.sync().unwrap();
    });
}

#[test]
fn missing_rows_in_commit_metadata_are_corruption() {
    for_backends(|mut storage| {
        storage.append("events_v1", 1, &row(1)).unwrap();
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        metadata::record_commit_range(&mut tx, 0, 1, 0..2).unwrap();
        metadata::record_commit_range(&mut tx, 0, 100, 0..1).unwrap();
        for commit in [1, 100] {
            assert!(matches!(
                storage.rows_of(&tx, "events_v1", commit, &ImmutableDataReadOptions::all()),
                Err(ImmutableDataStorageError::Corrupt(_))
            ));
        }
        storage.sync().unwrap();
    });
}

#[test]
fn case_colliding_segment_names_are_rejected_before_file_access() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(ImmutableDataCompression::None);
    {
        let mut storage = ImmutableDataStorage::open(cfg.clone(), [7; 32], dir.path()).unwrap();
        storage.append("events_v1", 1, &row(42)).unwrap();
        storage.sync().unwrap();
    }
    let data = path(dir.path(), 0);
    let paths = [
        data.clone(),
        data.with_extension("conf"),
        data.with_extension("off"),
    ];
    let before: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
    let mut invalid = cfg.clone();
    let mut collision = invalid.segments[0].clone();
    collision.name = "Events_v1".into();
    invalid.segments.push(collision);
    let fresh = dir.path().join("fresh");
    for target in [fresh.as_path(), dir.path()] {
        assert!(matches!(
            ImmutableDataStorage::open(invalid.clone(), [7; 32], target),
            Err(ImmutableDataStorageError::Configuration(
                ImmutableDataConfigError::Schema(ImmutableDataError::InvalidSegmentName { .. })
            ))
        ));
    }
    assert!(!fresh.exists());
    let after: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
    assert_eq!(before, after);
    let storage = ImmutableDataStorage::open(cfg, [7; 32], dir.path()).unwrap();
    assert_eq!(number(&storage, 0), 42);
}

#[test]
fn empty_catalogue_has_no_system_file_and_directory_owner_is_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ImmutableDataConfig::new(4, vec![]);
    let mut storage = ImmutableDataStorage::open(cfg.clone(), [7; 32], dir.path()).unwrap();
    assert!(ImmutableDataStorage::open(cfg, [7; 32], dir.path()).is_err());
    storage.sync().unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1); // directory lock only
    assert!(storage.end_ordinal("unknown").is_err());
}
