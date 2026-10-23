use golemdb_storage::{ReadCursor, ReadTransaction, Table, WriteTransaction};

use crate::{PruningError, Result};

/// Superseded rows awaiting deletion, one `commit ‖ table ‖ key` row each with
/// an empty value; `commit` is the one that superseded the row.
pub const STALE: Table = Table("Stale");

/// The versioned tables, by their code in `Stale` keys. The codes are
/// persisted: append only, never reorder.
const TABLES: [Table; 4] = [
    golemdb_cells::tables::CELL_TRIE,
    golemdb_index::tables::INDEX_TRIE,
    golemdb_index::tables::BITMAP_TRIE,
    golemdb_index::tables::BITMAP_CONTAINER,
];

/// List the rows `commit` superseded; returns how many.
pub(crate) fn record<'a>(
    tx: &mut impl WriteTransaction,
    commit: u64,
    superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
) -> Result<usize> {
    let mut keys = (superseded.into_iter())
        .map(|(table, key)| Ok([&commit.to_be_bytes()[..], &[code(table)?], key].concat()))
        .collect::<Result<Vec<_>>>()?;
    keys.sort_unstable();
    for key in &keys {
        tx.put(STALE, key, &[])?;
    }
    Ok(keys.len())
}

/// The due epoch being drained: its `Stale` keys sorted by table and key, how
/// many of them are deleted, and the head of the commit that last deleted some.
#[derive(Debug)]
pub(crate) struct Drain {
    epoch: u64,
    keys: Vec<Vec<u8>>,
    done: usize,
    head: u64,
}

/// Delete the next `slice` rows of the oldest due epoch, in table and key
/// order, with their `Stale` rows. An epoch of `epoch_commits` commits is due
/// once its last commit is at or before `cutoff`. `drain` caches the sorted
/// epoch across commits; it is rebuilt from `Stale` unless it was last used by
/// the commit before `head`, as one that rolled back leaves it ahead of
/// `Stale`. Unparsable `Stale` rows are dropped.
pub(crate) fn sweep(
    tx: &mut impl WriteTransaction,
    drain: &mut Option<Drain>,
    head: u64,
    cutoff: u64,
    epoch_commits: u64,
    slice: usize,
) -> Result<()> {
    let Some((first, _)) = tx.cursor(STALE, b"")?.next()? else {
        *drain = None;
        return Ok(());
    };
    let Some(epoch) = commit(&first).map(|commit| commit / epoch_commits) else {
        tx.delete(STALE, &first)?;
        return Ok(());
    };
    let last = (epoch * epoch_commits).saturating_add(epoch_commits - 1);
    if last > cutoff {
        return Ok(());
    }
    let mut current = match drain.take() {
        Some(cached) if cached.epoch == epoch && cached.head.checked_add(1) == Some(head) => cached,
        _ => load(tx, &first, epoch, epoch_commits)?,
    };
    let end = (current.done + slice).min(current.keys.len());
    for key in &current.keys[current.done..end] {
        if let Some((table, row)) = row(key) {
            tx.delete(table, row)?;
        }
        tx.delete(STALE, key)?;
    }
    current.done = end;
    current.head = head;
    if end < current.keys.len() {
        *drain = Some(current);
    }
    Ok(())
}

/// The epoch's `Stale` keys from `first` on, sorted by table and key.
fn load(tx: &impl ReadTransaction, first: &[u8], epoch: u64, epoch_commits: u64) -> Result<Drain> {
    let mut keys = Vec::new();
    let mut cursor = tx.cursor(STALE, first)?;
    while let Some((key, _)) = cursor.next()? {
        if commit(&key).is_some_and(|commit| commit / epoch_commits != epoch) {
            break;
        }
        keys.push(key);
    }
    keys.sort_unstable_by(|a, b| a.get(8..).cmp(&b.get(8..)));
    Ok(Drain {
        epoch,
        keys,
        done: 0,
        head: 0,
    })
}

/// The commit a `Stale` key starts with.
fn commit(key: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(*key.first_chunk::<8>()?))
}

/// The table and key of the row a `Stale` key lists.
fn row(key: &[u8]) -> Option<(Table, &[u8])> {
    let (code, row) = key.get(8..)?.split_first()?;
    Some((*TABLES.get(*code as usize)?, row))
}

fn code(table: Table) -> Result<u8> {
    let code = TABLES.iter().position(|versioned| *versioned == table);
    Ok(code.ok_or(PruningError::UnversionedTable(table.0))? as u8)
}

#[cfg(test)]
mod tests {
    use golemdb_cells::tables::CELL_TRIE;
    use golemdb_storage::{MemoryStore, ReadTransaction, Store, WriteTransaction, scan_prefix};

    use super::{Drain, STALE, code, record, sweep};
    use crate::{Pruner, PruningConfig};

    fn keys_of(tx: &impl ReadTransaction, table: golemdb_storage::Table) -> Vec<Vec<u8>> {
        (scan_prefix(tx, table, vec![]).unwrap())
            .map(|row| row.unwrap().0)
            .collect()
    }

    /// Commit `c` writes a few rows and supersedes those of `c − 1`. After every
    /// commit, no row is gone before its epoch is due, and with a slice larger
    /// than any epoch every due row is gone.
    fn run(window: u64, epoch_commits: u64, slice: usize) {
        let db = MemoryStore::new();
        let mut drain: Option<Drain> = None;
        let due =
            |at: u64, head: u64| (at / epoch_commits + 1) * epoch_commits - 1 + window <= head;
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
            let listed = old.iter().map(|row| (CELL_TRIE, row.as_slice()));
            record(&mut tx, commit, listed).unwrap();
            let cutoff = commit.saturating_sub(window);
            sweep(&mut tx, &mut drain, commit, cutoff, epoch_commits, slice).unwrap();
            superseded.extend(old.into_iter().map(|row| (commit, row)));
            tx.commit().unwrap();

            let tx = db.begin_read().unwrap();
            for (at, row) in &superseded {
                let stored = tx.get(CELL_TRIE, row).unwrap().is_some();
                if !due(*at, commit) {
                    assert!(stored, "row superseded at {at} deleted at head {commit}");
                } else if slice >= 1000 {
                    assert!(!stored, "row superseded at {at} kept at head {commit}");
                }
            }
            for row in &written {
                assert!(tx.get(CELL_TRIE, row).unwrap().is_some());
            }
        }
        let tx = db.begin_read().unwrap();
        for (at, row) in &superseded {
            assert_eq!(tx.get(CELL_TRIE, row).unwrap().is_some(), !due(*at, 200));
        }
    }

    #[test]
    fn rows_are_deleted_once_their_epoch_leaves_the_window() {
        for window in [0, 1, 3, 10] {
            for epoch_commits in [1, 4, 7] {
                for slice in [1, 3, 1000] {
                    run(window, epoch_commits, slice);
                }
            }
        }
    }

    /// An epoch's rows are deleted in table and key order across its commits,
    /// a slice at a time.
    #[test]
    fn a_due_epoch_drains_in_key_order() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        for (commit, rows) in [(0, [[2], [5]].as_slice()), (1, &[[1], [4]]), (2, &[[3]])] {
            for row in rows {
                tx.put(CELL_TRIE, row, b"node").unwrap();
            }
            let listed = rows.iter().map(|row| (CELL_TRIE, &row[..]));
            record(&mut tx, commit, listed).unwrap();
        }
        let mut drain = None;
        for (head, left) in (3..).zip([[[3], [4], [5]].as_slice(), &[[5]], &[]]) {
            sweep(&mut tx, &mut drain, head, 3, 4, 2).unwrap();
            assert_eq!(keys_of(&tx, CELL_TRIE), left);
        }
        assert!(keys_of(&tx, STALE).is_empty());
    }

    /// A slice whose commit rolled back is deleted again, not skipped.
    #[test]
    fn a_rolled_back_slice_is_retried() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        for row in [[1], [2]] {
            tx.put(CELL_TRIE, &row, b"node").unwrap();
        }
        record(&mut tx, 0, [(CELL_TRIE, &[1][..]), (CELL_TRIE, &[2][..])]).unwrap();
        tx.commit().unwrap();
        let mut drain = None;
        let mut tx = db.begin_write().unwrap();
        sweep(&mut tx, &mut drain, 1, 0, 1, 1).unwrap();
        drop(tx);
        // The retry has the same commit number.
        let mut tx = db.begin_write().unwrap();
        sweep(&mut tx, &mut drain, 1, 0, 1, 1).unwrap();
        assert_eq!(keys_of(&tx, CELL_TRIE), [[2]]);
    }

    /// With a zero window an epoch is due at its own last commit, whose rows
    /// the cache then lists. If that commit rolls back, its retry must not
    /// delete what the failed attempt alone superseded.
    #[test]
    fn a_rolled_back_commit_leaves_no_cached_rows() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        for row in [[1], [2], [3]] {
            tx.put(CELL_TRIE, &row, b"node").unwrap();
        }
        // Commit 0, in the same epoch, superseded row 3.
        record(&mut tx, 0, [(CELL_TRIE, &[3][..])]).unwrap();
        tx.commit().unwrap();
        let mut drain = None;
        let mut tx = db.begin_write().unwrap();
        record(&mut tx, 1, [(CELL_TRIE, &[1][..]), (CELL_TRIE, &[2][..])]).unwrap();
        sweep(&mut tx, &mut drain, 1, 1, 2, 1).unwrap();
        drop(tx);
        // The retry supersedes nothing, so only row 3 is dead.
        let mut tx = db.begin_write().unwrap();
        sweep(&mut tx, &mut drain, 1, 1, 2, 1).unwrap();
        assert_eq!(keys_of(&tx, CELL_TRIE), [[1], [2]]);
    }

    /// Recording alone deletes nothing, even of rows already due.
    #[test]
    fn recording_does_not_sweep() {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(0));
        let mut tx = db.begin_write().unwrap();
        tx.put(CELL_TRIE, b"row", b"node").unwrap();
        let head = crate::EPOCH_COMMITS - 1;
        let listed = pruner.record(&mut tx, head, [(CELL_TRIE, &b"row"[..])]);
        assert_eq!(listed.unwrap(), 1);
        assert!(tx.get(CELL_TRIE, b"row").unwrap().is_some());
        pruner.sweep(&mut tx, head, 1).unwrap();
        assert!(tx.get(CELL_TRIE, b"row").unwrap().is_none());
        assert!(keys_of(&tx, STALE).is_empty());
    }

    /// Unparsable `Stale` rows never fail a commit: they are dropped, and the
    /// rows listed around them are still deleted.
    #[test]
    fn malformed_stale_rows_are_dropped_and_the_rest_still_swept() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        tx.put(CELL_TRIE, b"row", b"node").unwrap();
        // Too short for a commit, no table code, and an unknown table code.
        tx.put(STALE, &[0], &[]).unwrap();
        tx.put(STALE, &0u64.to_be_bytes(), &[]).unwrap();
        tx.put(STALE, &[&0u64.to_be_bytes()[..], &[9], b"x"].concat(), &[])
            .unwrap();
        record(&mut tx, 0, [(CELL_TRIE, &b"row"[..])]).unwrap();
        let mut drain = None;
        for head in 1..3 {
            sweep(&mut tx, &mut drain, head, 0, 1, 1000).unwrap();
        }
        assert!(keys_of(&tx, STALE).is_empty());
        assert!(keys_of(&tx, CELL_TRIE).is_empty());
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
}
