//! Shared pre-image history over caller-owned storage transactions.
//!
//! Owners supply an encoded item key and two distinct tables. The history row
//! holds a serialized [`CommitSet`]; the changeset key is `commit:u64_be ‖ item`.
//! A change at c records the value immediately before c. Resolving state after
//! T uses the first change strictly after T, or returns [`ValueSource::Live`]
//! so the owner can read its live table in the **same snapshot**.
//!
//! Present pre-images are opaque, nonempty bytes. `None` records prior absence
//! as an empty changeset row; a missing selected row is an error. The caller
//! validates head/retention coverage before resolving: missing history alone
//! cannot distinguish an unchanged item from pruned or never-recorded history.
//!
//! Record each net item change once, after coalescing branch operations. The
//! caller publishes history and live state atomically and must abort after a
//! mutation error. [`HistoryTables::prune_batch`] updates the paired tables
//! in a caller's writer. The pruning crate chooses the cutoff and batch sizes
//! and advances the persisted floor in that transaction.
//!
//! History rows grow with retained changes and have no fixed 16 KiB ceiling.
//! Backend key/value limits still apply; exceeding one fails the transaction,
//! never truncates history. Decoding costs depend on bitmap size even though
//! resolution selects at most one changeset row.
//!
//! ```
//! use golemdb_history::{HistoryTables, ValueSource};
//! use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction};
//!
//! const HISTORY: HistoryTables = HistoryTables::new(Table("History"), Table("ChangeSet"));
//! let db = MemoryStore::new();
//! let mut tx = db.begin_write()?;
//! HISTORY.record(&mut tx, b"item", 10, None)?; // Creation: absent before 10.
//! HISTORY.record(&mut tx, b"item", 20, Some(&b"A"[..]))?; // A was replaced at 20.
//! tx.commit()?;
//! let tx = db.begin_read()?;
//!
//! // T = 1: select changeset 10.
//! // Before creation at 10, the item was absent.
//! assert_eq!(HISTORY.resolve(&tx, b"item", 1)?, ValueSource::Preimage(None));
//!
//! // T = 10: select changeset 20.
//! // Before the update at 20, the item contained A.
//! assert_eq!(
//!     HISTORY.resolve(&tx, b"item", 10)?,
//!     ValueSource::Preimage(Some(b"A".to_vec()))
//! );
//!
//! // T = 20: no later change exists.
//! // Read the live value from this same transaction.
//! assert_eq!(HISTORY.resolve(&tx, b"item", 20)?, ValueSource::Live);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod commits;
mod error;
mod history;
mod prune;
mod read;

pub use commits::{CommitSet, CommitSetError};
pub use error::{HistoryError, Result};
pub use history::{CHANGESET_PREFIX_LEN, HistoryTables, ValueSource, changeset_key};
pub use read::ReadOperation;
