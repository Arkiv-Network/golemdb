//! Cell and container history cleanup and its persisted lower bound.

use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{Batch, PruningConfig, PruningError, Result};

const SUPERBLOCK: Table = Table("Superblock");
const HISTORY_FLOOR: &[u8] = b"history_floor";

/// Establish complete history from genesis, in the genesis transaction.
/// Never call this to repair missing metadata or backfill an existing database.
pub(crate) fn initialize(tx: &mut impl WriteTransaction) -> Result<()> {
    Ok(tx.insert(SUPERBLOCK, HISTORY_FLOOR, &0u64.to_be_bytes())?)
}

/// Validate persisted history pruning state against this snapshot's head.
/// This checks metadata, not the contents of every history row.
pub(crate) fn validate(tx: &impl ReadTransaction, head: u64, min_retention: u64) -> Result<()> {
    if read_floor(tx, head)? > head.saturating_sub(min_retention) {
        return Err(PruningError::InvalidHistoryMetadata(
            "history floor violates genesis minimum retention",
        ));
    }
    Ok(())
}

pub(super) fn sweep(
    tx: &mut impl WriteTransaction,
    head: u64,
    cutoff: u64,
    batch: Batch,
) -> Result<()> {
    let mut highest = None;
    for tables in [
        golemdb_cells::tables::HISTORY,
        golemdb_index::tables::HISTORY,
    ] {
        let removed = tables.prune_batch(tx, cutoff, batch.max, |rows, oldest| {
            batch.ready(rows, oldest, cutoff)
        })?;
        highest = highest.max(removed);
    }
    if let Some(highest) = highest
        && highest > read_floor(tx, head)?
    {
        tx.put(SUPERBLOCK, HISTORY_FLOOR, &highest.to_be_bytes())?;
    }
    Ok(())
}

pub(crate) fn validate_target(
    tx: &impl ReadTransaction,
    head: u64,
    requested: u64,
    config: Option<PruningConfig>,
) -> Result<()> {
    if requested > head {
        return Err(PruningError::CommitInFuture { requested, head });
    }
    let floor = read_floor(tx, head)?;
    let earliest = config.map_or(floor, |config| {
        head.saturating_sub(config.window()).max(floor)
    });
    if requested < earliest {
        return Err(PruningError::CommitOutsideHistory {
            requested,
            earliest,
            head,
        });
    }
    Ok(())
}

fn read_floor(tx: &impl ReadTransaction, head: u64) -> Result<u64> {
    let bytes = tx
        .get(SUPERBLOCK, HISTORY_FLOOR)?
        .ok_or(PruningError::InvalidHistoryMetadata(
            "missing history floor",
        ))?;
    let floor = u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| PruningError::InvalidHistoryMetadata("malformed history floor"))?,
    );
    if floor > head {
        return Err(PruningError::InvalidHistoryMetadata(
            "history floor exceeds head",
        ));
    }
    Ok(floor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use golemdb_storage::{MemoryStore, StorageError, Store};

    #[test]
    fn supported_targets_respect_window_and_previous_cleanup() {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        initialize(&mut tx).unwrap();
        for (head, floor, window, earliest) in [
            (0, 0, None, 0),
            (0, 0, Some(0), 0),
            (5, 0, Some(10), 0),
            (10, 0, Some(3), 7),
            (10, 8, Some(3), 8),
            (10, 8, Some(100), 8),
            (10, 8, None, 8),
            (10, 0, Some(0), 10),
            (1000, 0, Some(64), 936),
            (1000, 0, Some(256), 744),
        ] {
            tx.put(SUPERBLOCK, HISTORY_FLOOR, &u64::to_be_bytes(floor))
                .unwrap();
            for target in 0..=head + 1 {
                let result = validate_target(&tx, head, target, window.map(PruningConfig::new));
                if target > head {
                    assert!(
                        matches!(result, Err(PruningError::CommitInFuture { requested, head: h }) if requested == target && h == head)
                    );
                } else if target < earliest {
                    assert!(
                        matches!(result, Err(PruningError::CommitOutsideHistory { requested, earliest: e, head: h }) if requested == target && e == earliest && h == head)
                    );
                } else {
                    result.unwrap();
                }
            }
        }
    }

    #[test]
    fn target_validation_never_infers_coverage_from_missing_or_invalid_metadata() {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        for bytes in [None, Some(vec![0; 7]), Some(11u64.to_be_bytes().to_vec())] {
            if let Some(bytes) = bytes {
                tx.put(SUPERBLOCK, HISTORY_FLOOR, &bytes).unwrap();
            }
            assert!(matches!(
                validate_target(&tx, 10, 0, None),
                Err(PruningError::InvalidHistoryMetadata(_))
            ));
        }
    }

    #[test]
    fn history_metadata_is_checked_and_never_reinitialized() {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        assert!(matches!(
            validate(&tx, 0, 0),
            Err(PruningError::InvalidHistoryMetadata(_))
        ));
        initialize(&mut tx).unwrap();
        validate(&tx, 0, 0).unwrap();
        assert_eq!(
            tx.get(SUPERBLOCK, HISTORY_FLOOR).unwrap(),
            Some(0u64.to_be_bytes().to_vec())
        );
        tx.put(SUPERBLOCK, HISTORY_FLOOR, &5u64.to_be_bytes())
            .unwrap();
        validate(&tx, 5, 0).unwrap();
        assert!(matches!(
            validate(&tx, 4, 0),
            Err(PruningError::InvalidHistoryMetadata(_))
        ));
        assert!(matches!(
            initialize(&mut tx),
            Err(PruningError::Storage(StorageError::AlreadyExists))
        ));
        assert_eq!(
            tx.get(SUPERBLOCK, HISTORY_FLOOR).unwrap(),
            Some(5u64.to_be_bytes().to_vec())
        );
        tx.put(SUPERBLOCK, HISTORY_FLOOR, &[0; 7]).unwrap();
        assert!(matches!(
            validate(&tx, 5, 0),
            Err(PruningError::InvalidHistoryMetadata(_))
        ));
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::*;
    use golemdb_cells::tables::HISTORY as CELLS;
    use golemdb_history::{HistoryError, changeset_key};
    use golemdb_index::tables::HISTORY as INDEX;
    use golemdb_storage::{MemoryStore, Store};

    #[test]
    fn tables_drain_independently_and_floor_tracks_only_actual_removals() {
        let store = MemoryStore::new();
        let mut tx = store.begin_write().unwrap();
        initialize(&mut tx).unwrap();
        // Split the cell changes at commit 5 across batches.
        for (commit, key) in [
            (1, b"a"),
            (2, b"a"),
            (5, b"a"),
            (5, b"b"),
            (5, b"c"),
            (9, b"a"),
        ] {
            CELLS.record(&mut tx, key, commit, None).unwrap();
        }
        INDEX.record(&mut tx, b"i", 3, None).unwrap();
        let batch = Batch {
            min: 3,
            max: 3,
            max_wait: 4,
        };
        sweep(&mut tx, 10, 4, batch).unwrap();
        assert_eq!(read_floor(&tx, 10).unwrap(), 0);
        sweep(&mut tx, 10, 5, batch).unwrap();
        assert_eq!(read_floor(&tx, 10).unwrap(), 5);
        assert_eq!(
            CELLS.read_commits(&tx, b"a").unwrap(),
            [9].into_iter().collect()
        );
        assert!(
            tx.get(CELLS.changeset, &changeset_key(5, b"b"))
                .unwrap()
                .is_some()
        );
        assert!(tx.get(INDEX.history, b"i").unwrap().is_some());
        sweep(&mut tx, 11, 7, batch).unwrap(); // Index is old enough, cells still wait.
        assert_eq!(read_floor(&tx, 11).unwrap(), 5);
        assert!(tx.get(INDEX.history, b"i").unwrap().is_none());
        sweep(&mut tx, 12, 9, batch).unwrap();
        assert_eq!(read_floor(&tx, 12).unwrap(), 9);
        assert!(tx.get(CELLS.history, b"b").unwrap().is_none());
        sweep(&mut tx, 100, 100, batch).unwrap();
        assert_eq!(read_floor(&tx, 100).unwrap(), 9);
    }

    #[test]
    fn malformed_keys_and_bitmaps_abort_the_batch() {
        let db = MemoryStore::new();
        let mut tx = db.begin_write().unwrap();
        initialize(&mut tx).unwrap();
        INDEX.record(&mut tx, b"i", 1, None).unwrap();
        tx.commit().unwrap();
        let batch = Batch {
            min: 1,
            max: 10,
            max_wait: 0,
        };
        let mut tx = db.begin_write().unwrap();
        tx.put(INDEX.changeset, &[0; 7], b"").unwrap();
        assert!(matches!(
            sweep(&mut tx, 2, 2, batch),
            Err(PruningError::History(
                HistoryError::InvalidChangesetKey { .. }
            ))
        ));
        tx.abort();
        let mut tx = db.begin_write().unwrap();
        tx.put(INDEX.history, b"i", b"malformed").unwrap();
        assert!(matches!(
            sweep(&mut tx, 2, 2, batch),
            Err(PruningError::History(HistoryError::InvalidCommits { .. }))
        ));
        tx.abort();
        let mut tx = db.begin_write().unwrap();
        sweep(&mut tx, 2, 2, batch).unwrap();
        assert_eq!(read_floor(&tx, 2).unwrap(), 1);
    }
}
