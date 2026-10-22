use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use golemdb_api::{CellLimits, Database, Genesis, HashAlgorithm, OpenConfig, OpenError};
use golemdb_storage::{MemoryStore, ReadTransaction, StorageError, Store, Table, WriteTransaction};

fn config(directory: &std::path::Path) -> OpenConfig {
    OpenConfig::new(Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        golemdb_api::RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        Some(golemdb_api::ImmutableDataConfig::new(
            500_000,
            vec![golemdb_api::ImmutableDataSegment {
                name: "events".into(),
                columns: vec![golemdb_api::ImmutableDataColumn::new("payload")],
                compression: golemdb_api::ImmutableDataCompression::Lz4,
            }],
        )),
    ))
    .with_immutable_data_directory(directory)
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Write(usize),
    Panic(usize),
    Commit,
    Read,
}

/// Clones share the wrapped store and the switches.
#[derive(Clone)]
struct Controlled<S> {
    inner: S,
    fault: Fault,
    mutations: Arc<AtomicUsize>,
    max_key: usize,
    max_value: usize,
}

impl<S: Store> Controlled<S> {
    fn new(inner: S, fault: Fault) -> Self {
        let max_key = inner.max_key_size();
        let max_value = inner.max_value_size();
        Self {
            inner,
            fault,
            mutations: Arc::new(AtomicUsize::new(0)),
            max_key,
            max_value,
        }
    }
}

struct ControlledWrite<W> {
    inner: W,
    fault: Fault,
    mutations: Arc<AtomicUsize>,
}

impl<S: Store> Store for Controlled<S> {
    type Read<'a>
        = S::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = ControlledWrite<S::Write<'a>>
    where
        Self: 'a;
    fn max_key_size(&self) -> usize {
        self.max_key
    }
    fn max_value_size(&self) -> usize {
        self.max_value
    }
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        self.inner.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        Ok(ControlledWrite {
            inner: self.inner.begin_write()?,
            fault: self.fault,
            mutations: self.mutations.clone(),
        })
    }
}

fn injected() -> StorageError {
    StorageError::Backend("injected opening failure".into())
}

impl<W: ReadTransaction> ReadTransaction for ControlledWrite<W> {
    type Cursor<'a>
        = W::Cursor<'a>
    where
        Self: 'a;
    fn get(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Option<Vec<u8>>> {
        if matches!(self.fault, Fault::Read) {
            return Err(injected());
        }
        self.inner.get(table, key)
    }
    fn cursor(&self, table: Table, key: &[u8]) -> golemdb_storage::Result<Self::Cursor<'_>> {
        self.inner.cursor(table, key)
    }
}

impl<W> ControlledWrite<W> {
    fn mutation(&self) -> golemdb_storage::Result<()> {
        let step = self.mutations.fetch_add(1, Ordering::SeqCst) + 1;
        if matches!(self.fault, Fault::Panic(n) if n == step) {
            panic!("injected opening panic");
        }
        if matches!(self.fault, Fault::Write(n) if n == step) {
            return Err(injected());
        }
        Ok(())
    }
}

impl<W: WriteTransaction> WriteTransaction for ControlledWrite<W> {
    fn is_empty(&self) -> golemdb_storage::Result<bool> {
        self.inner.is_empty()
    }
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.mutation()?;
        self.inner.put(table, key, value)
    }
    fn insert(&mut self, table: Table, key: &[u8], value: &[u8]) -> golemdb_storage::Result<()> {
        self.mutation()?;
        self.inner.insert(table, key, value)
    }
    fn delete(&mut self, table: Table, key: &[u8]) -> golemdb_storage::Result<bool> {
        self.mutation()?;
        self.inner.delete(table, key)
    }
    fn commit(self) -> golemdb_storage::Result<()> {
        if matches!(self.fault, Fault::Commit) {
            return Err(injected());
        }
        self.inner.commit()
    }
}

fn failures(db: impl Store + Clone + Send + Sync + 'static) {
    let files = tempfile::tempdir().unwrap();
    let baseline = Controlled::new(MemoryStore::new(), Fault::None);
    let count = baseline.mutations.clone();
    let expected = *Database::from_store(baseline, &config(files.path()))
        .unwrap()
        .info();
    let writes = count.load(Ordering::SeqCst);
    assert!(writes > 10);
    for fault in (1..=writes)
        .map(Fault::Write)
        .chain([Fault::Commit, Fault::Read])
    {
        let error = Database::from_store(Controlled::new(db.clone(), fault), &config(files.path()))
            .err()
            .expect("injected failure must abort opening");
        assert!(
            error.to_string().contains("injected opening failure"),
            "{error}"
        );
        assert!(db.begin_write().unwrap().is_empty().unwrap());
    }
    for step in [1, writes / 2, writes] {
        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| Database::from_store(
                Controlled::new(db.clone(), Fault::Panic(step)),
                &config(files.path())
            )))
            .is_err()
        );
        // No partial rows or poisoned MemoryStore writer remain after panic.
        assert!(db.begin_write().unwrap().is_empty().unwrap());
    }
    assert_eq!(
        *Database::from_store(db.clone(), &config(files.path()))
            .unwrap()
            .info(),
        expected
    );
    for fault in [Fault::Write(1), Fault::Commit] {
        let observed = Controlled::new(db.clone(), fault);
        let count = observed.mutations.clone();
        assert!(
            !Database::from_store(observed, &config(files.path()))
                .unwrap()
                .info()
                .created
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn memory_genesis_is_atomic_at_every_write_and_reopen_is_read_only() {
    failures(MemoryStore::new());
}

#[test]
fn mdbx_genesis_is_atomic_at_every_write_and_reopen_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    failures(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn physical_limits_cover_full_index_keys_fixed_values_and_size_overflow() {
    let files = tempfile::tempdir().unwrap();
    let mut cfg = config(files.path());
    for (name, string, bytes, max_key, max_value) in [
        (0, 64, 128, usize::MAX, usize::MAX),
        (32, 64, 128, 97, usize::MAX), // index needs 32 + 2 + 64
        (32, 0, 128, 65, usize::MAX),  // u256 needs 32 bytes even with zero string cap
        (32, 64, 16384, 98, 16384),    // cell tag needs one more byte
        (32, 64, 128, 98, 16383),      // engine rows need their own allowance
        (u32::MAX, u32::MAX, u32::MAX, u32::MAX as usize, usize::MAX),
    ] {
        cfg.genesis.cell_limits = CellLimits {
            max_cell_name_len: name,
            max_str_len: string,
            max_bytes_len: bytes,
        };
        let db = MemoryStore::new();
        let mut bounded = Controlled::new(db.clone(), Fault::None);
        bounded.max_key = max_key;
        bounded.max_value = max_value;
        assert!(matches!(
            Database::from_store(bounded, &cfg),
            Err(OpenError::InvalidConfig(_))
        ));
        assert!(db.begin_write().unwrap().is_empty().unwrap());
    }
    let mut bounded = Controlled::new(MemoryStore::new(), Fault::None);
    bounded.max_key = 98;
    bounded.max_value = 16384;
    assert!(Database::from_store(bounded, &config(files.path())).is_ok());
}

#[test]
fn invalid_typed_immutable_configuration_is_rejected_before_mutation() {
    let files = tempfile::tempdir().unwrap();
    for change in 0..6 {
        let mut cfg = config(files.path());
        let immutable_data = cfg.genesis.immutable_data.as_mut().unwrap();
        match change {
            0 => immutable_data.shard_span = 0,
            1 => immutable_data.segments[0].columns.clear(),
            2 => immutable_data.segments[0].name = "#system".into(),
            3 => {
                let segment = immutable_data.segments[0].clone();
                immutable_data.segments.push(segment);
            }
            4 => immutable_data.segments[0].columns[0].name = "x".repeat(16 * 1024),
            _ => immutable_data.segments.clear(),
        }
        let store = MemoryStore::new();
        let controlled = Controlled::new(store.clone(), Fault::None);
        let mutations = controlled.mutations.clone();
        assert!(matches!(
            Database::from_store(controlled, &cfg),
            Err(OpenError::InvalidConfig(_))
        ));
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        assert!(store.begin_write().unwrap().is_empty().unwrap());
    }
}
