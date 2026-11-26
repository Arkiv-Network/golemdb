//! Cell pre-images over the shared history mechanism.
//!
//! Present values retain their complete tagged encoding. Only an empty
//! changeset value means absence; a missing selected changeset is an error.
//!
//! ```
//! use golemdb_cells::{CellChange, CellKey, CellNameRef, CellReader, CellValue, Cells, history};
//! use golemdb_merkle::{Keccak256Hasher, RootRef};
//! use golemdb_storage::{MemoryStore, Store, WriteTransaction};
//!
//! let db = MemoryStore::new();
//! let key = CellKey::new(64, CellNameRef::raw(b"name"));
//! let mut tx = db.begin_write()?;
//! let update = Cells::new(&Keccak256Hasher).apply(&mut tx, 1, RootRef::Empty, [
//!     CellChange::Put { key: key.clone(), value: CellValue::from_str("Alice") },
//! ])?;
//! history::record_changes(&mut tx, 1, &update.changed_cells)?;
//! tx.commit()?;
//! let tx = db.begin_read()?;
//! // This example recorded all changes from the empty baseline at commit 0.
//! assert!(CellReader::at(&tx, 0).get(&key)?.is_none());
//! assert_eq!(CellReader::at(&tx, 1).get(&key)?, Some(CellValue::from_str("Alice")));
//! // Scans discover names in both live storage and history, then resolve at T.
//! assert!(CellReader::at(&tx, 0).scan_record(64)?.next().is_none());
//! let rows = CellReader::at(&tx, 1).scan_record(64)?
//!     .collect::<golemdb_cells::Result<Vec<_>>>()?;
//! assert_eq!(rows, vec![(key, CellValue::from_str("Alice"))]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use golemdb_storage::WriteTransaction;

use crate::{CellValue, CellValueChange, Result, tables};

/// Record the net changes returned by `Cells::apply`, once per commit.
/// Equal before/after values are skipped. The caller supplies origin pre-images,
/// enforces global commit order, and publishes live state and history together.
/// Abort the transaction after any error, including a failure partway through
/// the batch. This function does not update live cells, roots or head.
pub fn record_changes(
    tx: &mut impl WriteTransaction,
    commit: u64,
    changes: &[CellValueChange],
) -> Result<()> {
    for change in changes {
        if change.before == change.after {
            continue;
        }
        let preimage = change.before.as_ref().map(CellValue::encoded_bytes);
        tables::HISTORY.record(tx, &change.key.encode(), commit, preimage)?;
    }
    Ok(())
}
