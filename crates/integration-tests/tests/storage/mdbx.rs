use golemdb_storage::*;
use golemdb_storage_mdbx::{MdbxOptions, MdbxStore};
use proptest::prelude::*;
use std::{
    collections::BTreeMap,
    ops::Bound::{self, Excluded, Included, Unbounded},
};

const A: Table = Table("A");
const B: Table = Table("B");
#[test]
fn crud() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    assert_eq!(tx.get(A, b"a").unwrap(), None);
    tx.insert(A, b"a", b"one").unwrap();
    assert!(matches!(
        tx.insert(A, b"a", b"two"),
        Err(StorageError::AlreadyExists)
    ));
    assert_eq!(tx.get(A, b"a").unwrap(), Some(b"one".to_vec()));
    tx.put(A, b"a", b"two").unwrap();
    tx.put(B, b"a", b"other table").unwrap();
    assert_eq!(tx.get(A, b"a").unwrap(), Some(b"two".to_vec()));
    assert!(tx.delete(A, b"a").unwrap());
    assert!(!tx.delete(A, b"a").unwrap());
    tx.put(A, b"", b"").unwrap();
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(read.get(A, b"a").unwrap(), None);
    assert_eq!(read.get(A, b"").unwrap(), Some(vec![]));
    assert_eq!(read.get(B, b"a").unwrap(), Some(b"other table".to_vec()));
}

#[test]
fn snapshots_and_abort() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let old = db.begin_read().unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(A, b"a", b"1").unwrap();
    tx.put(B, b"b", b"2").unwrap();
    assert_eq!(old.get(A, b"a").unwrap(), None);
    assert_eq!(db.begin_read().unwrap().get(B, b"b").unwrap(), None);
    assert_eq!(
        tx.cursor(A, b"").unwrap().next().unwrap(),
        Some((b"a".to_vec(), b"1".to_vec()))
    );
    tx.commit().unwrap();
    assert_eq!(old.get(A, b"a").unwrap(), None);
    assert_eq!(old.get(B, b"b").unwrap(), None);
    let committed = db.begin_read().unwrap();
    assert_eq!(committed.get(A, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(committed.get(B, b"b").unwrap(), Some(b"2".to_vec()));
    let mut tx = db.begin_write().unwrap();
    tx.delete(A, b"a").unwrap();
    tx.put(B, b"b", b"3").unwrap();
    tx.abort();
    {
        let mut tx = db.begin_write().unwrap();
        tx.put(A, b"a", b"4").unwrap();
        tx.delete(B, b"b").unwrap();
        // Implicit abort.
    }
    let read = db.begin_read().unwrap();
    assert_eq!(read.get(A, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(read.get(B, b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let keys = [
        vec![],
        vec![0],
        vec![0, 255],
        vec![128],
        vec![255],
        vec![255, 255],
    ];
    let mut tx = db.begin_write().unwrap();
    for k in keys.iter().rev() {
        tx.put(A, k, k).unwrap();
    }
    tx.commit().unwrap();
    let tx = db.begin_read().unwrap();
    let mut c = tx.cursor(A, b"").unwrap();
    for k in &keys {
        assert_eq!(c.next().unwrap(), Some((k.clone(), k.clone())));
    }
    assert_eq!(c.next().unwrap(), None);
    assert_eq!(c.next().unwrap(), None);
    for k in keys.iter().rev() {
        assert_eq!(c.prev().unwrap(), Some((k.clone(), k.clone())));
    }
    assert_eq!(c.prev().unwrap(), None);
    assert_eq!(c.prev().unwrap(), None);
    assert_eq!(c.next().unwrap(), Some((vec![], vec![])));

    // Key starts include equality in either initial direction; missing keys
    // choose the nearest row in the requested direction.
    for start in keys.iter().chain([vec![1], vec![255, 255, 255]].iter()) {
        let forward = tx.cursor(A, start).unwrap().next().unwrap();
        let backward = tx.cursor(A, start).unwrap().prev().unwrap();
        assert_eq!(
            forward,
            keys.iter()
                .find(|k| *k >= start)
                .map(|k| (k.clone(), k.clone()))
        );
        assert_eq!(
            backward,
            keys.iter()
                .rev()
                .find(|k| *k <= start)
                .map(|k| (k.clone(), k.clone()))
        );
    }
    let mut c = tx.cursor(A, &[1]).unwrap();
    assert_eq!(c.next().unwrap(), Some((vec![128], vec![128])));
    assert_eq!(c.prev().unwrap(), Some((vec![0, 255], vec![0, 255])));
    assert_eq!(c.next().unwrap(), Some((vec![128], vec![128])));
    let mut c = tx.cursor(A, &[128]).unwrap();
    assert_eq!(c.prev().unwrap(), Some((vec![128], vec![128])));
    assert_eq!(c.next().unwrap(), Some((vec![255], vec![255])));
    let mut c = tx.cursor(A, &[255, 255, 255]).unwrap();
    assert_eq!(c.next().unwrap(), None);
    assert_eq!(c.prev().unwrap(), Some((vec![255, 255], vec![255, 255])));
    // Start-key lifetime does not limit the cursor lifetime.
    let mut c = {
        let key = vec![128];
        tx.cursor(A, &key).unwrap()
    };
    assert_eq!(c.next().unwrap(), Some((vec![128], vec![128])));
    for start in [&b""[..], &b"x"[..], &[255][..]] {
        let mut empty = tx.cursor(B, start).unwrap();
        assert_eq!(empty.prev().unwrap(), None);
        assert_eq!(empty.next().unwrap(), None);
        assert_eq!(empty.prev().unwrap(), None);
    }
}

#[test]
fn read_cursors_survive_writes_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(A, b"a", b"old-a").unwrap();
    tx.put(A, b"b", b"old-b").unwrap();
    tx.commit().unwrap();

    let first_reader = db.begin_read().unwrap();
    let mut first_cursor = first_reader.cursor(A, b"").unwrap();
    let mut writer = db.begin_write().unwrap();
    // Opening a second reader while a writer is active is also supported.
    let second_reader = db.begin_read().unwrap();
    let mut second_cursor = second_reader.cursor(A, b"b").unwrap();
    writer.put(A, b"a", b"new-a").unwrap();
    writer.delete(A, b"b").unwrap();
    writer.put(A, b"c", b"new-c").unwrap();
    assert_eq!(
        first_cursor.next().unwrap(),
        Some((b"a".to_vec(), b"old-a".to_vec()))
    );
    assert_eq!(
        second_cursor.prev().unwrap(),
        Some((b"b".to_vec(), b"old-b".to_vec()))
    );
    writer.commit().unwrap();
    // Both cursors remain live and continue to see their original snapshots.
    assert_eq!(
        first_cursor.next().unwrap(),
        Some((b"b".to_vec(), b"old-b".to_vec()))
    );
    assert_eq!(
        second_cursor.prev().unwrap(),
        Some((b"a".to_vec(), b"old-a".to_vec()))
    );
    assert_eq!(first_cursor.next().unwrap(), None);
    let latest = db.begin_read().unwrap();
    assert_eq!(latest.get(A, b"a").unwrap(), Some(b"new-a".to_vec()));
    assert_eq!(latest.get(A, b"b").unwrap(), None);
    assert_eq!(latest.get(A, b"c").unwrap(), Some(b"new-c".to_vec()));
}

fn inside(key: &[u8], lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> bool {
    let low = match lower {
        Unbounded => true,
        Included(x) => key >= x,
        Excluded(x) => key > x,
    };
    let high = match upper {
        Unbounded => true,
        Included(x) => key <= x,
        Excluded(x) => key < x,
    };
    low && high
}

#[test]
fn ranges() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let keys = [
        vec![],
        vec![0],
        vec![0, 255],
        vec![128],
        vec![255],
        vec![255, 255],
    ];
    let mut tx = db.begin_write().unwrap();
    for k in &keys {
        tx.put(A, k, k).unwrap();
    }
    tx.commit().unwrap();
    let tx = db.begin_read().unwrap();
    let mut bounds = vec![Unbounded];
    for k in keys.iter().chain([vec![1], vec![255, 255, 255]].iter()) {
        bounds.push(Included(k.clone()));
        bounds.push(Excluded(k.clone()));
    }
    for lower in &bounds {
        for upper in &bounds {
            let result = scan(&tx, A, lower.clone(), upper.clone());
            if let (Included(a) | Excluded(a), Included(b) | Excluded(b)) = (lower, upper)
                && a > b
            {
                assert!(matches!(result, Err(StorageError::InvalidRange)));
                continue;
            }
            let mut iter = result.unwrap();
            let actual: Vec<_> = iter.by_ref().map(|r| r.unwrap().0).collect();
            let expected: Vec<_> = keys
                .iter()
                .filter(|k| inside(k, lower, upper))
                .cloned()
                .collect();
            assert_eq!(actual, expected, "{lower:?}..{upper:?}");
            assert!(iter.next().is_none());
        }
    }
    for prefix in [
        vec![],
        vec![0],
        vec![0, 255],
        vec![1],
        vec![255],
        vec![255, 255],
    ] {
        let actual: Vec<_> = scan_prefix(&tx, A, prefix.clone())
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        let expected: Vec<_> = keys
            .iter()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    // Invalid names are errors even for reads of a never-created table.
    for table in [Table(""), Table("a\0b")] {
        let read = db.begin_read().unwrap();
        assert!(matches!(
            read.get(table, b"a"),
            Err(StorageError::InvalidTableName(_))
        ));
        assert!(matches!(
            read.cursor(table, b""),
            Err(StorageError::InvalidTableName(_))
        ));
        assert!(matches!(
            scan(&read, table, Unbounded, Unbounded),
            Err(StorageError::InvalidTableName(_))
        ));
        for operation in 0..3 {
            let mut tx = db.begin_write().unwrap();
            let result = match operation {
                0 => tx.put(table, b"a", b"v"),
                1 => tx.insert(table, b"a", b"v"),
                _ => tx.delete(table, b"a").map(|_| ()),
            };
            assert!(matches!(result, Err(StorageError::InvalidTableName(_))));
            tx.abort();
        }
    }
}

#[test]
fn implicit_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let old = db.begin_read().unwrap();
    assert_eq!(old.get(A, b"key").unwrap(), None);
    assert!(
        scan(&old, A, Unbounded, Unbounded)
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        scan_prefix(&old, A, b"key".to_vec())
            .unwrap()
            .next()
            .is_none()
    );
    let mut empty = old.cursor(A, b"key").unwrap();
    assert_eq!(empty.next().unwrap(), None);
    assert_eq!(empty.prev().unwrap(), None);
    {
        let mut tx = db.begin_write().unwrap();
        assert!(!tx.delete(A, b"key").unwrap());
        tx.put(A, b"key", b"aborted").unwrap();
        assert_eq!(tx.get(A, b"key").unwrap(), Some(b"aborted".to_vec()));
        tx.abort();
    }
    {
        let mut tx = db.begin_write().unwrap();
        tx.insert(A, b"key", b"dropped").unwrap();
        // Drop also rolls back first-write table creation.
    }
    assert_eq!(db.begin_read().unwrap().get(A, b"key").unwrap(), None);
    let mut tx = db.begin_write().unwrap();
    tx.insert(A, b"key", b"A").unwrap();
    tx.put(B, b"key", b"B").unwrap();
    tx.commit().unwrap();
    assert_eq!(old.get(A, b"key").unwrap(), None);
    assert_eq!(empty.next().unwrap(), None);
    assert_eq!(empty.prev().unwrap(), None);
    let read = db.begin_read().unwrap();
    assert_eq!(read.get(A, b"key").unwrap(), Some(b"A".to_vec()));
    assert_eq!(read.get(B, b"key").unwrap(), Some(b"B".to_vec()));
    for (table, value) in [(A, b"A"), (B, b"B")] {
        let mut cursor = read.cursor(table, b"key").unwrap();
        assert_eq!(
            cursor.prev().unwrap(),
            Some((b"key".to_vec(), value.to_vec()))
        );
        assert_eq!(cursor.prev().unwrap(), None);
        assert_eq!(
            cursor.next().unwrap(),
            Some((b"key".to_vec(), value.to_vec()))
        );
        assert_eq!(cursor.next().unwrap(), None);
    }
}

#[test]
fn writers_serialize_and_see_latest_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let mut first = db.begin_write().unwrap();
    first.put(A, b"x", b"first").unwrap();
    let (starting, started) = std::sync::mpsc::channel();
    let (acquired, acquired_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            starting.send(()).unwrap();
            let mut second = db.begin_write().unwrap();
            acquired.send(()).unwrap();
            assert_eq!(second.get(A, b"x").unwrap(), Some(b"first".to_vec()));
            second.put(B, b"x", b"second").unwrap();
            second.commit().unwrap();
        });
        started.recv().unwrap();
        assert!(matches!(
            acquired_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        // Readers are not blocked by the pending writer.
        assert_eq!(db.begin_read().unwrap().get(A, b"x").unwrap(), None);
        first.commit().unwrap();
    });
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.get(A, b"x").unwrap(), Some(b"first".to_vec()));
    assert_eq!(tx.get(B, b"x").unwrap(), Some(b"second".to_vec()));
}

#[test]
fn readers_on_other_threads_continue_during_write_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let mut seed = db.begin_write().unwrap();
    seed.put(A, b"a", b"old-a").unwrap();
    seed.put(A, b"b", b"old-b").unwrap();
    seed.commit().unwrap();
    let mut writer = db.begin_write().unwrap();
    writer.put(A, b"a", b"new-a").unwrap();
    writer.delete(A, b"b").unwrap();
    let ready = std::sync::Barrier::new(4);
    let committed = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                let reader = db.begin_read().unwrap();
                let mut cursor = reader.cursor(A, b"").unwrap();
                assert_eq!(
                    cursor.next().unwrap(),
                    Some((b"a".to_vec(), b"old-a".to_vec()))
                );
                ready.wait();
                committed.wait();
                assert_eq!(
                    cursor.next().unwrap(),
                    Some((b"b".to_vec(), b"old-b".to_vec()))
                );
                assert_eq!(
                    db.begin_read().unwrap().get(A, b"a").unwrap(),
                    Some(b"new-a".to_vec())
                );
            });
        }
        ready.wait();
        writer.commit().unwrap();
        committed.wait();
    });
}

proptest! {
    #[test]
    fn transactional_edits_match_committed_model(
        batches in prop::collection::vec((prop::collection::vec((any::<u8>(), any::<u8>(), any::<bool>()), 0..30), any::<bool>()), 0..20)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = MdbxStore::open(dir.path()).unwrap();
        let mut committed = BTreeMap::new();
        for (edits, commit) in batches {
            let old = db.begin_read().unwrap();
            let old_expected = committed.clone();
            let mut expected = committed.clone();
            let mut tx = db.begin_write().unwrap();
            for (k, v, remove) in edits {
                if remove {
                    tx.delete(A, &[k]).unwrap();
                    expected.remove(&vec![k]);
                } else {
                    tx.put(A, &[k], &[v]).unwrap();
                    expected.insert(vec![k], vec![v]);
                }
            }
            let actual: BTreeMap<_, _> = scan(&tx, A, Unbounded, Unbounded)
                .unwrap()
                .collect::<Result<_>>()
                .unwrap();
            assert_eq!(actual, expected.clone());
            if commit {
                tx.commit().unwrap();
                committed = expected;
            } else {
                tx.abort();
            }
            let old_rows: BTreeMap<_, _> = scan(&old, A, Unbounded, Unbounded)
                .unwrap()
                .collect::<Result<_>>()
                .unwrap();
            assert_eq!(old_rows, old_expected);
            let read = db.begin_read().unwrap();
            let actual: BTreeMap<_, _> = scan(&read, A, Unbounded, Unbounded)
                .unwrap()
                .collect::<Result<_>>()
                .unwrap();
            assert_eq!(&actual, &committed);
        }
    }
}

#[test]
fn persistent_tables_and_native_limits() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    let max_key = vec![1; db.max_key_size()];
    assert!(db.max_value_size() >= 65536);
    tx.put(A, &max_key, b"maximum key").unwrap();
    assert_eq!(tx.get(A, &max_key).unwrap(), Some(b"maximum key".to_vec()));
    let too_long = vec![1; db.max_key_size() + 1];
    assert!(matches!(
        tx.get(A, &too_long),
        Err(StorageError::KeyTooLarge { .. })
    ));
    assert!(matches!(
        tx.cursor(A, &too_long),
        Err(StorageError::KeyTooLarge { .. })
    ));
    assert!(matches!(
        tx.delete(A, &too_long),
        Err(StorageError::KeyTooLarge { .. })
    ));
    tx.put(A, b"", b"empty key").unwrap();
    tx.put(B, b"b", &vec![7; 65536]).unwrap();
    tx.commit().unwrap();
    let mut tx = db.begin_write().unwrap();
    assert!(matches!(
        tx.put(A, &vec![0; 65536], b"too long"),
        Err(StorageError::KeyTooLarge { .. })
    ));
    tx.abort();
    drop(db);
    let db = MdbxStore::open(dir.path()).unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.get(A, b"").unwrap(), Some(b"empty key".to_vec()));
    assert_eq!(tx.get(B, b"b").unwrap(), Some(vec![7; 65536]));
}

#[test]
fn table_capacity_error_is_not_absence_and_can_be_raised_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open_with_options(
        dir.path(),
        MdbxOptions {
            max_tables: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(A, b"a", b"1").unwrap();
    tx.commit().unwrap();
    let mut tx = db.begin_write().unwrap();
    assert!(matches!(
        tx.put(B, b"b", b"2"),
        Err(StorageError::Backend(_))
    ));
    tx.abort();
    drop(db);
    let db = MdbxStore::open(dir.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(B, b"b", b"2").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.begin_read().unwrap().get(A, b"a").unwrap(),
        Some(b"1".to_vec())
    );
}

#[test]
fn map_full_aborts_all_pending_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open_with_options(
        dir.path(),
        MdbxOptions {
            max_map_size: 1024 * 1024,
            growth_step: 65536,
            ..Default::default()
        },
    )
    .unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(A, b"a", b"before failure").unwrap();
    assert!(matches!(
        tx.put(A, b"huge", &vec![0; 2 * 1024 * 1024]),
        Err(StorageError::Full)
    ));
    tx.abort();
    assert!(db.begin_read().unwrap().get(A, b"a").unwrap().is_none());
    let mut tx = db.begin_write().unwrap();
    tx.put(A, b"recovered", b"ok").unwrap();
    tx.commit().unwrap();
}
