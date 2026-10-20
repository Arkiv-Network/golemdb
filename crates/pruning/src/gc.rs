use std::{cmp::Reverse, collections::BinaryHeap};

use golemdb_storage::{ReadCursor, ReadTransaction, StorageError, Table, WriteTransaction};

use crate::{PruningConfig, PruningError, Result};

/// Superseded rows awaiting deletion: each commit appends its sorted list as
/// `commit ‖ chunk` rows of up to [`PruningConfig::with_chunk`] entries, each
/// `table ‖ key`.
pub const STALE: Table = Table("Stale");

/// The versioned tables, by their code in `Stale` entries. The codes are
/// persisted: append only, never reorder.
const TABLES: [Table; 4] = [
    golemdb_cells::tables::CELL_TRIE,
    golemdb_index::tables::INDEX_TRIE,
    golemdb_index::tables::BITMAP_TRIE,
    golemdb_index::tables::BITMAP_CONTAINER,
];

/// Longest key an entry can list: `table ‖ key` must fit its `u16` length.
const MAX_KEY: usize = u16::MAX as usize - 1;

/// Fewest rows a commit may delete, so a quiet commit still makes progress.
const MIN_DELETES: usize = 1024;

/// Why a sweep stopped early: an error, or a `Stale` row it could not read.
enum SweepError {
    Failed(PruningError),
    Unreadable(Vec<u8>),
}

impl<E: Into<PruningError>> From<E> for SweepError {
    fn from(error: E) -> Self {
        Self::Failed(error.into())
    }
}

/// Record the rows `commit` supersedes and delete due ones, in the commit's own
/// transaction. An unreadable `Stale` row never fails the commit: it is deleted,
/// losing only the rows it listed, and the sweep goes on.
pub(crate) fn collect<'a>(
    tx: &mut impl WriteTransaction,
    gc: &PruningConfig,
    commit: u64,
    superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
) -> Result<()> {
    let mut entries = (superseded.into_iter())
        .map(|(table, key)| {
            if key.len() > MAX_KEY {
                return Err(PruningError::KeyTooLong { len: key.len() });
            }
            Ok([&[code(table)?][..], key].concat())
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_unstable();
    for (index, chunk) in entries.chunks(gc.chunk).enumerate() {
        let (key, value) = (chunk_key(commit, index as u32), encode(chunk));
        // A stray row sorting after this commit's must not block recording.
        match tx.append(STALE, &key, &value) {
            Err(StorageError::OutOfOrder | StorageError::AlreadyExists) => {
                tx.put(STALE, &key, &value)?
            }
            result => result?,
        }
    }
    let quota = (2 * entries.len()).max(MIN_DELETES).min(gc.max_deletes);
    let mut deleted = 0;
    loop {
        match sweep(tx, gc, commit, quota, &mut deleted) {
            Ok(()) => return Ok(()),
            Err(SweepError::Unreadable(key)) => {
                tx.delete(STALE, &key)?;
            }
            Err(SweepError::Failed(error)) => return Err(error),
        }
    }
}

/// Delete up to `quota` rows, counting in `deleted`, of epochs (`batch`
/// consecutive commits) whose last commit is at or below `head − window`. A row
/// superseded at `s` is reached only by roots before `s`, which have all left
/// the window. An epoch's lists are merged in `(table, key)` order, so rows on
/// the same page are deleted one after another.
fn sweep(
    tx: &mut impl WriteTransaction,
    gc: &PruningConfig,
    head: u64,
    quota: usize,
    deleted: &mut usize,
) -> std::result::Result<(), SweepError> {
    let due = head.saturating_add(1).saturating_sub(gc.window) / gc.batch;
    while *deleted < quota {
        let Some((first, _)) = tx.cursor(STALE, b"")?.next()? else {
            break;
        };
        let epoch = leading_u64(&first).ok_or(SweepError::Unreadable(first))? / gc.batch;
        if epoch >= due {
            break;
        }
        let slice = merge_epoch(tx, gc, epoch, quota - *deleted)?;
        for entry in &slice.entries {
            // A key the store cannot hold names no row.
            match tx.delete(TABLES[entry[0] as usize], &entry[1..]) {
                Ok(_) | Err(StorageError::KeyTooLarge { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        for key in &slice.consumed {
            tx.delete(STALE, key)?;
        }
        for (key, rest) in &slice.partial {
            tx.put(STALE, key, rest)?;
        }
        *deleted += slice.entries.len();
    }
    Ok(())
}

/// One commit's list during a merge: its current chunk and position in it.
struct Run<C> {
    commit: u64,
    cursor: C,
    /// The commit's first row, already read while finding the commit.
    first: Option<(Vec<u8>, Vec<u8>)>,
    key: Vec<u8>,
    entries: Vec<Vec<u8>>,
    next: usize,
}

impl<C: ReadCursor> Run<C> {
    /// Load the run's next chunk; `false` once the commit's list is done.
    fn load(&mut self) -> std::result::Result<bool, SweepError> {
        let row = match self.first.take() {
            Some(row) => Some(row),
            None => self.cursor.next()?,
        };
        match row {
            Some((key, value)) if leading_u64(&key) == Some(self.commit) => {
                let Some(entries) = decode(&value) else {
                    return Err(SweepError::Unreadable(key));
                };
                self.entries = entries;
                self.key = key;
                self.next = 0;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// One slice of a due epoch: the rows to delete, and what becomes of the
/// `Stale` chunks the merge read.
struct Slice {
    /// `table ‖ key` of each row to delete, in merge order.
    entries: Vec<Vec<u8>>,
    /// `Stale` chunks fully used up, to delete.
    consumed: Vec<Vec<u8>>,
    /// `Stale` chunks partly used up, with the entries left to rewrite.
    partial: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Up to `limit` entries of `epoch`, merged across its commits' lists in
/// `(table, key)` order. Only commits with rows are visited: each seek lands on
/// the next one.
fn merge_epoch(
    tx: &impl ReadTransaction,
    gc: &PruningConfig,
    epoch: u64,
    limit: usize,
) -> std::result::Result<Slice, SweepError> {
    let mut runs = Vec::new();
    let mut heads = BinaryHeap::new();
    let end = epoch.saturating_add(1).saturating_mul(gc.batch);
    let mut next = epoch.saturating_mul(gc.batch);
    while next < end {
        let mut cursor = tx.cursor(STALE, &next.to_be_bytes())?;
        let Some((key, value)) = cursor.next()? else {
            break;
        };
        let Some(commit) = leading_u64(&key) else {
            return Err(SweepError::Unreadable(key));
        };
        if commit >= end {
            break;
        }
        let mut run = Run {
            commit,
            cursor,
            first: Some((key, value)),
            key: Vec::new(),
            entries: Vec::new(),
            next: 0,
        };
        if run.load()? {
            heads.push(Reverse((run.entries[0].clone(), runs.len())));
            runs.push(run);
        }
        let Some(after) = commit.checked_add(1) else {
            break;
        };
        next = after;
    }
    let (mut entries, mut consumed, mut partial) = (Vec::new(), Vec::new(), Vec::new());
    while entries.len() < limit {
        let Some(Reverse((entry, index))) = heads.pop() else {
            break;
        };
        entries.push(entry);
        let run = &mut runs[index];
        run.next += 1;
        if run.next == run.entries.len() {
            consumed.push(run.key.clone());
            if !run.load()? {
                continue;
            }
        }
        heads.push(Reverse((run.entries[run.next].clone(), index)));
    }
    for Reverse((_, index)) in heads {
        let run = &runs[index];
        if run.next > 0 {
            partial.push((run.key.clone(), encode(&run.entries[run.next..])));
        }
    }
    Ok(Slice {
        entries,
        consumed,
        partial,
    })
}

fn chunk_key(commit: u64, index: u32) -> Vec<u8> {
    [&commit.to_be_bytes()[..], &index.to_be_bytes()].concat()
}

fn encode(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend_from_slice(&(entry.len() as u16).to_be_bytes());
        bytes.extend_from_slice(entry);
    }
    bytes
}

/// `None` unless `bytes` is a nonempty list of entries naming versioned tables.
fn decode(mut bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut entries = Vec::new();
    while let [high, low, rest @ ..] = bytes {
        let (entry, tail) = rest.split_at_checked(u16::from_be_bytes([*high, *low]) as usize)?;
        if entry
            .first()
            .is_none_or(|&code| code as usize >= TABLES.len())
        {
            return None;
        }
        entries.push(entry.to_vec());
        bytes = tail;
    }
    (bytes.is_empty() && !entries.is_empty()).then_some(entries)
}

/// The commit a `Stale` key starts with.
fn leading_u64(key: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(key.get(..8)?.try_into().unwrap()))
}

fn code(table: Table) -> Result<u8> {
    let code = TABLES.iter().position(|versioned| *versioned == table);
    Ok(code.ok_or(PruningError::UnversionedTable(table.0))? as u8)
}

#[cfg(test)]
mod tests {
    use golemdb_cells::tables::CELL_TRIE;
    use golemdb_storage::{MemoryStore, ReadTransaction, Store, WriteTransaction, scan_prefix};

    use super::{STALE, code, decode, encode};
    use crate::{Pruner, PruningConfig};

    /// Commit `c` writes a few rows and supersedes those of `c − 1`. After every
    /// commit, a row is gone only once its epoch is due, and (when the cap
    /// allows) every due row is gone.
    fn run(window: u64, batch: u64, max_deletes: usize, chunk: usize) {
        let db = MemoryStore::new();
        let pruner = Pruner::new(
            PruningConfig::new(window)
                .with_batch(batch)
                .with_max_deletes(max_deletes)
                .with_chunk(chunk),
        );
        let (mut written, mut superseded) = (Vec::new(), Vec::new());
        for commit in 1..=200u64 {
            let mut tx = db.begin_write().unwrap();
            let rows: Vec<Vec<u8>> = if commit <= 40 {
                (0..commit % 5)
                    .map(|i| [&commit.to_be_bytes()[..], &[i as u8]].concat())
                    .collect()
            } else {
                Vec::new()
            };
            for row in &rows {
                tx.put(CELL_TRIE, row, b"node").unwrap();
            }
            let old = std::mem::replace(&mut written, rows);
            pruner
                .prune(
                    &mut tx,
                    commit,
                    old.iter().map(|row| (CELL_TRIE, row.as_slice())),
                )
                .unwrap();
            superseded.extend(old.into_iter().map(|row| (commit, row)));
            tx.commit().unwrap();

            let tx = db.begin_read().unwrap();
            let due = (commit + 1).saturating_sub(window) / batch;
            for (at, row) in &superseded {
                let stored = tx.get(CELL_TRIE, row).unwrap().is_some();
                if at / batch >= due {
                    assert!(stored, "row superseded at {at} deleted at head {commit}");
                } else if max_deletes >= 1024 {
                    assert!(
                        !stored,
                        "row superseded at {at} still stored at head {commit}"
                    );
                }
            }
            for row in &written {
                assert!(tx.get(CELL_TRIE, row).unwrap().is_some());
            }
        }
        let tx = db.begin_read().unwrap();
        let due = 201u64.saturating_sub(window) / batch;
        for (at, row) in &superseded {
            assert_eq!(tx.get(CELL_TRIE, row).unwrap().is_some(), at / batch >= due);
        }
        let stale = scan_prefix(&tx, STALE, vec![]).unwrap().count();
        assert_eq!(
            stale == 0,
            superseded.iter().all(|(at, _)| at / batch < due)
        );
    }

    #[test]
    fn table_codes_are_pinned_and_unversioned_tables_are_rejected() {
        use golemdb_index::tables::{BITMAP_CONTAINER, BITMAP_TRIE, INDEX, INDEX_TRIE};
        let codes =
            [CELL_TRIE, INDEX_TRIE, BITMAP_TRIE, BITMAP_CONTAINER].map(|t| code(t).unwrap());
        assert_eq!(codes, [0, 1, 2, 3]);
        assert!(matches!(
            code(INDEX),
            Err(crate::PruningError::UnversionedTable("Index"))
        ));
        // The codes follow the crates' own lists of versioned tables.
        let owned = [
            &golemdb_cells::tables::VERSIONED[..],
            &golemdb_index::tables::VERSIONED,
        ]
        .concat();
        assert_eq!(super::TABLES.as_slice(), owned.as_slice());
    }

    #[test]
    fn rows_are_deleted_once_their_epoch_leaves_the_window() {
        for (window, batch) in [(0, 1), (3, 1), (3, 4), (10, 7)] {
            for chunk in [1, 2, 1000] {
                run(window, batch, 100_000, chunk);
                run(window, batch, 1, chunk);
            }
        }
    }

    /// Unreadable `Stale` rows never fail a commit: they are dropped, and the
    /// readable rows around them are still deleted in the same commits.
    #[test]
    fn malformed_stale_rows_are_dropped_and_the_rest_still_swept() {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(0).with_batch(1));
        let mut tx = db.begin_write().unwrap();
        let (live, dead) = (b"dead-at-1".to_vec(), b"dead-at-2".to_vec());
        tx.put(CELL_TRIE, &live, b"node").unwrap();
        tx.put(CELL_TRIE, &dead, b"node").unwrap();
        // Short keys sorting first and last, an unknown table code, and a
        // truncated list.
        tx.put(STALE, &[0], b"x").unwrap();
        tx.put(STALE, &[0, 1], b"x").unwrap();
        tx.put(STALE, &super::chunk_key(0, 0), &[0, 2, 9, 1])
            .unwrap();
        tx.put(STALE, &super::chunk_key(0, 1), &[0, 9, 0]).unwrap();
        tx.commit().unwrap();
        for (commit, row) in [(1, &live), (2, &dead)] {
            let mut tx = db.begin_write().unwrap();
            pruner
                .prune(&mut tx, commit, [(CELL_TRIE, row.as_slice())])
                .unwrap();
            tx.commit().unwrap();
        }
        let mut tx = db.begin_write().unwrap();
        pruner.prune(&mut tx, 3, []).unwrap();
        tx.commit().unwrap();
        let tx = db.begin_read().unwrap();
        assert_eq!(scan_prefix(&tx, STALE, vec![]).unwrap().count(), 0);
        assert_eq!(scan_prefix(&tx, CELL_TRIE, vec![]).unwrap().count(), 0);
    }

    /// A key whose entry would overflow its length is refused before anything
    /// is written; the longest one that fits is listed.
    #[test]
    fn keys_too_long_to_list_are_refused() {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(10));
        let mut tx = db.begin_write().unwrap();
        let fits = vec![1; super::MAX_KEY];
        pruner
            .prune(&mut tx, 1, [(CELL_TRIE, fits.as_slice())])
            .unwrap();
        let long = vec![2; super::MAX_KEY + 1];
        assert!(matches!(
            pruner.prune(&mut tx, 2, [(CELL_TRIE, long.as_slice())]),
            Err(crate::PruningError::KeyTooLong { len }) if len == long.len()
        ));
        let rows: Vec<_> = (scan_prefix(&tx, STALE, vec![]).unwrap())
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(decode(&rows[0].1), Some(vec![[&[0][..], &fits].concat()]));
    }

    /// An entry naming a key the store cannot hold is skipped, not fatal.
    #[test]
    fn an_entry_the_store_cannot_delete_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
        let pruner = Pruner::new(PruningConfig::new(0).with_batch(1));
        let huge = vec![7; 60_000];
        let mut tx = db.begin_write().unwrap();
        pruner
            .prune(&mut tx, 1, [(CELL_TRIE, huge.as_slice())])
            .unwrap();
        tx.commit().unwrap();
        let mut tx = db.begin_write().unwrap();
        pruner.prune(&mut tx, 2, []).unwrap();
        tx.commit().unwrap();
        let tx = db.begin_read().unwrap();
        assert_eq!(scan_prefix(&tx, STALE, vec![]).unwrap().count(), 0);
    }

    #[test]
    fn lists_round_trip_and_malformed_ones_are_rejected() {
        let entries = vec![vec![0, 1, 2], vec![3, 9]];
        assert_eq!(decode(&encode(&entries)), Some(entries));
        for bad in [
            &[][..],
            &[0, 9, 0, 1],
            &[0, 2, 7, 1],
            &[0, 0],
            &[0, 1, 0, 5],
        ] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }
}
