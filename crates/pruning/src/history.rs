//! Internal state for cell and container history pruning. A new database has
//! complete history from commit zero. Future cleanup must advance the stored
//! floor atomically with destructive writes; changing the pruning window must
//! never lower it. The key and encoding stay inside this module.

use golemdb_storage::{ReadTransaction, Table, WriteTransaction};

use crate::{PruningError, Result};

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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use golemdb_storage::{MemoryStore, StorageError, Store};

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
