//! Transactional, byte-ordered key/value storage for GolemDB.
//!
//! Transactions span all tables. Readers retain a committed snapshot;
//! a writer sees its own changes and publishes them atomically on commit. Drop
//! aborts an uncommitted writer.
//!
//! [`MemoryStore`] is the initial backend. The traits intentionally expose
//! owned bytes and no backend-specific handles or threading requirements.
//! The separate `golemdb-storage-mdbx` crate implements these traits for MDBX.
//!
//! ```
//! use golemdb_storage::{Store, MemoryStore, ReadCursor, ReadTransaction,
//!     Table, WriteTransaction, scan_prefix};
//!
//! let terms = Table("Index");
//! let store = MemoryStore::new();
//! let snapshot = store.begin_read()?;
//! let mut tx = store.begin_write()?;
//! tx.put(terms, b"name\0Alice", b"root")?;
//! tx.commit()?;
//! assert_eq!(snapshot.get(terms, b"name\0Alice")?, None);
//! let latest = store.begin_read()?;
//! let mut cursor = latest.cursor(terms, b"name\0Alice")?;
//! assert_eq!(cursor.next()?.unwrap().1, b"root");
//! let mut reverse = latest.cursor(terms, b"name\0Alice")?;
//! assert_eq!(reverse.prev()?.unwrap().1, b"root");
//! let rows = scan_prefix(&latest, terms, b"name\0".to_vec())?
//!     .collect::<golemdb_storage::Result<Vec<_>>>()?;
//! assert_eq!(rows.len(), 1);
//! # Ok::<(), golemdb_storage::StorageError>(())
//! ```

mod error;
mod memory;
mod scan;
mod table;

pub use error::StorageError;
pub use memory::{MemoryCursor, MemoryReadTransaction, MemoryStore, MemoryWriteTransaction};
pub use scan::{Scan, scan, scan_prefix};
pub use table::Table;

pub type Entry = (Vec<u8>, Vec<u8>);
pub type Result<T> = std::result::Result<T, StorageError>;

/// A store with concurrent snapshot readers and one active writer at a time.
///
/// Readers and their cursors remain usable while a writer is active or commits.
/// Only writers serialize with other writers. Every backend must support this.
pub trait Store {
    /// Physical ceilings, independent of deployment admission policy.
    /// Unbounded backends may keep these defaults; bounded adapters must override.
    fn max_key_size(&self) -> usize {
        usize::MAX
    }
    fn max_value_size(&self) -> usize {
        usize::MAX
    }

    type Read<'db>: ReadTransaction
    where
        Self: 'db;
    type Write<'db>: WriteTransaction
    where
        Self: 'db;

    fn begin_read(&self) -> Result<Self::Read<'_>>;
    /// Serializes with other writers. Do not nest writes on the same store.
    fn begin_write(&self) -> Result<Self::Write<'_>>;
}

pub trait ReadTransaction {
    type Cursor<'tx>: ReadCursor
    where
        Self: 'tx;

    /// Return `None` for an absent key or a table not yet written.
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Create a cursor starting at `key`, without consuming its first row.
    /// The first `next` returns the smallest key >= it; the first `prev` returns
    /// the largest key <= it. Use `b""` to scan forward from the first row.
    /// The cursor retains any needed key bytes without borrowing this argument.
    /// A table not yet written behaves like an empty table.
    fn cursor(&self, table: Table, key: &[u8]) -> Result<Self::Cursor<'_>>;
}

/// Bidirectional cursor with explicit boundary behavior.
///
/// Creation supplies the initial key. Once a row
/// is returned, `next`/`prev` move strictly after/before that row. Moving past an
/// end returns `None` repeatedly in that direction; reversing returns the
/// boundary row. An empty table always returns `None`. Create another cursor to
/// reposition; no separate seek/first sequence is needed.
pub trait ReadCursor {
    fn next(&mut self) -> Result<Option<Entry>>;
    fn prev(&mut self) -> Result<Option<Entry>>;
}

/// Mutations borrow this write transaction exclusively, not the store.
///
/// Independent read transactions and their cursors may remain active throughout
/// writes and commit. Only a cursor borrowed from *this same write transaction*
/// must finish being used before mutating it, because mutation requires `&mut
/// self`. This is an API borrowing rule, not a restriction on concurrent readers.
///
/// `AlreadyExists` is recoverable; callers should abort on other write errors.
/// Implementations must discard pending changes and release the transaction
/// when dropped without committing.
pub trait WriteTransaction: ReadTransaction {
    /// True only when no tables or rows exist in this writer's snapshot.
    /// Initialization must make this check under the same writer as its writes.
    /// Adapters that cannot inspect the catalogue must fail, never assume empty.
    fn is_pristine(&self) -> Result<bool> {
        Err(StorageError::Backend(
            "backend cannot inspect its table catalogue".into(),
        ))
    }

    /// Insert or replace a row, creating the table transactionally if needed.
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()>;
    /// Insert only, creating the table transactionally if needed.
    /// Existing keys return `AlreadyExists`, preserving the row.
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()>;
    /// Insert a row whose key sorts after every key in the table, creating the
    /// table transactionally if needed. A key equal to the last returns
    /// `AlreadyExists`; backends may return `OutOfOrder` for an earlier key.
    fn append(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        self.insert(table, key, value)
    }
    /// Return whether a row was removed. An absent table returns `false`
    /// without creating it.
    fn delete(&mut self, table: Table, key: &[u8]) -> Result<bool>;
    fn commit(self) -> Result<()>;
    /// Discard pending changes and release the transaction immediately.
    /// Dropping an uncommitted transaction has the same effect.
    fn abort(self)
    where
        Self: Sized,
    {
    }
}

#[cfg(test)]
mod tests;
