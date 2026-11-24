//! Internal state for cell and container history pruning. A new database has
//! complete history from commit zero. Future cleanup must advance the stored
//! floor atomically with destructive writes; changing the pruning window must
//! never lower it. The key and encoding stay inside this module.

use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{PruningConfig, PruningError, Result};

const SUPERBLOCK: Table = Table("Superblock");
const HISTORY_FLOOR: &[u8] = b"history_floor";

/// Establish complete history from genesis, in the genesis transaction.
/// Never call this to repair missing metadata or backfill an existing database.
pub(crate) fn initialize(tx: &mut impl WriteTransaction) -> Result<()> {
    Ok(tx.insert(SUPERBLOCK, HISTORY_FLOOR, &0u64.to_be_bytes())?)
}

/// Validate persisted history pruning state against this snapshot's head.
/// This checks metadata, not the contents of every history row.
pub(crate) fn validate(tx: &impl ReadTransaction, head: u64) -> Result<()> {
    read_floor(tx, head).map(|_| ())
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
            validate(&tx, 0),
            Err(PruningError::InvalidHistoryMetadata(_))
        ));
        initialize(&mut tx).unwrap();
        validate(&tx, 0).unwrap();
        assert_eq!(
            tx.get(SUPERBLOCK, HISTORY_FLOOR).unwrap(),
            Some(0u64.to_be_bytes().to_vec())
        );
        tx.put(SUPERBLOCK, HISTORY_FLOOR, &5u64.to_be_bytes())
            .unwrap();
        validate(&tx, 5).unwrap();
        assert!(matches!(
            validate(&tx, 4),
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
            validate(&tx, 5),
            Err(PruningError::InvalidHistoryMetadata(_))
        ));
    }
}
