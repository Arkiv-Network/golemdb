use golemdb_storage::{ReadCursor, Table, WriteTransaction};

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

/// List the rows `commit` superseded.
pub(crate) fn record<'a>(
    tx: &mut impl WriteTransaction,
    commit: u64,
    superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
) -> Result<()> {
    let mut keys = (superseded.into_iter())
        .map(|(table, key)| Ok([&commit.to_be_bytes()[..], &[code(table)?], key].concat()))
        .collect::<Result<Vec<_>>>()?;
    keys.sort_unstable();
    for key in keys {
        tx.put(STALE, &key, &[])?;
    }
    Ok(())
}

/// Delete the rows superseded at or before `cutoff` by the oldest
/// `max_commits` commits that listed any. A due `Stale` row that does not
/// parse is dropped, never failing the commit.
pub(crate) fn sweep(tx: &mut impl WriteTransaction, cutoff: u64, max_commits: usize) -> Result<()> {
    let (mut due, mut taken, mut last) = (Vec::new(), 0, None);
    let mut cursor = tx.cursor(STALE, b"")?;
    while let Some((key, _)) = cursor.next()? {
        if let Some(commit) = commit(&key) {
            if commit > cutoff {
                break;
            }
            if last != Some(commit) {
                if taken == max_commits {
                    break;
                }
                (taken, last) = (taken + 1, Some(commit));
            }
        }
        due.push(key);
    }
    drop(cursor);
    for key in due {
        if let Some((table, row)) = row(&key) {
            tx.delete(table, row)?;
        }
        tx.delete(STALE, &key)?;
    }
    Ok(())
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

    use super::{STALE, code};
    use crate::{Pruner, PruningConfig};

    /// Commit `c` writes a few rows and supersedes those of `c − 1`. After every
    /// commit, a row is gone exactly once it left the window.
    fn run(window: u64) {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(window));
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
            pruner.record(&mut tx, commit, listed).unwrap();
            pruner.sweep(&mut tx, commit).unwrap();
            superseded.extend(old.into_iter().map(|row| (commit, row)));
            tx.commit().unwrap();

            let tx = db.begin_read().unwrap();
            for (at, row) in &superseded {
                let stored = tx.get(CELL_TRIE, row).unwrap().is_some();
                assert_eq!(
                    stored,
                    at + window > commit,
                    "row superseded at {at}, head {commit}"
                );
            }
            for row in &written {
                assert!(tx.get(CELL_TRIE, row).unwrap().is_some());
            }
        }
        let tx = db.begin_read().unwrap();
        for (at, row) in &superseded {
            assert_eq!(tx.get(CELL_TRIE, row).unwrap().is_some(), at + window > 200);
        }
        let stale = scan_prefix(&tx, STALE, vec![]).unwrap().count();
        assert_eq!(
            stale == 0,
            superseded.iter().all(|(at, _)| at + window <= 200)
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
    fn rows_are_deleted_once_they_leave_the_window() {
        for window in [0, 1, 3, 10] {
            run(window);
        }
    }

    /// A backlog drains `max_commits` commits per sweep, counting only commits
    /// that listed rows, each in full.
    #[test]
    fn a_sweep_drains_at_most_max_commits() {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(0));
        let mut tx = db.begin_write().unwrap();
        // Commit 3 supersedes nothing.
        let listed = [(1, 2), (2, 1), (4, 3), (5, 1)];
        for (commit, rows) in listed {
            let keys: Vec<_> = (0..rows).map(|i| [commit, i]).collect();
            for key in &keys {
                tx.put(CELL_TRIE, key, b"node").unwrap();
            }
            let superseded = keys.iter().map(|key| (CELL_TRIE, &key[..]));
            pruner.record(&mut tx, commit.into(), superseded).unwrap();
        }
        let left = |tx: &_| keys_of(tx, CELL_TRIE);
        // Three listing commits are 1, 2 and 4, not 1, 2 and 3.
        super::sweep(&mut tx, 5, 3).unwrap();
        assert_eq!(left(&tx), [[5, 0]]);
        // Commit 5 is not due yet.
        super::sweep(&mut tx, 4, 3).unwrap();
        assert_eq!(left(&tx), [[5, 0]]);
        super::sweep(&mut tx, 5, 1).unwrap();
        assert!(left(&tx).is_empty());
        assert!(keys_of(&tx, STALE).is_empty());
    }

    fn keys_of(tx: &impl ReadTransaction, table: golemdb_storage::Table) -> Vec<Vec<u8>> {
        (scan_prefix(tx, table, vec![]).unwrap())
            .map(|row| row.unwrap().0)
            .collect()
    }

    /// Recording alone deletes nothing, even of rows already due.
    #[test]
    fn recording_does_not_sweep() {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(0));
        let mut tx = db.begin_write().unwrap();
        tx.put(CELL_TRIE, b"row", b"node").unwrap();
        pruner
            .record(&mut tx, 1, [(CELL_TRIE, &b"row"[..])])
            .unwrap();
        assert!(tx.get(CELL_TRIE, b"row").unwrap().is_some());
        pruner.sweep(&mut tx, 1).unwrap();
        assert!(tx.get(CELL_TRIE, b"row").unwrap().is_none());
        assert_eq!(scan_prefix(&tx, STALE, vec![]).unwrap().count(), 0);
    }

    /// Unparsable `Stale` rows never fail a commit: they are dropped, and the
    /// rows listed around them are still deleted.
    #[test]
    fn malformed_stale_rows_are_dropped_and_the_rest_still_swept() {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(0));
        let mut tx = db.begin_write().unwrap();
        tx.put(CELL_TRIE, b"row", b"node").unwrap();
        // Too short for a commit, no table code, and an unknown table code.
        tx.put(STALE, &[0], &[]).unwrap();
        tx.put(STALE, &0u64.to_be_bytes(), &[]).unwrap();
        tx.put(STALE, &[&0u64.to_be_bytes()[..], &[9], b"x"].concat(), &[])
            .unwrap();
        pruner
            .record(&mut tx, 1, [(CELL_TRIE, &b"row"[..])])
            .unwrap();
        pruner.sweep(&mut tx, 1).unwrap();
        assert_eq!(scan_prefix(&tx, STALE, vec![]).unwrap().count(), 0);
        assert_eq!(scan_prefix(&tx, CELL_TRIE, vec![]).unwrap().count(), 0);
    }
}
