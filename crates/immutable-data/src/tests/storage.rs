//! Internal file layout, metadata encodings and synthetic NippyJar fixtures.
use crate::{files::FileHeader, metadata, *};
use golemdb_cells::{CellType, CellValue, Width};
use golemdb_storage::{MemoryStore, ReadTransaction, Store, WriteTransaction};
use reth_nippy_jar::{NippyJar, NippyJarWriter};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[path = "storage_safety.rs"]
mod safety;

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

#[test]
fn sync_reopen_rotation_and_truncation_persist() {
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut storage =
                ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
            storage.append("events_v1", 1, &row(0)).unwrap();
            storage.append("events_v1", 1, &row(1)).unwrap();
            // Appends retain a writer and do not rewrite its configuration each row.
            assert_eq!(
                NippyJar::<FileHeader>::load(&path(dir.path(), 0))
                    .unwrap()
                    .rows(),
                0
            );
            let store = MemoryStore::new();
            let mut tx = store.begin_write().unwrap();
            storage
                .bind_key(&mut tx, "events_v1", ImmutableDataKey([1; 32]), 0)
                .unwrap();
            storage
                .record_commit_range(&mut tx, "events_v1", 1, 0..2)
                .unwrap();
            assert_eq!(
                NippyJar::<FileHeader>::load(&path(dir.path(), 0))
                    .unwrap()
                    .rows(),
                0
            );
            storage.append("events_v1", 12, &row(2)).unwrap();
            assert_eq!(
                NippyJar::<FileHeader>::load(&path(dir.path(), 0))
                    .unwrap()
                    .rows(),
                2
            );
            storage.sync().unwrap();
        }
        {
            let mut storage =
                ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
            assert_eq!(storage.end_ordinal("events_v1").unwrap(), 3);
            assert_eq!(number(&storage, 1), 1);
            assert_eq!(storage.append("events_v1", 13, &row(3)).unwrap(), 3);
            storage.truncate("events_v1", 1).unwrap();
            storage.sync().unwrap();
            assert!(!path(dir.path(), 12).exists());
        }
        let mut storage =
            ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 1);
        assert_eq!(storage.append("events_v1", 2, &row(4)).unwrap(), 1);
        storage.sync().unwrap();
        assert_eq!(number(&storage, 1), 4);
    }
}

#[test]
fn configuration_discovery_is_newest_first_and_does_not_read_skipped_payloads() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut storage =
            ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
                .unwrap();
        for (commit, n) in [(1, 0), (8, 1), (12, 2)] {
            storage.append("events_v1", commit, &row(n)).unwrap();
        }
        storage.sync().unwrap();
    }
    let storage =
        ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
            .unwrap();
    // Numeric discovery must place span 12 after span 8 despite filename order.
    assert_eq!(storage.end_ordinal("events_v1").unwrap(), 3);
    assert_eq!(number(&storage, 2), 2);
    let original = fs::read(path(dir.path(), 0).with_extension("conf")).unwrap();
    fs::write(path(dir.path(), 0).with_extension("conf"), b"broken").unwrap();
    assert_eq!(number(&storage, 1), 1); // stops before the oldest damaged config
    fs::remove_file(path(dir.path(), 12)).unwrap();
    fs::remove_file(path(dir.path(), 12).with_extension("off")).unwrap();
    assert_eq!(number(&storage, 1), 1); // only configuration of newer file is needed
    fs::write(path(dir.path(), 0).with_extension("conf"), original).unwrap();
    assert_eq!(number(&storage, 0), 0);
}

#[test]
fn metadata_vectors_reverse_validation_and_invalid_ranges() {
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    let key = ImmutableDataKey([3; 32]);
    let n = u32::MAX as u64 + 42;
    metadata::bind_key(&mut tx, 7, key, n).unwrap();
    let mut forward = vec![0];
    forward.extend(7u32.to_be_bytes());
    forward.extend(key.0);
    assert_eq!(
        tx.get(metadata::KEYS, &forward).unwrap().unwrap(),
        n.to_be_bytes()
    );
    metadata::record_commit_range(&mut tx, 7, 9, n..n + 2).unwrap();
    let mut range_key = 7u32.to_be_bytes().to_vec();
    range_key.extend(9u64.to_be_bytes());
    let mut expected = n.to_be_bytes().to_vec();
    expected.extend((n + 2).to_be_bytes());
    assert_eq!(
        tx.get(metadata::COMMIT_RANGES, &range_key)
            .unwrap()
            .unwrap(),
        expected
    );
    assert_eq!(metadata::commit_range(&tx, 7, 9).unwrap(), Some(n..n + 2));
    assert_eq!(metadata::key_for_ordinal(&tx, 7, n).unwrap(), Some(key));
    tx.delete(metadata::KEYS, &forward).unwrap();
    assert!(matches!(
        metadata::key_for_ordinal(&tx, 7, n),
        Err(ImmutableDataStorageError::Corrupt(
            "reverse binding has no matching key"
        ))
    ));
    for bad in [
        vec![],
        vec![0; 15],
        vec![0; 16],
        [2u64.to_be_bytes(), 1u64.to_be_bytes()].concat(),
    ] {
        tx.put(metadata::COMMIT_RANGES, &range_key, &bad).unwrap();
        let expected = if bad.len() == 16 {
            "invalid commit range bounds"
        } else {
            "invalid commit range encoding"
        };
        assert!(matches!(metadata::commit_range(&tx, 7, 9),
            Err(ImmutableDataStorageError::Corrupt(message)) if message == expected));
    }
}

#[test]
fn interrupted_sync_discards_extra_offsets_without_changing_configured_rows() {
    // A sync may stop during the offset write or before publishing the config.
    for extra_offset_bytes in [1, 8, 16] {
        let dir = tempfile::tempdir().unwrap();
        let open = || {
            ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
                .unwrap()
        };
        {
            let mut storage = open();
            storage.append("events_v1", 0, &row(1)).unwrap();
            storage.sync().unwrap();
        }
        let data = path(dir.path(), 0);
        let paths = [
            data.clone(),
            data.with_extension("off"),
            data.with_extension("conf"),
        ];
        let before: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
        {
            let jar = NippyJar::<FileHeader>::load(&data).unwrap();
            let mut writer = NippyJarWriter::new(jar).unwrap();
            for value in row(2).payloads() {
                writer.append_column(Some(Ok(value))).unwrap();
            }
            writer.sync_all().unwrap(); // offsets/data written, old config retained
        }
        let mut offsets = fs::read(&paths[1]).unwrap();
        offsets.truncate(before[1].len() + extra_offset_bytes);
        fs::write(&paths[1], offsets).unwrap();

        let mut storage = open();
        let recovered = storage
            .get("events_v1", 0, &ImmutableDataReadOptions::all())
            .unwrap();
        assert_eq!(recovered.values(), row(1).values());
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 1);
        let after: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
        assert_eq!(before, after, "extra offset bytes: {extra_offset_bytes}");
        assert_eq!(storage.append("events_v1", 0, &row(3)).unwrap(), 1);
        storage.sync().unwrap();
    }
}

#[test]
fn truncate_rejects_boundary_in_unsynced_tail_without_changing_files() {
    let dir = tempfile::tempdir().unwrap();
    let open = || {
        ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
            .unwrap()
    };
    let data = path(dir.path(), 0);
    let synced_len;
    {
        let mut storage = open();
        storage.append("events_v1", 0, &row(1)).unwrap();
        storage.append("events_v1", 0, &row(2)).unwrap();
        storage.sync().unwrap();
        synced_len = fs::metadata(&data).unwrap().len();
        storage.append("events_v1", 0, &row(3)).unwrap();
    }
    assert!(fs::metadata(&data).unwrap().len() > synced_len);
    let paths = [
        data.clone(),
        data.with_extension("off"),
        data.with_extension("conf"),
    ];
    let mut offsets = fs::read(&paths[1]).unwrap();
    // The boundary after row 0 must not point into the unfinished third row.
    offsets[17..25].copy_from_slice(&(synced_len + 1).to_le_bytes());
    fs::write(&paths[1], offsets).unwrap();
    let before: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
    let mut storage = open();
    assert!(matches!(
        storage.truncate("events_v1", 1),
        Err(ImmutableDataStorageError::Corrupt(_))
    ));
    let after: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
    assert_eq!(before, after);
}

#[test]
fn corrupt_files_fail_without_auto_healing_or_recreation() {
    for damage in [
        "data",
        "conf",
        "off",
        "length",
        "short_data",
        "short_offsets",
        "first_offset",
        "last_offset",
        "identity",
    ] {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut storage = ImmutableDataStorage::open(
                config(ImmutableDataCompression::None),
                [7; 32],
                dir.path(),
            )
            .unwrap();
            storage.append("events_v1", 1, &row(1)).unwrap();
            storage.sync().unwrap();
        }
        let data = path(dir.path(), 0);
        let offsets = data.with_extension("off");
        match damage {
            "data" => fs::remove_file(&data).unwrap(),
            "conf" => fs::remove_file(data.with_extension("conf")).unwrap(),
            "off" => fs::remove_file(&offsets).unwrap(),
            "length" => fs::write(&offsets, []).unwrap(),
            "short_data" | "short_offsets" => {
                let file = if damage == "short_data" {
                    &data
                } else {
                    &offsets
                };
                let mut bytes = fs::read(file).unwrap();
                bytes.pop();
                fs::write(file, bytes).unwrap();
            }
            "first_offset" | "last_offset" => {
                let mut b = fs::read(&offsets).unwrap();
                let index = if damage == "first_offset" {
                    1
                } else {
                    b.len() - 8
                };
                b[index..index + 8].copy_from_slice(&u64::MAX.to_le_bytes());
                fs::write(&offsets, b).unwrap();
            }
            _ => (),
        }
        let paths = [data.clone(), offsets.clone(), data.with_extension("conf")];
        let before: Vec<_> = paths.iter().map(|p| fs::read(p).ok()).collect();
        let mut storage = ImmutableDataStorage::open(
            config(ImmutableDataCompression::None),
            if damage == "identity" {
                [8; 32]
            } else {
                [7; 32]
            },
            dir.path(),
        )
        .unwrap();
        for result in [
            storage
                .get("events_v1", 0, &ImmutableDataReadOptions::all())
                .map(|_| ()),
            storage.append("events_v1", 1, &row(2)).map(|_| ()),
            storage.truncate("events_v1", 0),
        ] {
            assert!(
                matches!(result, Err(ImmutableDataStorageError::Corrupt(_))),
                "{damage}: {result:?}"
            );
        }
        let after: Vec<_> = paths.iter().map(|p| fs::read(p).ok()).collect();
        assert_eq!(before, after, "{damage}");
    }
}

#[test]
fn large_global_ordinals_and_overflow_use_small_local_rows() {
    for base in [u32::MAX as u64 + 10, u64::MAX - 1] {
        let dir = tempfile::tempdir().unwrap();
        let jar = NippyJar::new(
            2,
            &path(dir.path(), 4),
            FileHeader {
                format: 1,
                genesis_id: [7; 32],
                segment: 0,
                first_commit: 4,
                first_ordinal: base,
            },
        );
        let mut writer = NippyJarWriter::new(jar).unwrap();
        for v in [42u64.to_be_bytes().to_vec(), vec![]] {
            writer.append_column(Some(Ok(v))).unwrap();
        }
        writer.commit().unwrap();
        drop(writer);
        let mut storage =
            ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
                .unwrap();
        assert_eq!(number(&storage, base), 42);
        if base == u64::MAX - 1 {
            for commit in [5, 8] {
                assert!(matches!(
                    storage.append("events_v1", commit, &row(43)),
                    Err(ImmutableDataStorageError::Overflow)
                ));
            }
            assert_eq!(storage.end_ordinal("events_v1").unwrap(), u64::MAX);
            assert_eq!(number(&storage, base), 42);
            assert!(!path(dir.path(), 8).exists());
            assert!(matches!(
                storage.get("events_v1", u64::MAX, &ImmutableDataReadOptions::all()),
                Err(ImmutableDataStorageError::NotFound)
            ));
            continue;
        }
        assert_eq!(storage.append("events_v1", 5, &row(43)).unwrap(), base + 1);
        storage.sync().unwrap();
        assert_eq!(number(&storage, base + 1), 43);
    }
}

#[test]
fn projection_skips_invalid_unselected_payload_and_supports_column_63() {
    let dir = tempfile::tempdir().unwrap();
    let columns = (0..64)
        .map(|i| {
            ImmutableDataColumn::new(format!("c{i}")).with_type(if i == 63 {
                CellType::Uint(Width::W8)
            } else {
                CellType::Bool
            })
        })
        .collect();
    let cfg = ImmutableDataConfig::new(
        4,
        vec![ImmutableDataSegment {
            name: "events_v1".into(),
            columns,
            compression: ImmutableDataCompression::None,
        }],
    );
    let jar = NippyJar::new(
        64,
        &path(dir.path(), 0),
        FileHeader {
            format: 1,
            genesis_id: [7; 32],
            segment: 0,
            first_commit: 0,
            first_ordinal: 0,
        },
    );
    let mut writer = NippyJarWriter::new(jar).unwrap();
    for i in 0..64 {
        writer
            .append_column(Some(Ok(if i == 63 {
                42u64.to_be_bytes().to_vec()
            } else {
                vec![255]
            })))
            .unwrap();
    }
    writer.commit().unwrap();
    drop(writer);
    let storage = ImmutableDataStorage::open(cfg, [7; 32], dir.path()).unwrap();
    assert_eq!(
        storage
            .get("events_v1", 0, &ImmutableDataReadOptions::columns(["c63"]))
            .unwrap()
            .get::<u64>("c63")
            .unwrap(),
        42
    );
    assert!(matches!(
        storage.get("events_v1", 0, &ImmutableDataReadOptions::all()),
        Err(ImmutableDataStorageError::Input(
            ImmutableDataError::InvalidPayload { column: 0, .. }
        ))
    ));
}

#[test]
fn end_ordinal_at_uses_latest_range_within_segment_and_commit_bound() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    metadata::record_commit_range(&mut tx, 0, 3, 0..5).unwrap();
    metadata::record_commit_range(&mut tx, 0, 8, 5..9).unwrap();
    metadata::record_commit_range(&mut tx, 1, 2, 0..20).unwrap();
    metadata::record_commit_range(&mut tx, 1, u64::MAX, 20..u64::MAX).unwrap();
    for (segment, commit, expected) in [
        (0, 0, 0),
        (0, 3, 5),
        (0, 7, 5),
        (0, 8, 9),
        (0, u64::MAX, 9),
        (1, 1, 0),
        (1, 2, 20),
        (1, 8, 20),
        (1, u64::MAX, u64::MAX),
        (2, u64::MAX, 0),
    ] {
        assert_eq!(
            metadata::end_ordinal_at(&tx, segment, commit).unwrap(),
            expected
        );
    }
    let key = [0u32.to_be_bytes().as_slice(), &8u64.to_be_bytes()].concat();
    tx.put(metadata::COMMIT_RANGES, &key, b"bad").unwrap();
    assert!(matches!(
        metadata::end_ordinal_at(&tx, 0, 9),
        Err(ImmutableDataStorageError::Corrupt(_))
    ));
    assert_eq!(metadata::end_ordinal_at(&tx, 0, 7).unwrap(), 5);
    tx.put(metadata::COMMIT_RANGES, &2u32.to_be_bytes(), &[0; 16])
        .unwrap();
    assert!(matches!(
        metadata::end_ordinal_at(&tx, 2, 9),
        Err(ImmutableDataStorageError::Corrupt(_))
    ));
}

#[test]
fn recovery_trims_partial_and_synced_tails_and_is_idempotent() {
    use std::io::Write;
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        for damage in [
            "data",
            "offsets",
            "config",
            "recovery_offsets",
            "recovery_data",
            "new_file",
            "new_file_without_config",
            "tmp_only",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = MemoryStore::new();
            {
                let mut storage =
                    ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
                storage.append("events_v1", 1, &row(42)).unwrap();
                storage.sync().unwrap();
                let mut tx = db.begin_write().unwrap();
                storage
                    .record_commit_range(&mut tx, "events_v1", 1, 0..1)
                    .unwrap();
                tx.commit().unwrap();
                if damage == "config" || damage.starts_with("recovery_") {
                    let data = path(dir.path(), 0);
                    let data_len = fs::metadata(&data).unwrap().len();
                    let offsets_len = fs::metadata(data.with_extension("off")).unwrap().len();
                    storage.append("events_v1", 2, &row(43)).unwrap();
                    storage.sync().unwrap();
                    // Simulate recovery interrupted after offsets, or after both
                    // files, were trimmed but before the config was replaced.
                    if damage.starts_with("recovery_") {
                        fs::OpenOptions::new()
                            .write(true)
                            .open(data.with_extension("off"))
                            .unwrap()
                            .set_len(offsets_len)
                            .unwrap();
                    }
                    if damage == "recovery_data" {
                        fs::OpenOptions::new()
                            .write(true)
                            .open(data)
                            .unwrap()
                            .set_len(data_len)
                            .unwrap();
                    }
                }
                if damage.starts_with("new_file") {
                    storage.append("events_v1", 4, &row(43)).unwrap();
                    storage.sync().unwrap();
                }
            }
            let data = path(dir.path(), 0);
            if damage == "data" || damage == "offsets" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&data)
                    .unwrap()
                    .write_all(b"partial data")
                    .unwrap();
            }
            if damage == "offsets" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(data.with_extension("off"))
                    .unwrap()
                    .write_all(&[255; 5])
                    .unwrap();
            }
            if damage == "new_file_without_config" {
                fs::remove_file(path(dir.path(), 4).with_extension("conf")).unwrap();
                fs::write(path(dir.path(), 4).with_extension("tmp"), b"torn config").unwrap();
            }
            if damage == "tmp_only" {
                fs::write(path(dir.path(), 4).with_extension("tmp"), b"torn config").unwrap();
            }
            let mut storage =
                ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
            let tx = db.begin_read().unwrap();
            storage.recover(&tx, 3).unwrap();
            storage.recover(&tx, 3).unwrap();
            assert_eq!(storage.end_ordinal("events_v1").unwrap(), 1, "{damage}");
            assert_eq!(number(&storage, 0), 42);
            assert!(!path(dir.path(), 4).exists());
            assert!(!path(dir.path(), 4).with_extension("tmp").exists());
            assert_eq!(storage.append("events_v1", 4, &row(44)).unwrap(), 1);
            storage.sync().unwrap();
        }
    }
}

#[test]
fn recovery_resets_a_failed_active_writer_after_transient_sync_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db = MemoryStore::new();
    let mut storage =
        ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
            .unwrap();
    storage.append("events_v1", 1, &row(42)).unwrap();
    storage.sync().unwrap();
    let mut tx = db.begin_write().unwrap();
    storage
        .record_commit_range(&mut tx, "events_v1", 1, 0..1)
        .unwrap();
    tx.commit().unwrap();
    storage.append("events_v1", 2, &row(43)).unwrap();
    let temporary = path(dir.path(), 0).with_extension("tmp");
    fs::create_dir(&temporary).unwrap(); // make atomic config replacement fail
    assert!(matches!(
        storage.sync(),
        Err(ImmutableDataStorageError::NippyJar(_))
    ));
    fs::remove_dir(&temporary).unwrap();
    // Removing the I/O obstacle must not make a partially synced writer usable.
    for result in [
        storage.append("events_v1", 2, &row(44)).map(|_| ()),
        storage
            .get("events_v1", 0, &ImmutableDataReadOptions::all())
            .map(|_| ()),
        storage.sync(),
        storage.end_ordinal("events_v1").map(|_| ()),
    ] {
        assert!(
            matches!(
                result,
                Err(ImmutableDataStorageError::Invalid(
                    "file writer failed; recovery required"
                ))
            ),
            "{result:?}"
        );
    }
    let tx = db.begin_read().unwrap();
    assert_eq!(
        storage
            .reader()
            .get(
                &tx,
                1,
                "events_v1",
                ImmutableDataAddress::Ordinal(0),
                &ImmutableDataReadOptions::all()
            )
            .unwrap(),
        row(42)
    );
    storage.recover(&tx, 1).unwrap();
    assert_eq!(number(&storage, 0), 42);
    assert_eq!(storage.append("events_v1", 2, &row(44)).unwrap(), 1);
    storage.sync().unwrap();
}

#[test]
fn recovery_never_discards_missing_committed_data_or_future_metadata() {
    for damage in ["missing", "short", "future"] {
        let dir = tempfile::tempdir().unwrap();
        let db = MemoryStore::new();
        let mut storage =
            ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
                .unwrap();
        storage.append("events_v1", 1, &row(42)).unwrap();
        storage.sync().unwrap();
        let mut tx = db.begin_write().unwrap();
        metadata::record_commit_range(&mut tx, 0, if damage == "future" { 2 } else { 1 }, 0..1)
            .unwrap();
        tx.commit().unwrap();
        if damage == "missing" {
            fs::remove_file(path(dir.path(), 0)).unwrap();
        }
        if damage == "short" {
            fs::OpenOptions::new()
                .write(true)
                .open(path(dir.path(), 0))
                .unwrap()
                .set_len(1)
                .unwrap();
        }
        let conf = fs::read(path(dir.path(), 0).with_extension("conf")).unwrap();
        let result = storage.recover(&db.begin_read().unwrap(), 1);
        match damage {
            "missing" => assert!(
                matches!(result,
                Err(ImmutableDataStorageError::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound),
                "{result:?}"
            ),
            "short" => assert!(
                matches!(
                    result,
                    Err(ImmutableDataStorageError::Corrupt("missing published data"))
                ),
                "{result:?}"
            ),
            "future" => assert!(
                matches!(
                    result,
                    Err(ImmutableDataStorageError::Corrupt(
                        "commit range exceeds published head"
                    ))
                ),
                "{result:?}"
            ),
            _ => unreachable!(),
        }
        assert_eq!(
            fs::read(path(dir.path(), 0).with_extension("conf")).unwrap(),
            conf
        );
    }
}

#[test]
fn committed_reader_ignores_unflushed_and_partial_tails_without_flushing_writer() {
    use std::io::Write;
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = MemoryStore::new();
        let mut storage =
            ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
        storage.append("events_v1", 1, &row(42)).unwrap();
        storage.sync().unwrap();
        let mut tx = db.begin_write().unwrap();
        storage
            .record_commit_range(&mut tx, "events_v1", 1, 0..1)
            .unwrap();
        storage
            .bind_key(&mut tx, "events_v1", ImmutableDataKey([1; 32]), 0)
            .unwrap();
        tx.commit().unwrap();
        let reader = storage.reader();
        let tx = db.begin_read().unwrap();
        let conf = fs::read(path(dir.path(), 0).with_extension("conf")).unwrap();
        storage.append("events_v1", 2, &row(43)).unwrap();
        // A process may be partway through data and offset writes. None of this
        // tail belongs to the reader's committed prefix or should be flushed by it.
        fs::OpenOptions::new()
            .append(true)
            .open(path(dir.path(), 0))
            .unwrap()
            .write_all(b"partial data")
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(path(dir.path(), 0).with_extension("off"))
            .unwrap()
            .write_all(&[255; 5])
            .unwrap();
        // Global head could be 9 with no segment rows since commit 1. A partially
        // created file in span 4 must still be ignored using this segment's bound.
        fs::write(path(dir.path(), 4), b"partial new file").unwrap();
        fs::write(path(dir.path(), 4).with_extension("off"), [8]).unwrap();
        for address in [
            ImmutableDataAddress::Ordinal(0),
            ImmutableDataAddress::Key(ImmutableDataKey([1; 32])),
        ] {
            assert_eq!(
                reader
                    .get(
                        &tx,
                        9,
                        "events_v1",
                        address,
                        &ImmutableDataReadOptions::columns(["number"])
                    )
                    .unwrap()
                    .get::<u64>("number")
                    .unwrap(),
                42
            );
        }
        assert_eq!(
            reader
                .rows_of(&tx, 9, "events_v1", 1, &ImmutableDataReadOptions::all())
                .unwrap(),
            vec![row(42)]
        );
        assert!(matches!(
            reader.get(
                &tx,
                9,
                "events_v1",
                ImmutableDataAddress::Ordinal(1),
                &ImmutableDataReadOptions::all()
            ),
            Err(ImmutableDataStorageError::NotFound)
        ));
        assert_eq!(
            fs::read(path(dir.path(), 0).with_extension("conf")).unwrap(),
            conf
        );
        storage.recover(&tx, 9).unwrap();
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 1);
        assert_eq!(
            reader
                .get(
                    &tx,
                    9,
                    "events_v1",
                    ImmutableDataAddress::Ordinal(0),
                    &ImmutableDataReadOptions::all()
                )
                .unwrap()
                .get::<u64>("number")
                .unwrap(),
            42
        );
    }
}

#[test]
fn reader_keeps_directory_owned_after_writer_drops() {
    let dir = tempfile::tempdir().unwrap();
    let db = MemoryStore::new();
    let cfg = config(ImmutableDataCompression::None);
    let mut storage = ImmutableDataStorage::open(cfg.clone(), [7; 32], dir.path()).unwrap();
    storage.append("events_v1", 1, &row(42)).unwrap();
    storage.sync().unwrap();
    let mut tx = db.begin_write().unwrap();
    storage
        .record_commit_range(&mut tx, "events_v1", 1, 0..1)
        .unwrap();
    tx.commit().unwrap();
    let reader = storage.reader();
    let last = reader.clone();
    drop(reader);
    drop(storage);
    assert!(matches!(
        ImmutableDataStorage::open(cfg.clone(), [7; 32], dir.path()),
        Err(ImmutableDataStorageError::Invalid(
            "immutable directory already owned or locking unsupported"
        ))
    ));
    assert_eq!(
        last.get(
            &db.begin_read().unwrap(),
            1,
            "events_v1",
            ImmutableDataAddress::Ordinal(0),
            &ImmutableDataReadOptions::all()
        )
        .unwrap()
        .get::<u64>("number")
        .unwrap(),
        42
    );
    drop(last);
    assert!(ImmutableDataStorage::open(cfg, [7; 32], dir.path()).is_ok());
}

#[test]
fn committed_reader_decodes_high_expansion_columns_and_preserves_projection_order() {
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = MemoryStore::new();
        let mut storage =
            ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
        let body = vec![7; 128 * 1024];
        let large =
            ImmutableDataRow::new(vec![CellValue::from_u64(42), CellValue::from_bytes(&body)]);
        storage.append("events_v1", 1, &large).unwrap();
        storage.sync().unwrap();
        let mut tx = db.begin_write().unwrap();
        storage
            .record_commit_range(&mut tx, "events_v1", 1, 0..1)
            .unwrap();
        tx.commit().unwrap();
        let reader = storage.reader();
        let tx = db.begin_read().unwrap();
        let selected = reader
            .get(
                &tx,
                1,
                "events_v1",
                ImmutableDataAddress::Ordinal(0),
                &ImmutableDataReadOptions::columns(["body", "number"]),
            )
            .unwrap();
        assert_eq!(selected.get::<Vec<u8>>("body").unwrap(), body);
        assert_eq!(selected.get::<u64>("number").unwrap(), 42);
        assert_eq!(
            selected
                .iter()
                .unwrap()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["body", "number"]
        );
    }
}
