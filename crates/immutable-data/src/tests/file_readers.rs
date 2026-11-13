use super::test_sync::{Point, TIMEOUT, signal_contention, with_hook};
use super::*;
use crate::{ImmutableDataColumn, ImmutableDataSchema};
use std::sync::mpsc;

#[test]
fn committed_get_does_not_list_directory() {
    use crate::{
        ImmutableDataAddress, ImmutableDataConfig, ImmutableDataKey, ImmutableDataReadOptions,
        ImmutableDataRow, ImmutableDataSegment, ImmutableDataStorage, metadata,
    };
    use golemdb_cells::CellValue;
    use golemdb_storage::{MemoryStore, Store, WriteTransaction};

    let dir = tempfile::tempdir().unwrap();
    let config = ImmutableDataConfig::new(
        4,
        vec![ImmutableDataSegment {
            name: "events".into(),
            columns: vec![ImmutableDataColumn::new("body")],
            compression: ImmutableDataCompression::Lz4,
        }],
    );
    let mut storage = ImmutableDataStorage::open(config, [7; 32], dir.path()).unwrap();
    let store = MemoryStore::new();
    let mut tx = store.begin_write().unwrap();
    // Include an older shard, a skipped span and two batches in the newest shard.
    for (ordinal, commit) in [1, 9, 11].into_iter().enumerate() {
        let ordinal = ordinal as u64;
        let row = ImmutableDataRow::new(vec![CellValue::from_bytes(b"published")]);
        storage.append("events", commit, &row).unwrap();
        metadata::record_commit_range(&mut tx, 0, commit, ordinal..ordinal + 1).unwrap();
        metadata::bind_key(&mut tx, 0, ImmutableDataKey([ordinal as u8; 32]), ordinal).unwrap();
    }
    storage.sync().unwrap();
    tx.commit().unwrap();
    let tx = store.begin_read().unwrap();
    let reader = storage.reader();
    for ordinal in 0..3 {
        for address in [
            ImmutableDataAddress::Ordinal(ordinal),
            ImmutableDataAddress::Key(ImmutableDataKey([ordinal as u8; 32])),
        ] {
            let row = with_hook(
                Point::DirectoryScan,
                || {
                    Err(Error::Io(std::io::Error::other(
                        "directory listing unavailable",
                    )))
                },
                || reader.get(&tx, 20, "events", address, &ImmutableDataReadOptions::all()),
            )
            .unwrap();
            assert_eq!(row.get::<Vec<u8>>("body").unwrap(), b"published");
        }
    }
}

#[test]
fn reopened_file_tail_recovery_waits_for_live_mmaps() {
    for append in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let segment = Segment {
            id: 0,
            schema: ImmutableDataSchema::new("events", vec![ImmutableDataColumn::new("body")])
                .unwrap(),
            compression: ImmutableDataCompression::None,
        };
        {
            let mut writer = NippyJarStorage::open(dir.path(), [7; 32], 4).unwrap();
            writer.append(&segment, 1, &[b"published"]).unwrap();
            writer.sync().unwrap();
            writer.append(&segment, 2, &[b"unfinished"]).unwrap();
        }
        let mut writer = NippyJarStorage::open(dir.path(), [7; 32], 4).unwrap();
        let files = writer.reader();
        let reading = files.cleanup.read().unwrap();
        let jar = files.configuration(&segment, 0).unwrap();
        let mapped = jar.open_data_reader().unwrap();
        let data_len = fs::metadata(jar.data_path()).unwrap().len();
        assert!(data_len > b"published".len() as u64);
        let (contended, wait_contention) = mpsc::channel();
        let (finished, wait_cleanup) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            signal_contention(contended, || {
                if append {
                    assert_eq!(writer.append(&segment, 2, &[b"replacement"]).unwrap(), 1);
                    writer.sync().unwrap();
                } else {
                    assert_eq!(writer.get(&segment, 0, &[0]).unwrap()[0], b"published");
                }
                finished.send(()).unwrap();
            });
        });
        // Cleanup reached the exclusive lock and found our live mapping guard.
        wait_contention.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(fs::metadata(jar.data_path()).unwrap().len(), data_len);
        let end = usize::try_from(mapped.offset(1).unwrap()).unwrap();
        assert_eq!(mapped.data(0..end), b"published");
        drop(mapped);
        drop(reading);
        wait_cleanup.recv_timeout(TIMEOUT).unwrap();
        worker.join().unwrap();
    }
}

#[test]
fn append_runs_with_live_mmaps_but_tail_cleanup_waits_for_them() {
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        for recovery in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let segment = Segment {
                id: 0,
                schema: ImmutableDataSchema::new("events", vec![ImmutableDataColumn::new("body")])
                    .unwrap(),
                compression,
            };
            let mut writer = NippyJarStorage::open(dir.path(), [7; 32], 4).unwrap();
            writer.append(&segment, 1, &[b"published"]).unwrap();
            writer.sync().unwrap();
            let files = writer.reader();
            let reading = files.cleanup.read().unwrap();
            let jar = files.configuration(&segment, 0).unwrap();
            let mut cursor = NippyJarCursor::new(&jar).unwrap();
            let (appended, wait_append) = mpsc::channel();
            let (contended, wait_contention) = mpsc::channel();
            let (finished, wait_cleanup) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                signal_contention(contended, || {
                    writer.append(&segment, 2, &[b"unpublished"]).unwrap();
                    writer.sync().unwrap();
                    appended.send(()).unwrap();
                    if recovery {
                        writer
                            .recover(&segment, &BTreeMap::from([(0, 0..1)]))
                            .unwrap();
                    } else {
                        writer.truncate(&segment, 1).unwrap();
                    }
                    finished.send(()).unwrap();
                    assert_eq!(writer.end_ordinal(&segment).unwrap(), 1);
                });
            });
            // Appending and syncing must finish despite a live read mapping.
            wait_append.recv_timeout(TIMEOUT).unwrap();
            wait_contention.recv_timeout(TIMEOUT).unwrap();
            assert_eq!(
                cursor.row_by_number_with_cols(0, 1).unwrap().unwrap()[0],
                b"published"
            );
            drop(cursor);
            drop(reading);
            wait_cleanup.recv_timeout(TIMEOUT).unwrap();
            worker.join().unwrap();
        }
    }
}

#[test]
fn partial_column_append_blocks_physical_operations_until_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let segment = Segment {
        id: 0,
        schema: ImmutableDataSchema::new(
            "events",
            vec![
                ImmutableDataColumn::new("first"),
                ImmutableDataColumn::new("last"),
            ],
        )
        .unwrap(),
        compression: ImmutableDataCompression::None,
    };
    let mut writer = NippyJarStorage::open(dir.path(), [7; 32], 4).unwrap();
    writer
        .append(&segment, 1, &[b"first", b"published"])
        .unwrap();
    writer.sync().unwrap();
    let result = with_hook(
        Point::ColumnAppended,
        || Err(Error::Io(std::io::Error::other("injected partial append"))),
        || writer.append(&segment, 2, &[&vec![7; 128 * 1024], b"unfinished"]),
    );
    assert!(
        matches!(result, Err(Error::Io(ref error)) if error.to_string() == "injected partial append")
    );
    for result in [
        writer.append(&segment, 2, &[b"next", b"row"]).map(|_| ()),
        writer.get(&segment, 0, &[0, 1]).map(|_| ()),
        writer.sync(),
        writer.end_ordinal(&segment).map(|_| ()),
    ] {
        assert!(
            matches!(
                result,
                Err(Error::Invalid("file writer failed; recovery required"))
            ),
            "{result:?}"
        );
    }
    // Committed readers remain independent of the failed physical writer.
    let files = writer.reader();
    assert_eq!(
        files.read_range(&segment, 0, 0..1, &[0, 1]).unwrap(),
        vec![vec![b"first".to_vec(), b"published".to_vec()]]
    );
    writer
        .recover(&segment, &BTreeMap::from([(0, 0..1)]))
        .unwrap();
    assert_eq!(writer.end_ordinal(&segment).unwrap(), 1);
    assert_eq!(
        writer
            .append(&segment, 2, &[b"replacement", b"complete"])
            .unwrap(),
        1
    );
    writer.sync().unwrap();
    assert_eq!(
        writer.get(&segment, 1, &[0, 1]).unwrap(),
        [b"replacement".to_vec(), b"complete".to_vec()]
    );
    assert_eq!(
        files.read_range(&segment, 0, 0..1, &[1]).unwrap(),
        vec![vec![b"published".to_vec()]]
    );
}

#[test]
fn last_published_column_remains_exact_while_writer_extends_the_same_file() {
    // Incompressible payload exceeds NippyJar's write buffer for every codec.
    let mut state = 1u32;
    let growing: Vec<u8> = (0..128 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    for compression in [
        ImmutableDataCompression::None,
        ImmutableDataCompression::Lz4,
        ImmutableDataCompression::Zstd,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let segment = Segment {
            id: 0,
            schema: ImmutableDataSchema::new(
                "events",
                vec![
                    ImmutableDataColumn::new("first"),
                    ImmutableDataColumn::new("last"),
                ],
            )
            .unwrap(),
            compression,
        };
        let mut writer = NippyJarStorage::open(dir.path(), [7; 32], 4).unwrap();
        let published = b"last published column\x00\xff";
        writer.append(&segment, 1, &[b"first", published]).unwrap();
        writer.sync().unwrap();
        let files = writer.reader();
        let path = files.path(&segment, 0).unwrap();
        let data_len = fs::metadata(&path).unwrap().len();
        let config = fs::read(path.with_extension("conf")).unwrap();
        let offsets = fs::read(path.with_extension("off")).unwrap();
        let (entered, wait_entered) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let (read, wait_read) = mpsc::channel();
        let read_segment = Segment {
            id: segment.id,
            schema: segment.schema.clone(),
            compression,
        };
        let reader = files.clone();
        let worker = std::thread::spawn(move || {
            with_hook(
                Point::PrefixRead,
                move || {
                    entered.send(()).unwrap();
                    wait_release.recv_timeout(TIMEOUT).unwrap();
                    Ok(())
                },
                || {
                    // Configuration is captured before the writer grows the file.
                    read.send(reader.read_range(&read_segment, 0, 0..1, &[1]))
                        .unwrap();
                },
            );
        });
        wait_entered.recv_timeout(TIMEOUT).unwrap();
        with_hook(
            Point::ColumnAppended,
            move || {
                // The new first column is on disk, but offsets/config still describe
                // the published row. An EOF-based read would include the new bytes.
                assert!(fs::metadata(&path).unwrap().len() > data_len);
                assert_eq!(fs::read(path.with_extension("conf")).unwrap(), config);
                assert_eq!(fs::read(path.with_extension("off")).unwrap(), offsets);
                release.send(()).unwrap();
                assert_eq!(
                    wait_read.recv_timeout(TIMEOUT).unwrap().unwrap(),
                    vec![vec![published.to_vec()]]
                );
                Ok(())
            },
            || {
                writer
                    .append(&segment, 2, &[&growing, b"new last column"])
                    .unwrap()
            },
        );
        worker.join().unwrap();
        writer.sync().unwrap();
        assert_eq!(
            files.read_range(&segment, 1, 0..1, &[1]).unwrap(),
            vec![vec![published.to_vec()]]
        );
        assert_eq!(
            writer.get(&segment, 1, &[0, 1]).unwrap(),
            [growing.clone(), b"new last column".to_vec()]
        );
    }
}
