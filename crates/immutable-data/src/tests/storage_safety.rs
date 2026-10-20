//! Isolated corruption fixtures: each keeps unrelated bounds and encodings valid.
use super::*;

#[test]
fn forward_key_requires_its_matching_reverse_entry() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    let key = ImmutableDataKey([3; 32]);
    metadata::bind_key(&mut tx, 0, key, 7).unwrap();
    let reverse = [
        vec![1],
        0u32.to_be_bytes().to_vec(),
        7u64.to_be_bytes().to_vec(),
    ]
    .concat();
    tx.delete(metadata::KEYS, &reverse).unwrap();
    assert!(matches!(
        metadata::ordinal_for_key(&tx, 0, key),
        Err(ImmutableDataStorageError::Corrupt(
            "key binding has no matching reverse entry"
        ))
    ));
}

#[test]
fn recovery_rejects_gaps_even_when_all_physical_rows_exist() {
    for files in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(ImmutableDataCompression::None);
        let mut storage = if files {
            ImmutableDataStorage::open(cfg, [7; 32], dir.path()).unwrap()
        } else {
            ImmutableDataStorage::in_memory(cfg, [7; 32]).unwrap()
        };
        for n in 0..3 {
            storage.append("events_v1", 1, &row(n)).unwrap();
        }
        storage.sync().unwrap();
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        metadata::record_commit_range(&mut tx, 0, 1, 0..1).unwrap();
        metadata::record_commit_range(&mut tx, 0, 2, 2..3).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            storage.recover(&db.begin_read().unwrap(), 2),
            Err(ImmutableDataStorageError::Corrupt(
                "noncontiguous published ranges"
            ))
        ));
        assert_eq!(storage.end_ordinal("events_v1").unwrap(), 3);
        for n in 0..3 {
            assert_eq!(
                storage
                    .get("events_v1", n, &ImmutableDataReadOptions::all())
                    .unwrap(),
                row(n)
            );
        }
    }
}

#[test]
fn memory_recovery_checks_span_associations_before_truncating() {
    let mut storage =
        ImmutableDataStorage::in_memory(config(ImmutableDataCompression::None), [7; 32]).unwrap();
    storage.append("events_v1", 4, &row(42)).unwrap();
    storage.append("events_v1", 4, &row(43)).unwrap();
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    metadata::record_commit_range(&mut tx, 0, 1, 0..1).unwrap();
    // Contiguous range, valid length, but commit 1 names span 0 instead of 4.
    assert!(matches!(
        storage.recover(&tx, 4),
        Err(ImmutableDataStorageError::Corrupt(
            "published rows belong to another file span"
        ))
    ));
    assert_eq!(storage.end_ordinal("events_v1").unwrap(), 2);
    assert_eq!(
        storage
            .get("events_v1", 0, &ImmutableDataReadOptions::all())
            .unwrap(),
        row(42)
    );
    assert_eq!(
        storage
            .get("events_v1", 1, &ImmutableDataReadOptions::all())
            .unwrap(),
        row(43)
    );
    tx.abort();
    let mut tx = db.begin_write().unwrap();
    metadata::record_commit_range(&mut tx, 0, 4, 0..1).unwrap();
    storage.recover(&tx, 4).unwrap();
    assert_eq!(storage.end_ordinal("events_v1").unwrap(), 1);
}

#[test]
fn file_configuration_checks_segment_and_compression_independently() {
    for wrong_segment in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let jar = NippyJar::new(
            2,
            &path(dir.path(), 0),
            FileHeader {
                format: 1,
                genesis_id: [7; 32],
                segment: u32::from(wrong_segment),
                first_commit: 0,
                first_ordinal: 0,
            },
        );
        let mut writer = NippyJarWriter::new(jar).unwrap();
        let expected = row(42);
        for payload in expected.payloads() {
            writer.append_column(Some(Ok(payload))).unwrap();
        }
        writer.commit().unwrap();
        drop(writer);
        // The fixture is a valid uncompressed jar. Only one declaration differs.
        let compression = if wrong_segment {
            ImmutableDataCompression::None
        } else {
            ImmutableDataCompression::Lz4
        };
        let mut storage =
            ImmutableDataStorage::open(config(compression), [7; 32], dir.path()).unwrap();
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        metadata::record_commit_range(&mut tx, 0, 1, 0..1).unwrap();
        let message = if wrong_segment {
            "file identity disagrees with genesis or filename"
        } else {
            "file compression disagrees with schema"
        };
        let before = ["jar", "conf", "off"]
            .map(|ext| fs::read(path(dir.path(), 0).with_extension(ext)).unwrap());
        for result in [
            storage
                .reader()
                .get(
                    &tx,
                    1,
                    "events_v1",
                    ImmutableDataAddress::Ordinal(0),
                    &ImmutableDataReadOptions::all(),
                )
                .map(|_| ()),
            storage.append("events_v1", 2, &row(43)).map(|_| ()),
            storage.recover(&tx, 1),
        ] {
            assert!(
                matches!(result, Err(ImmutableDataStorageError::Corrupt(actual)) if actual == message),
                "{result:?}"
            );
        }
        assert_eq!(
            before,
            ["jar", "conf", "off"]
                .map(|ext| fs::read(path(dir.path(), 0).with_extension(ext)).unwrap())
        );
    }
}

#[test]
fn committed_reader_validates_the_prefix_even_for_an_intact_selected_column() {
    let dir = tempfile::tempdir().unwrap();
    let mut storage =
        ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
            .unwrap();
    storage.append("events_v1", 1, &row(42)).unwrap();
    storage.sync().unwrap();
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    metadata::record_commit_range(&mut tx, 0, 1, 0..1).unwrap();
    let offsets = path(dir.path(), 0).with_extension("off");
    let original = fs::read(&offsets).unwrap();
    let mut damaged = original.clone();
    damaged[1..9].copy_from_slice(&1u64.to_le_bytes());
    fs::write(&offsets, &damaged).unwrap();
    // The body's own offsets and bytes are intact. Skipping prefix validation
    // would return it successfully despite the invalid first file offset.
    let options = ImmutableDataReadOptions::columns(["body"]);
    let reader = storage.reader();
    assert!(matches!(
        reader.get(
            &tx,
            1,
            "events_v1",
            ImmutableDataAddress::Ordinal(0),
            &options
        ),
        Err(ImmutableDataStorageError::Corrupt(
            "incomplete published file prefix"
        ))
    ));
    assert!(matches!(
        reader.rows_of(&tx, 1, "events_v1", 1, &options),
        Err(ImmutableDataStorageError::Corrupt(
            "incomplete published file prefix"
        ))
    ));
    assert_eq!(fs::read(&offsets).unwrap(), damaged);
    fs::write(&offsets, original).unwrap();
    assert_eq!(
        reader
            .get(
                &tx,
                1,
                "events_v1",
                ImmutableDataAddress::Ordinal(0),
                &options
            )
            .unwrap()
            .get::<Vec<u8>>("body")
            .unwrap(),
        b"data"
    );
}

#[test]
fn committed_reader_does_not_inspect_newer_span_configurations() {
    let dir = tempfile::tempdir().unwrap();
    let mut storage =
        ImmutableDataStorage::open(config(ImmutableDataCompression::None), [7; 32], dir.path())
            .unwrap();
    storage.append("events_v1", 1, &row(42)).unwrap();
    storage.sync().unwrap();
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    metadata::record_commit_range(&mut tx, 0, 1, 0..1).unwrap();
    tx.commit().unwrap();
    let tx = db.begin_read().unwrap();
    storage.append("events_v1", 4, &row(99)).unwrap();
    storage.sync().unwrap();
    let reader = storage.reader();
    let newer = path(dir.path(), 4).with_extension("conf");
    // Head 9 can include empty commits. This segment's last published batch
    // is still commit 1, so even span 4 is outside the reader's file boundary.
    for torn in [false, true] {
        if torn {
            fs::write(&newer, b"incomplete future configuration").unwrap();
        }
        assert_eq!(
            reader
                .get(
                    &tx,
                    9,
                    "events_v1",
                    ImmutableDataAddress::Ordinal(0),
                    &ImmutableDataReadOptions::all()
                )
                .unwrap(),
            row(42)
        );
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
    }
}
