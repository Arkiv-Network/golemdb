//! Native MDBX tables, snapshot transactions and lazy bidirectional cursors.
use std::{path::Path, sync::Arc};

use libmdbx::{NoWriteMap, RO, RW, TableFlags, TransactionKind, WriteFlags};

use golemdb_storage::{
    Entry, ReadCursor, ReadTransaction, Result, StorageError, Store, StoreStats, Table,
    WriteTransaction,
};

#[cfg(test)]
mod tests;

/// Backend capacity settings, not table declarations or logical storage quotas.
#[derive(Clone, Copy, Debug)]
pub struct MdbxOptions {
    pub max_tables: u64,
    /// Maximum database size in bytes. Reserves address space, not disk.
    pub max_map_size: usize,
    pub growth_step: usize,
    /// Page size in bytes, used only when creating a database. Sets the maximum key size.
    pub page_size: usize,
}

impl Default for MdbxOptions {
    fn default() -> Self {
        Self {
            max_tables: 128,
            max_map_size: DEFAULT_MAX_MAP_SIZE,
            growth_step: 16 * 1024 * 1024,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

/// 1 TiB: a generous ceiling, since it only reserves address space.
const DEFAULT_MAX_MAP_SIZE: usize = 1024 * 1024 * 1024 * 1024;

/// Fixed so the maximum key size doesn't depend on the OS page size.
const DEFAULT_PAGE_SIZE: usize = 4096;

/// Clone to share one environment. Do not independently open the same directory
/// twice within a process; close all handles before reopening it.
/// The environment's named tables must be managed by this adapter, with normal
/// byte ordering and unique keys (no external integer/reverse/DUPSORT tables).
///
/// ```
/// use golemdb_storage::{Store, ReadTransaction, Table, WriteTransaction};
/// use golemdb_storage_mdbx::MdbxStore;
/// let dir = tempfile::tempdir()?;
/// let store = MdbxStore::open(dir.path())?;
/// let mut tx = store.begin_write()?;
/// tx.put(Table("Example"), b"key", b"value")?;
/// tx.commit()?;
/// assert_eq!(store.begin_read()?.get(Table("Example"), b"key")?, Some(b"value".to_vec()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct MdbxStore {
    inner: Arc<libmdbx::Database<NoWriteMap>>,
    limits: Limits,
}

#[derive(Clone, Copy)]
struct Limits {
    key: usize,
    value: usize,
}

impl Limits {
    fn key(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.key {
            return Err(StorageError::KeyTooLarge {
                actual: bytes.len(),
                max: self.key,
            });
        }
        Ok(())
    }
}

impl MdbxStore {
    /// Create the directory if needed and open a durable read/write environment.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, MdbxOptions::default())
    }

    pub fn open_with_options(path: impl AsRef<Path>, options: MdbxOptions) -> Result<Self> {
        let invalid = || {
            StorageError::Backend(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid MDBX capacity settings",
                )
                .into(),
            )
        };
        if options.max_tables == 0
            || options.max_map_size == 0
            || options.growth_step == 0
            || options.page_size == 0
        {
            return Err(invalid());
        }
        let max_size = isize::try_from(options.max_map_size).map_err(|_| invalid())?;
        let growth_step = isize::try_from(options.growth_step).map_err(|_| invalid())?;
        std::fs::create_dir_all(path.as_ref()).map_err(|e| StorageError::Backend(e.into()))?;
        let inner = libmdbx::Database::open_with_options(
            path,
            libmdbx::DatabaseOptions {
                max_tables: Some(options.max_tables),
                page_size: Some(libmdbx::PageSize::Set(options.page_size)),
                mode: libmdbx::Mode::ReadWrite(libmdbx::ReadWriteOptions {
                    sync_mode: libmdbx::SyncMode::Durable,
                    max_size: Some(max_size),
                    growth_step: Some(growth_step),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .map_err(backend)?;
        // SAFETY: the environment is successfully opened and remains alive;
        // both functions are read-only queries with normal (non-DUPSORT) flags.
        let (key, value) = unsafe {
            (
                mdbx_sys::mdbx_env_get_maxkeysize_ex(inner.ptr().0, 0),
                mdbx_sys::mdbx_env_get_maxvalsize_ex(inner.ptr().0, 0),
            )
        };
        if key <= 0 || value <= 0 {
            return Err(backend(libmdbx::Error::Invalid));
        }
        Ok(Self {
            inner: Arc::new(inner),
            limits: Limits {
                key: key as usize,
                value: value as usize,
            },
        })
    }

    /// Physical key/table-name limit for this environment's page size.
    pub fn max_key_size(&self) -> usize {
        self.limits.key
    }
    /// Physical value limit for normal (non-DUPSORT) tables.
    pub fn max_value_size(&self) -> usize {
        self.limits.value
    }
}

fn backend(error: libmdbx::Error) -> StorageError {
    match error {
        libmdbx::Error::MapFull => StorageError::Full,
        libmdbx::Error::KeyExist => StorageError::AlreadyExists,
        error => StorageError::Backend(Box::new(error)),
    }
}

pub struct MdbxTransaction<'db, K: TransactionKind> {
    inner: libmdbx::Transaction<'db, K, NoWriteMap>,
    limits: Limits,
}
pub type MdbxReadTransaction<'db> = MdbxTransaction<'db, RO>;
pub type MdbxWriteTransaction<'db> = MdbxTransaction<'db, RW>;

impl Store for MdbxStore {
    fn max_key_size(&self) -> usize {
        self.limits.key
    }
    fn max_value_size(&self) -> usize {
        self.limits.value
    }
    /// Bytes written are estimated from MDBX's page counters, so they include
    /// pages that never reach disk: those of aborted transactions, and pages a
    /// transaction copies and then frees, as when a table empties. A session
    /// lasts while any process has the environment open.
    fn stats(&self) -> Result<Option<StoreStats>> {
        // SAFETY: `MDBX_envinfo` holds only integers, so all zeros is valid; the
        // environment remains open and `info` is writable for its full size.
        let (code, info) = unsafe {
            let mut info: mdbx_sys::MDBX_envinfo = std::mem::zeroed();
            let code = mdbx_sys::mdbx_env_info_ex(
                self.inner.ptr().0,
                std::ptr::null(),
                &mut info,
                size_of::<mdbx_sys::MDBX_envinfo>(),
            );
            (code, info)
        };
        if code != mdbx_sys::MDBX_SUCCESS {
            return Err(backend(libmdbx::Error::from_err_code(code)));
        }
        let page = u64::from(info.mi_dxb_pagesize);
        let ops = info.mi_pgop_stat;
        Ok(Some(StoreStats {
            db_size: (info.mi_last_pgno + 1) * page,
            // Pages created or copied, plus pages flushed early and changed
            // again, less merged pages.
            session_bytes_written: (ops.newly + ops.cow + ops.unspill - ops.merge) * page,
            session_syncs: ops.fsync + ops.msync,
        }))
    }

    type Read<'db> = MdbxReadTransaction<'db>;
    type Write<'db> = MdbxWriteTransaction<'db>;
    fn begin_read(&self) -> Result<Self::Read<'_>> {
        Ok(MdbxTransaction {
            inner: self.inner.begin_ro_txn().map_err(backend)?,
            limits: self.limits,
        })
    }
    fn begin_write(&self) -> Result<Self::Write<'_>> {
        Ok(MdbxTransaction {
            inner: self.inner.begin_rw_txn().map_err(backend)?,
            limits: self.limits,
        })
    }
}

impl<K: TransactionKind> MdbxTransaction<'_, K> {
    fn table(&self, table: Table) -> Result<Option<libmdbx::Table<'_>>> {
        table.validate()?;
        self.limits.key(table.0.as_bytes())?;
        // DBI handles are environment-wide: opening a name can find a cached
        // handle for a table newer than this reader's snapshot. Check the
        // native catalogue in this transaction before using such a handle.
        // Do not translate arbitrary BadDbi failures into missing data.
        let catalog = self.inner.open_table(None).map_err(backend)?;
        if self
            .inner
            .get::<Vec<u8>>(&catalog, table.0.as_bytes())
            .map_err(backend)?
            .is_none()
        {
            return Ok(None);
        }
        match self.inner.open_table(Some(table.0)) {
            Ok(handle) => Ok(Some(handle)),
            Err(libmdbx::Error::NotFound) => Ok(None),
            Err(error) => Err(backend(error)),
        }
    }
}

impl<K: TransactionKind> ReadTransaction for MdbxTransaction<'_, K> {
    type Cursor<'tx>
        = MdbxCursor<'tx, K>
    where
        Self: 'tx;

    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>> {
        table.validate()?;
        self.limits.key(key)?;
        match self.table(table)? {
            Some(handle) => self.inner.get(&handle, key).map_err(backend),
            None => Ok(None),
        }
    }

    fn cursor(&self, table: Table, key: &[u8]) -> Result<Self::Cursor<'_>> {
        table.validate()?;
        self.limits.key(key)?;
        let inner = self
            .table(table)?
            .map(|handle| self.inner.cursor(&handle))
            .transpose()
            .map_err(backend)?;
        Ok(MdbxCursor {
            inner,
            position: Position::Start(key.to_vec()),
        })
    }
}

impl MdbxWriteTransaction<'_> {
    fn write(&mut self, table: Table, key: &[u8], value: &[u8], flags: WriteFlags) -> Result<()> {
        table.validate()?;
        self.limits.key(table.0.as_bytes())?;
        self.limits.key(key)?;
        if value.len() > self.limits.value {
            return Err(StorageError::ValueTooLarge {
                actual: value.len(),
                max: self.limits.value,
            });
        }
        let handle = self
            .inner
            .create_table(Some(table.0), TableFlags::empty())
            .map_err(backend)?;
        self.inner.put(&handle, key, value, flags).map_err(backend)
    }
}

impl WriteTransaction for MdbxWriteTransaction<'_> {
    fn is_empty(&self) -> Result<bool> {
        let catalog = self.inner.open_table(None).map_err(backend)?;
        Ok(self.inner.table_stat(&catalog).map_err(backend)?.entries() == 0)
    }

    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        self.write(table, key, value, WriteFlags::UPSERT)
    }
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<()> {
        self.write(table, key, value, WriteFlags::NO_OVERWRITE)
    }
    fn delete(&mut self, table: Table, key: &[u8]) -> Result<bool> {
        table.validate()?;
        self.limits.key(key)?;
        match self.table(table)? {
            Some(handle) => self.inner.del(&handle, key, None).map_err(backend),
            None => Ok(false),
        }
    }
    fn commit(self) -> Result<()> {
        self.inner.commit().map(|_| ()).map_err(backend)
    }
}

enum Position {
    Start(Vec<u8>),
    At,
    Before,
    After,
}

pub struct MdbxCursor<'tx, K: TransactionKind> {
    inner: Option<libmdbx::Cursor<'tx, K>>,
    position: Position,
}

impl<K: TransactionKind> MdbxCursor<'_, K> {
    fn advance(&mut self, forward: bool) -> Result<Option<Entry>> {
        let Some(cursor) = &mut self.inner else {
            return Ok(None);
        };
        let row = match &self.position {
            Position::Start(key) => {
                let row = cursor.set_range::<Vec<u8>, Vec<u8>>(key).map_err(backend)?;
                if forward {
                    row
                } else {
                    match row {
                        Some((found, value)) if found.as_slice() == key => Some((found, value)),
                        Some(_) => cursor.prev().map_err(backend)?,
                        None => cursor.last().map_err(backend)?,
                    }
                }
            }
            Position::Before if forward => cursor.first().map_err(backend)?,
            Position::After if !forward => cursor.last().map_err(backend)?,
            Position::Before | Position::After => None,
            Position::At if forward => cursor.next().map_err(backend)?,
            Position::At => cursor.prev().map_err(backend)?,
        };
        self.position = if row.is_some() {
            Position::At
        } else if forward {
            Position::After
        } else {
            Position::Before
        };
        Ok(row)
    }
}

impl<K: TransactionKind> ReadCursor for MdbxCursor<'_, K> {
    fn next(&mut self) -> Result<Option<Entry>> {
        self.advance(true)
    }
    fn prev(&mut self) -> Result<Option<Entry>> {
        self.advance(false)
    }
}
