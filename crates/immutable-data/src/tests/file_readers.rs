use super::*;
use crate::{ImmutableDataColumn, ImmutableDataSchema};
use std::{sync::mpsc, time::Duration};

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
        let (started, wait_start) = mpsc::channel();
        let (finished, wait_cleanup) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            if append {
                assert_eq!(writer.append(&segment, 2, &[b"replacement"]).unwrap(), 1);
                writer.sync().unwrap();
            } else {
                assert_eq!(writer.get(&segment, 0, &[0]).unwrap()[0], b"published");
            }
            finished.send(()).unwrap();
        });
        wait_start.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            wait_cleanup
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        assert_eq!(fs::metadata(jar.data_path()).unwrap().len(), data_len);
        let end = usize::try_from(mapped.offset(1).unwrap()).unwrap();
        assert_eq!(mapped.data(0..end), b"published");
        drop(mapped);
        drop(reading);
        wait_cleanup.recv_timeout(Duration::from_secs(5)).unwrap();
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
            let (finished, wait_cleanup) = mpsc::channel();
            let worker = std::thread::spawn(move || {
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
            // Appending and syncing must finish despite a live read mapping.
            wait_append.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(
                wait_cleanup
                    .recv_timeout(Duration::from_millis(50))
                    .is_err()
            );
            assert_eq!(
                cursor.row_by_number_with_cols(0, 1).unwrap().unwrap()[0],
                b"published"
            );
            drop(cursor);
            drop(reading);
            wait_cleanup.recv_timeout(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
        }
    }
}
