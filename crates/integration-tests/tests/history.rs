use std::cell::RefCell;

use golemdb_history::{CommitSet, HistoryError, HistoryTables, ValueSource, changeset_key};
use golemdb_storage::{MemoryStore, ReadTransaction, StorageError, Store, Table, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;
use proptest::prelude::*;

const HISTORY: HistoryTables = HistoryTables::new(Table("History"), Table("ChangeSet"));
const LIVE: Table = Table("Live");

fn timeline(db: &impl Store) {
    let old = db.begin_read().unwrap();
    for (commit, before, after) in [
        (10, None, Some(&b"A"[..])),
        (20, Some(&b"A"[..]), Some(&b"B"[..])),
        (30, Some(&b"B"[..]), None),
        (40, None, Some(&b"C"[..])),
    ] {
        let mut tx = db.begin_write().unwrap();
        HISTORY
            .record(&mut tx, b"item", commit, before.unwrap_or_default())
            .unwrap();
        match after {
            Some(bytes) => tx.put(LIVE, b"item", bytes).unwrap(),
            None => {
                tx.delete(LIVE, b"item").unwrap();
            }
        }
        tx.commit().unwrap();
    }
    assert_eq!(
        HISTORY.resolve(&old, b"item", 0).unwrap(),
        ValueSource::Live
    );
    assert_eq!(old.get(LIVE, b"item").unwrap(), None);
    let tx = db.begin_read().unwrap();
    for (target, expected) in [
        (0, None),
        (9, None),
        (10, Some(&b"A"[..])),
        (19, Some(&b"A"[..])),
        (20, Some(&b"B"[..])),
        (29, Some(&b"B"[..])),
        (30, None),
        (39, None),
        (40, Some(&b"C"[..])),
    ] {
        assert_eq!(value_at(&tx, b"item", target), expected.map(<[u8]>::to_vec));
    }
    // The same stored zero bytes mean an absent pre-image, not a missing row.
    assert_eq!(
        tx.get(HISTORY.changeset, &changeset_key(10, b"item"))
            .unwrap(),
        Some(vec![])
    );
    assert_eq!(
        HISTORY.resolve(&tx, b"item", 0).unwrap(),
        ValueSource::Preimage(vec![])
    );
    assert_eq!(
        HISTORY.resolve(&tx, b"never", 0).unwrap(),
        ValueSource::Live
    );
}

fn value_at(tx: &impl ReadTransaction, item: &[u8], target: u64) -> Option<Vec<u8>> {
    match HISTORY.resolve(tx, item, target).unwrap() {
        ValueSource::Live => tx.get(LIVE, item).unwrap(),
        ValueSource::Preimage(bytes) => (!bytes.is_empty()).then_some(bytes),
    }
}

#[test]
fn memory_timeline() {
    timeline(&MemoryStore::new());
}

#[test]
fn mdbx_timeline_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = MdbxStore::open(dir.path()).unwrap();
        timeline(&db);
    }
    let db = MdbxStore::open(dir.path()).unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(value_at(&tx, b"item", 10), Some(b"A".to_vec()));
    assert_eq!(value_at(&tx, b"item", 30), None);
    assert_eq!(value_at(&tx, b"item", 40), Some(b"C".to_vec()));
}

fn rejected_writes(db: &impl Store) {
    let mut tx = db.begin_write().unwrap();
    // Prefixes/binary bytes remain opaque, and different items share a commit.
    for item in [b"a".as_slice(), b"a\0", b"a\0\xff"] {
        HISTORY.record(&mut tx, item, 20, item).unwrap();
    }
    tx.commit().unwrap();
    for commit in [20, 19, 0] {
        let mut tx = db.begin_write().unwrap();
        assert!(matches!(HISTORY.record(&mut tx, b"a", commit, b"wrong"),
            Err(HistoryError::NonIncreasingCommit { previous: 20, commit: c, .. }) if c == commit));
        assert_eq!(
            tx.get(HISTORY.changeset, &changeset_key(20, b"a")).unwrap(),
            Some(b"a".to_vec())
        );
        tx.abort();
    }
    let mut tx = db.begin_write().unwrap();
    // An orphan pre-image is not permission to overwrite it.
    tx.put(HISTORY.changeset, &changeset_key(30, b"orphan"), b"")
        .unwrap();
    tx.commit().unwrap();
    let mut tx = db.begin_write().unwrap();
    assert!(matches!(
        HISTORY.record(&mut tx, b"orphan", 30, b"wrong"),
        Err(HistoryError::ExistingPreimage { commit: 30, .. })
    ));
    assert_eq!(tx.get(HISTORY.history, b"orphan").unwrap(), None);
    assert_eq!(
        tx.get(HISTORY.changeset, &changeset_key(30, b"orphan"))
            .unwrap(),
        Some(vec![])
    );
    tx.abort();
}

#[test]
fn writes_never_replace_existing_preimages() {
    rejected_writes(&MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    rejected_writes(&MdbxStore::open(dir.path()).unwrap());
}

fn large_commits(db: &impl Store) {
    let mut tx = db.begin_write().unwrap();
    for (commit, before) in [
        (u32::MAX as u64, &b"a"[..]),
        (1 << 32, b"b"),
        (u64::MAX, b"c"),
    ] {
        HISTORY.record(&mut tx, b"large", commit, before).unwrap();
    }
    tx.commit().unwrap();
    let tx = db.begin_read().unwrap();
    for (target, before) in [
        (0, &b"a"[..]),
        (u32::MAX as u64, b"b"),
        (1 << 32, b"c"),
        (u64::MAX - 1, b"c"),
    ] {
        assert_eq!(
            HISTORY.resolve(&tx, b"large", target).unwrap(),
            ValueSource::Preimage(before.to_vec())
        );
    }
    assert_eq!(
        HISTORY.resolve(&tx, b"large", u64::MAX).unwrap(),
        ValueSource::Live
    );
}

#[test]
fn persisted_commits_use_the_full_u64_range() {
    large_commits(&MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    large_commits(&MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn missing_preimage_and_malformed_history_do_not_invoke_the_reader() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    HISTORY.record(&mut tx, b"item", 10, b"").unwrap();
    tx.delete(HISTORY.changeset, &changeset_key(10, b"item"))
        .unwrap();
    let result = HISTORY.with_resolved(&tx, b"item", 0, |_| panic!("missing is not absent"));
    assert!(matches!(
        result,
        Err(HistoryError::MissingPreimage { commit: 10, .. })
    ));
    for bytes in [vec![], vec![0; 8], vec![255; 12]] {
        tx.put(HISTORY.history, b"bad", &bytes).unwrap();
        let result = HISTORY.with_resolved(&tx, b"bad", 0, |_| panic!("corruption is not current"));
        assert!(matches!(result, Err(HistoryError::InvalidCommits { .. })));
        assert!(matches!(
            HISTORY.record(&mut tx, b"bad", 20, b"value"),
            Err(HistoryError::InvalidCommits { .. })
        ));
        assert_eq!(
            tx.get(HISTORY.changeset, &changeset_key(20, b"bad"))
                .unwrap(),
            None
        );
    }
}

#[test]
fn table_aliases_are_rejected_before_writing() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    let invalid = HistoryTables::new(LIVE, LIVE);
    assert!(matches!(
        invalid.record(&mut tx, b"x", 1, b"x"),
        Err(HistoryError::AliasedTables(LIVE))
    ));
    assert!(tx.is_empty().unwrap());
}

struct BorrowOnly<R> {
    inner: R,
    reads: RefCell<Vec<Table>>,
    fail: Option<Table>,
}

impl<R: ReadTransaction> ReadTransaction for BorrowOnly<R> {
    type Cursor<'a>
        = R::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, _: Table, _: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        panic!("history reads must use borrowed inspection")
    }
    fn with_value<T>(
        &self,
        table: Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> golemdb_storage::Result<T> {
        self.reads.borrow_mut().push(table);
        if self.fail == Some(table) {
            return Err(StorageError::Full);
        }
        self.inner.with_value(table, key, inspect)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.inner.cursor(table, key)
    }
}

#[test]
fn borrowed_reads_and_scan_reuse_do_not_fetch_extra_rows() {
    let db = MemoryStore::new();
    let mut tx = db.begin_write().unwrap();
    HISTORY.record(&mut tx, b"item", 10, &[7; 100_000]).unwrap();
    tx.commit().unwrap();
    let mut tx = BorrowOnly {
        inner: db.begin_read().unwrap(),
        reads: RefCell::new(vec![]),
        fail: None,
    };
    let length = HISTORY
        .with_resolved(&tx, b"item", 9, |value| match value {
            ValueSource::Preimage(bytes) => bytes.len(),
            ValueSource::Live => panic!("expected pre-image"),
        })
        .unwrap();
    assert_eq!(length, 100_000);
    assert_eq!(*tx.reads.borrow(), [HISTORY.history, HISTORY.changeset]);
    tx.reads.borrow_mut().clear();
    assert_eq!(
        HISTORY.resolve(&tx, b"item", 10).unwrap(),
        ValueSource::Live
    );
    assert_eq!(*tx.reads.borrow(), [HISTORY.history]);
    let commits = HISTORY.read_commits(&tx, b"item").unwrap();
    tx.reads.borrow_mut().clear();
    let commit = commits.first_after(0).unwrap();
    assert_eq!(
        HISTORY
            .with_preimage(&tx, b"item", commit, <[u8]>::len)
            .unwrap(),
        100_000
    );
    assert_eq!(*tx.reads.borrow(), [HISTORY.changeset]);
    for table in [HISTORY.history, HISTORY.changeset] {
        tx.fail = Some(table);
        assert!(matches!(
            HISTORY.with_resolved(&tx, b"item", 0, |_| panic!("failed read")),
            Err(HistoryError::Storage(StorageError::Full))
        ));
    }
}

struct FailWrite<W> {
    inner: W,
    writes: usize,
    fail_at: usize,
}

impl<W: ReadTransaction> ReadTransaction for FailWrite<W> {
    type Cursor<'a>
        = W::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        self.inner.get(table, key)
    }
    fn with_value<T>(
        &self,
        table: Table,
        key: &[u8],
        inspect: impl FnOnce(Option<&[u8]>) -> T,
    ) -> golemdb_storage::Result<T> {
        self.inner.with_value(table, key, inspect)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.inner.cursor(table, key)
    }
}

impl<W: WriteTransaction> FailWrite<W> {
    fn before_write(&mut self) -> golemdb_storage::Result<()> {
        self.writes += 1;
        if self.writes == self.fail_at {
            Err(StorageError::Full)
        } else {
            Ok(())
        }
    }
}

impl<W: WriteTransaction> WriteTransaction for FailWrite<W> {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.before_write()?;
        self.inner.put(table, key, value)
    }
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.before_write()?;
        self.inner.insert(table, key, value)
    }
    fn delete(&mut self, table: Table, key: &[u8]) -> golemdb_storage::Result<bool> {
        self.inner.delete(table, key)
    }
    fn commit(self) -> golemdb_storage::Result<()> {
        self.inner.commit()
    }
}

fn aborts(db: &impl Store) {
    let mut tx = db.begin_write().unwrap();
    HISTORY.record(&mut tx, b"item", 10, b"origin").unwrap();
    tx.commit().unwrap();
    let old = db.begin_read().unwrap();
    for fail_at in [1, 2] {
        let mut tx = FailWrite {
            inner: db.begin_write().unwrap(),
            writes: 0,
            fail_at,
        };
        assert!(matches!(
            HISTORY.record(&mut tx, b"item", 20, b"before20"),
            Err(HistoryError::Storage(StorageError::Full))
        ));
        tx.abort();
        let tx = db.begin_read().unwrap();
        assert_eq!(
            HISTORY.read_commits(&tx, b"item").unwrap().latest(),
            Some(10)
        );
        assert_eq!(
            tx.get(HISTORY.changeset, &changeset_key(20, b"item"))
                .unwrap(),
            None
        );
        assert_eq!(
            HISTORY.resolve(&tx, b"item", 0).unwrap(),
            ValueSource::Preimage(b"origin".to_vec())
        );
    }
    {
        let mut tx = db.begin_write().unwrap();
        HISTORY.record(&mut tx, b"item", 20, b"before20").unwrap();
        // Dropping the writer aborts both successful writes too.
    }
    let tx = db.begin_read().unwrap();
    assert_eq!(
        HISTORY.read_commits(&tx, b"item").unwrap().latest(),
        Some(10)
    );
    assert_eq!(
        HISTORY.read_commits(&old, b"item").unwrap().latest(),
        Some(10)
    );
}

#[test]
fn failures_between_writes_abort_both_tables() {
    aborts(&MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    aborts(&MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn changeset_keys_keep_commit_and_complete_item_order() {
    assert_eq!(
        changeset_key(1, b"a\0"),
        [vec![0, 0, 0, 0, 0, 0, 0, 1], b"a\0".to_vec()].concat()
    );
    assert!(changeset_key(255, b"z") < changeset_key(256, b"a"));
    assert!(changeset_key(10, b"a") < changeset_key(10, b"a\0"));
}

proptest! {
    #[test]
    fn historical_values_match_saved_snapshots(operations in prop::collection::vec((0u8..3, prop::option::of(1u8..10)), 1..60)) {
        let db = MemoryStore::new();
        let mut state = [None; 3];
        let mut snapshots = vec![state];
        let mut events = [Vec::new(), Vec::new(), Vec::new()];
        for (i, &(item, after)) in operations.iter().enumerate() {
            let commit = (i as u64 + 1) * 3;
            let before = state[item as usize];
            let mut tx = db.begin_write().unwrap();
            if before != after {
                HISTORY.record(&mut tx, &[item], commit, &before.into_iter().collect::<Vec<_>>()).unwrap();
                match after {
                    Some(value) => tx.put(LIVE, &[item], &[value]).unwrap(),
                    None => { tx.delete(LIVE, &[item]).unwrap(); }
                }
                events[item as usize].push(commit);
            }
            state[item as usize] = after;
            tx.commit().unwrap();
            snapshots.push(state);
        }
        let tx = db.begin_read().unwrap();
        for target in 0..=(operations.len() as u64 * 3) {
            for item in 0u8..3 {
                let expected = snapshots[target as usize / 3][item as usize].map(|v| vec![v]);
                prop_assert_eq!(value_at(&tx, &[item], target), expected);
            }
        }
        for item in 0u8..3 {
            prop_assert_eq!(HISTORY.read_commits(&tx, &[item]).unwrap(), events[item as usize].iter().copied().collect::<CommitSet>());
        }
    }
}
