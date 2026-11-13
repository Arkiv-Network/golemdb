use golemdb_storage::{ReadCursor, Table, WriteTransaction};

use crate::{PruningError, Result};

/// Superseded rows awaiting deletion, one `commit ‖ table ‖ key` row each with
/// an empty value; `commit` is the one that superseded the row.
pub const STALE: Table = Table("Stale");

/// The versioned tables, by their code in `Stale` keys. The codes are
/// persisted: append only, never reorder.
const TABLES: [Table; 4] = [
    golemdb_cells::tables::CELL_TRIE.table(),
    golemdb_index::tables::INDEX_TRIE.table(),
    golemdb_index::tables::BITMAP_TRIE.table(),
    golemdb_index::tables::BITMAP_CONTAINER.table(),
];

/// List the rows `commit` superseded.
pub(crate) fn register<'a>(
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

/// When a sweep deletes a batch, and how large one may be.
pub(crate) struct Batch {
    /// Due rows to wait for.
    pub(crate) min: usize,
    /// Most rows to delete.
    pub(crate) max: usize,
    /// Commits the oldest due row may wait before a smaller batch goes.
    pub(crate) max_wait: u64,
}

/// Delete the oldest rows superseded at or before `cutoff`, at most
/// `batch.max`, sorted by table and key, once at least `batch.min` are due or
/// the oldest has been due for `batch.max_wait` commits. A `Stale` row too short
/// to name a commit counts as listed at commit 0.
pub(crate) fn sweep(tx: &mut impl WriteTransaction, cutoff: u64, batch: Batch) -> Result<()> {
    let (mut due, mut oldest) = (Vec::new(), None);
    let mut cursor = tx.cursor(STALE, b"")?;
    while due.len() < batch.max {
        let Some((key, _)) = cursor.next()? else {
            break;
        };
        let commit = commit(&key).unwrap_or(0);
        if commit > cutoff {
            break;
        }
        oldest.get_or_insert(commit);
        due.push(key);
    }
    drop(cursor);
    let waited = oldest.is_some_and(|oldest| cutoff - oldest >= batch.max_wait);
    if due.len() < batch.min && !waited {
        return Ok(());
    }
    // Trie rows sorted, so rows sharing a page go together.
    let mut rows: Vec<_> = due.iter().filter_map(|key| row(key)).collect();
    rows.sort_unstable();
    for (table, row) in rows {
        tx.delete(table, row)?;
    }
    for key in &due {
        tx.delete(STALE, key)?;
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
    use golemdb_storage::{MemoryStore, ReadTransaction, Store, WriteTransaction, scan_prefix};

    use super::{Batch, STALE, code, register, sweep};
    use crate::{Pruner, PruningConfig};

    const CELL_TRIE: golemdb_storage::Table = golemdb_cells::tables::CELL_TRIE.table();

    fn keys_of(tx: &impl ReadTransaction, table: golemdb_storage::Table) -> Vec<Vec<u8>> {
        (scan_prefix(tx, table, vec![]).unwrap())
            .map(|row| row.unwrap().0)
            .collect()
    }

    /// Store `rows` and register them as superseded by `commit`.
    fn supersede(tx: &mut impl WriteTransaction, commit: u64, rows: &[[u8; 1]]) {
        for row in rows {
            tx.put(CELL_TRIE, row, b"node").unwrap();
        }
        register(tx, commit, rows.iter().map(|row| (CELL_TRIE, &row[..]))).unwrap();
    }

    /// Commit `c` writes a few rows and supersedes those of `c − 1`. After every
    /// commit, no row is gone before it leaves the window, and once the rows
    /// stop coming every due row is eventually gone.
    fn run(window: u64, min: usize, max: usize, max_wait: u64) {
        let db = MemoryStore::new();
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
            register(
                &mut tx,
                commit,
                old.iter().map(|row| (CELL_TRIE, row.as_slice())),
            )
            .unwrap();
            let cutoff = commit.saturating_sub(window);
            sweep(&mut tx, cutoff, Batch { min, max, max_wait }).unwrap();
            superseded.extend(old.into_iter().map(|row| (commit, row)));
            tx.commit().unwrap();

            let tx = db.begin_read().unwrap();
            for (at, row) in &superseded {
                if at + window > commit {
                    let stored = tx.get(CELL_TRIE, row).unwrap().is_some();
                    assert!(stored, "row superseded at {at} deleted at head {commit}");
                }
            }
            for row in &written {
                assert!(tx.get(CELL_TRIE, row).unwrap().is_some());
            }
        }
        let tx = db.begin_read().unwrap();
        for (at, row) in &superseded {
            let stored = tx.get(CELL_TRIE, row).unwrap().is_some();
            assert_eq!(stored, at + window > 200, "row superseded at {at}");
        }
    }

    #[test]
    fn rows_are_deleted_only_after_they_leave_the_window() {
        for window in [0, 1, 3, 10] {
            for (min, max, max_wait) in [(1, 1000, 50), (5, 8, 50), (1000, 1000, 3)] {
                run(window, min, max, max_wait);
            }
        }
    }

    /// Nothing is deleted until a batch of `min` rows is due; then it all goes.
    #[test]
    fn a_batch_waits_for_min_rows() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        let batch = || Batch {
            min: 3,
            max: 10,
            max_wait: 50,
        };
        supersede(&mut tx, 1, &[[1], [2]]);
        sweep(&mut tx, 1, batch()).unwrap();
        assert_eq!(keys_of(&tx, CELL_TRIE), [[1], [2]]);
        supersede(&mut tx, 2, &[[3]]);
        sweep(&mut tx, 2, batch()).unwrap();
        assert!(keys_of(&tx, CELL_TRIE).is_empty());
        assert!(keys_of(&tx, STALE).is_empty());
    }

    /// A batch takes the oldest due rows, at most `max`.
    #[test]
    fn a_batch_takes_the_oldest_rows_up_to_max() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        supersede(&mut tx, 1, &[[5], [6]]);
        supersede(&mut tx, 2, &[[1], [2]]);
        let batch = || Batch {
            min: 1,
            max: 3,
            max_wait: 50,
        };
        sweep(&mut tx, 2, batch()).unwrap();
        assert_eq!(keys_of(&tx, CELL_TRIE), [[2]]);
        sweep(&mut tx, 2, batch()).unwrap();
        assert!(keys_of(&tx, CELL_TRIE).is_empty());
    }

    /// Fewer than `min` due rows still go once the oldest has waited long
    /// enough.
    #[test]
    fn an_old_row_goes_without_a_full_batch() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        supersede(&mut tx, 1, &[[1]]);
        let batch = || Batch {
            min: 1000,
            max: 1000,
            max_wait: 3,
        };
        sweep(&mut tx, 3, batch()).unwrap();
        assert_eq!(keys_of(&tx, CELL_TRIE), [[1]]);
        sweep(&mut tx, 4, batch()).unwrap();
        assert!(keys_of(&tx, CELL_TRIE).is_empty());
    }

    /// Registering alone deletes nothing, even of rows already due.
    #[test]
    fn registering_does_not_sweep() {
        let db = MemoryStore::new();
        let pruner = Pruner::new(PruningConfig::new(0));
        let mut tx = db.begin_write().unwrap();
        tx.put(CELL_TRIE, b"row", b"node").unwrap();
        pruner
            .register_tries(&mut tx, 1, [(CELL_TRIE, &b"row"[..])])
            .unwrap();
        assert!(tx.get(CELL_TRIE, b"row").unwrap().is_some());
        assert_eq!(keys_of(&tx, STALE).len(), 1);
    }

    /// Unparsable `Stale` rows never fail a commit: they are dropped, and the
    /// rows listed around them are still deleted.
    #[test]
    fn malformed_stale_rows_are_dropped_and_the_rest_still_swept() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        // Too short for a commit, no table code, and an unknown table code.
        tx.put(STALE, &[0], &[]).unwrap();
        tx.put(STALE, &0u64.to_be_bytes(), &[]).unwrap();
        tx.put(STALE, &[&0u64.to_be_bytes()[..], &[9], b"x"].concat(), &[])
            .unwrap();
        supersede(&mut tx, 0, &[[1]]);
        let batch = Batch {
            min: 1,
            max: 1000,
            max_wait: 50,
        };
        sweep(&mut tx, 0, batch).unwrap();
        assert!(keys_of(&tx, STALE).is_empty());
        assert!(keys_of(&tx, CELL_TRIE).is_empty());
    }

    #[test]
    fn table_codes_are_pinned_and_unversioned_tables_are_rejected() {
        use golemdb_index::tables::{BITMAP_CONTAINER, BITMAP_TRIE, INDEX, INDEX_TRIE};
        let codes = [
            CELL_TRIE,
            INDEX_TRIE.table(),
            BITMAP_TRIE.table(),
            BITMAP_CONTAINER.table(),
        ]
        .map(|t| code(t).unwrap());
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
