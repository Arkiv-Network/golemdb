//! History for individual `(term, container_id)` rows.
//!
//! Pre-images use the live canonical container encoding. Empty changeset bytes
//! mean that this container was absent, independently of the term's other rows.
//!
//! ```
//! use golemdb_cells::CellType;
//! use golemdb_index::{ContainerId, Index, IndexKey, IndexReader, IndexTerm, PostingChange, TermRange, history};
//! use golemdb_merkle::{Keccak256Hasher, RootRef};
//! use golemdb_storage::{MemoryStore, Store, WriteTransaction};
//!
//! let db = MemoryStore::new();
//! let key = IndexKey {
//!     term: IndexTerm::new("color", CellType::Str, b"blue")?,
//!     container_id: ContainerId::split(42).0,
//! };
//! let mut tx = db.begin_write()?;
//! let update = Index::new(&Keccak256Hasher).apply(&mut tx, 1, RootRef::Empty, [
//!     PostingChange::Add { term: key.term.clone(), record_id: 42 },
//! ])?;
//! history::record_changes(&mut tx, 1, &update.changed_containers)?;
//! tx.commit()?;
//! let tx = db.begin_read()?;
//! // This example recorded all changes from the empty baseline at commit 0.
//! assert!(IndexReader::at(&tx, 0).container(&key)?.is_none());
//! assert_eq!(IndexReader::at(&tx, 1).container(&key)?.unwrap().iter().collect::<Vec<_>>(), [42]);
//! // Exact-term and range reads discover all containers, including deleted ones.
//! assert!(!IndexReader::at(&tx, 0).term_exists(&key.term)?);
//! assert!(IndexReader::at(&tx, 0).bitmap(&key.term)?.is_none());
//! assert!(IndexReader::at(&tx, 1).postings(&key.term)?.treemap().contains(42));
//! let terms = IndexReader::at(&tx, 1).terms(TermRange::all())?
//!     .collect::<golemdb_index::Result<Vec<_>>>()?;
//! assert_eq!(terms.len(), 1);
//! assert_eq!(terms[0].0, key.term);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use golemdb_history::ValueSource;
use golemdb_storage::{ReadTransaction, WriteTransaction};

use crate::{BitmapError, ContainerValueChange, Result, tables};

/// Record the net canonical changes returned by `Index::apply`, once per commit.
/// The caller supplies origin pre-images, enforces global commit order, and
/// publishes live state and history together. Payloads must be canonical bytes
/// from `apply`; they are not decoded/re-encoded here. `Some(empty)` is invalid,
/// because only `None` represents absence. Equal before/after values are skipped.
/// Abort the transaction after any error. This does not update live rows or roots.
pub fn record_changes(
    tx: &mut impl WriteTransaction,
    commit: u64,
    changes: &[ContainerValueChange],
) -> Result<()> {
    for change in changes {
        if change.before.as_ref().is_some_and(Vec::is_empty)
            || change.after.as_ref().is_some_and(Vec::is_empty)
        {
            return Err(BitmapError::EmptyContainer.into());
        }
        if change.before == change.after {
            continue;
        }
        tables::HISTORY.record(
            tx,
            &change.key.encode(),
            commit,
            change.before.as_deref().unwrap_or_default(),
        )?;
    }
    Ok(())
}

/// Borrow selected raw bytes; parsing/charging belongs to the caller.
/// The owner must validate target/head and retention coverage first.
pub(crate) fn with_encoded_container<T>(
    tx: &impl ReadTransaction,
    key: &[u8],
    target: u64,
    inspect: impl FnOnce(Option<&[u8]>) -> T,
) -> Result<T> {
    Ok(
        tables::HISTORY.with_resolved(tx, key, target, |source| match source {
            ValueSource::Live => tx.with_value(tables::INDEX, key, inspect),
            ValueSource::Preimage(bytes) => Ok(inspect((!bytes.is_empty()).then_some(bytes))),
        })??,
    )
}
