use crate::{DEFAULT_MAX_MAP_SIZE, DEFAULT_PAGE_SIZE, MdbxOptions, MdbxStore};
use golemdb_storage::{ReadTransaction, Store, StoreStats, Table, WriteTransaction};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// The page size MDBX uses for this database.
fn page_size(db: &MdbxStore) -> usize {
    db.inner.stat().unwrap().page_size() as usize
}

/// The address space MDBX reserved for this database.
fn map_size(db: &MdbxStore) -> usize {
    db.inner.info().unwrap().map_size()
}

#[test]
fn default_page_size_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    assert_eq!(page_size(&db), DEFAULT_PAGE_SIZE);
}

#[test]
fn configured_page_size_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let options = MdbxOptions {
        page_size: 16 * KIB,
        txn_dp_limit: None,
        ..Default::default()
    };
    let db = MdbxStore::open_with_options(dir.path(), options).unwrap();
    assert_eq!(page_size(&db), 16 * KIB);
}

#[test]
fn reopened_database_keeps_its_page_size() {
    let dir = tempfile::tempdir().unwrap();
    let options = MdbxOptions {
        page_size: 16 * KIB,
        txn_dp_limit: None,
        ..Default::default()
    };
    drop(MdbxStore::open_with_options(dir.path(), options).unwrap());
    let db = MdbxStore::open(dir.path()).unwrap();
    assert_eq!(page_size(&db), 16 * KIB);
}

#[test]
fn default_map_ceiling_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    assert_eq!(map_size(&db), DEFAULT_MAX_MAP_SIZE);
}

#[test]
fn configured_map_ceiling_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let options = MdbxOptions {
        max_map_size: 64 * MIB,
        ..Default::default()
    };
    let db = MdbxStore::open_with_options(dir.path(), options).unwrap();
    assert_eq!(map_size(&db), 64 * MIB);
}

/// The storage stats MDBX reports for this database.
fn stats(db: &MdbxStore) -> StoreStats {
    db.stats().unwrap().unwrap()
}

/// Rows `first..first + count`, alternating between two tables. Keys are
/// bit-reversed so that, like hashed keys, they spread across pages.
fn rows(first: u64, count: u64) -> impl Iterator<Item = (Table, [u8; 8])> {
    (first..first + count).map(|n| {
        let table = if n % 2 == 0 {
            Table("Even")
        } else {
            Table("Odd")
        };
        (table, n.reverse_bits().to_be_bytes())
    })
}

#[test]
fn commit_writes_pages_grows_size_and_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let before = stats(&db);
    let mut tx = db.begin_write().unwrap();
    for (table, key) in rows(0, 200) {
        tx.put(table, &key, &[7; 300]).unwrap();
    }
    stats(&db); // Readable while the writer is active.
    tx.commit().unwrap();
    let after = stats(&db);
    let written = after.session_bytes_written - before.session_bytes_written;
    assert!(written > 0);
    assert_eq!(written % DEFAULT_PAGE_SIZE as u64, 0);
    assert!(after.db_size > before.db_size);
    assert!(after.session_syncs > before.session_syncs);
}

#[test]
fn reads_and_empty_commits_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put(Table("Example"), b"key", b"value").unwrap();
    tx.commit().unwrap();
    let before = stats(&db);
    db.begin_read()
        .unwrap()
        .get(Table("Example"), b"key")
        .unwrap();
    db.begin_write().unwrap().commit().unwrap();
    assert_eq!(stats(&db), before);
}

#[test]
fn same_writes_give_same_stats() {
    let run = || {
        let dir = tempfile::tempdir().unwrap();
        let db = MdbxStore::open(dir.path()).unwrap();
        // Fill both tables, then empty them so that pages merge.
        for batch in 0..10 {
            let mut tx = db.begin_write().unwrap();
            if batch < 5 {
                for (table, key) in rows(batch * 300, 300) {
                    tx.put(table, &key, &[7; 300]).unwrap();
                }
            } else {
                for (table, key) in rows((batch - 5) * 300, 300) {
                    assert!(tx.delete(table, &key).unwrap());
                }
            }
            tx.commit().unwrap();
        }
        stats(&db)
    };
    assert_eq!(run(), run());
}

/// Pages of `after` that differ from `before`, where `before` reads as zeros
/// beyond its end.
fn changed_pages(before: &[u8], after: &[u8], page: usize) -> u64 {
    let zeros = vec![0; page];
    after
        .chunks(page)
        .enumerate()
        .filter(|&(i, new)| before.get(i * page..(i + 1) * page).unwrap_or(&zeros) != new)
        .count() as u64
}

#[test]
fn bytes_written_match_changed_pages() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    let file = dir.path().join("mdbx.dat");
    for batch in 0..9 {
        let (old, before) = (std::fs::read(&file).unwrap(), stats(&db));
        let mut tx = db.begin_write().unwrap();
        // Fill both tables, then shrink them so that pages merge. Emptying them
        // would free copied pages, which the estimate counts but MDBX never writes.
        if batch < 5 {
            for (table, key) in rows(batch * 300, 300) {
                tx.put(table, &key, &[7; 300]).unwrap();
            }
        } else {
            for (table, key) in rows((batch - 5) * 300, 300) {
                tx.delete(table, &key).unwrap();
            }
        }
        tx.commit().unwrap();
        // MDBX stamps each page it writes with the transaction ID, so every
        // written page changes, including one meta page per commit.
        let changed = changed_pages(&old, &std::fs::read(&file).unwrap(), DEFAULT_PAGE_SIZE);
        let estimated = (stats(&db).session_bytes_written - before.session_bytes_written)
            / DEFAULT_PAGE_SIZE as u64;
        assert!(
            changed.abs_diff(estimated) <= 1,
            "batch {batch}: {changed} pages changed, {estimated} estimated"
        );
    }
}

#[test]
fn pages_flushed_early_and_changed_again_count_twice() {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    // Leave room for only 128 changed pages, so MDBX flushes pages early.
    // SAFETY: the environment remains open; the option takes a plain integer.
    let code = unsafe {
        mdbx_sys::mdbx_env_set_option(db.inner.ptr().0, mdbx_sys::MDBX_opt_txn_dp_limit, 128)
    };
    assert_eq!(code, 0);
    let before = stats(&db);
    let mut tx = db.begin_write().unwrap();
    // The second pass changes flushed pages again, so they are written twice.
    for value in [7, 8] {
        for (table, key) in rows(0, 2000) {
            tx.put(table, &key, &[value; 300]).unwrap();
        }
    }
    tx.commit().unwrap();
    let after = stats(&db);
    // Otherwise a new database writes about as many bytes as it grows.
    assert!(
        after.session_bytes_written - before.session_bytes_written
            > 2 * (after.db_size - before.db_size)
    );
}
