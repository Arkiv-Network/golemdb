//! Shared pre-image history over caller-owned storage transactions.
//!
//! Owners supply an encoded item key and two distinct tables. The history row
//! holds a canonical [`CommitSet`]; the changeset key is `commit:u64_be ‖ item`.
//! A change at c records the value immediately before c. Resolving state after
//! T uses the first change strictly after T, or returns [`ValueSource::Live`]
//! so the owner can read its live table in the **same snapshot**.
//!
//! Pre-images are opaque. Empty bytes are a present row, which domain codecs
//! may interpret as absence; a missing selected row is an error. The caller
//! validates head/retention coverage before resolving: missing history alone
//! cannot distinguish an unchanged item from pruned or never-recorded history.
//!
//! Record each net item change once, after coalescing branch operations. The
//! caller publishes history and live state atomically and must abort after a
//! mutation error. No method opens or commits a transaction. Pruning, domain
//! codecs, pricing and branch integration belong to subsequent layers.
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
//! HISTORY.record(&mut tx, b"item", 10, b"")?; // Creation: absent before 10.
//! HISTORY.record(&mut tx, b"item", 20, b"A")?; // A was replaced at 20.
//! tx.commit()?;
//! let tx = db.begin_read()?;
//! assert_eq!(HISTORY.resolve(&tx, b"item", 10)?, ValueSource::Preimage(b"A".to_vec()));
//! assert_eq!(HISTORY.resolve(&tx, b"item", 20)?, ValueSource::Live);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod commits;
mod error;
mod history;

pub use commits::{CommitSet, CommitSetError};
pub use error::{HistoryError, Result};
pub use history::{HistoryTables, ValueSource, changeset_key};
