use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use golemdb_api::*;
use golemdb_storage::{MemoryStore, StorageError, Store};

/// Clones share the wrapped store and the switches.
#[derive(Clone)]
struct FailingStore<S> {
    database: S,
    fail_reads: Arc<AtomicBool>,
    fail_writes: Arc<AtomicBool>,
}

impl<S: Store> Store for FailingStore<S> {
    type Read<'a>
        = S::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = S::Write<'a>
    where
        Self: 'a;
    fn max_key_size(&self) -> usize {
        self.database.max_key_size()
    }
    fn max_value_size(&self) -> usize {
        self.database.max_value_size()
    }
    fn begin_read(&self) -> golemdb_storage::Result<Self::Read<'_>> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(injected());
        }
        self.database.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(injected());
        }
        self.database.begin_write()
    }
}

fn injected() -> StorageError {
    StorageError::Backend(std::io::Error::other("injected facade storage failure").into())
}

fn diagnostic(error: ApiError) {
    assert!(matches!(error, ApiError::Internal { .. }));
    assert!(
        error
            .source()
            .unwrap()
            .to_string()
            .contains("injected facade storage failure")
    );
    let mut source = error.source();
    while let Some(cause) = source {
        if cause.downcast_ref::<std::io::Error>().is_some() {
            return;
        }
        source = cause.source();
    }
    panic!("the backend diagnostic must survive the facade's error conversion");
}

fn failure_contract(database: impl Store + Clone + Send + Sync + 'static) {
    let fail_reads = Arc::new(AtomicBool::new(false));
    let fail_writes = Arc::new(AtomicBool::new(false));
    let config = OpenConfig::new(Genesis::new(
        HashAlgorithm::Blake3,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        golemdb_api::RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
        golemdb_api::PricingConfig::zero(),
    ));
    let db = Database::from_store(
        FailingStore {
            database,
            fail_reads: fail_reads.clone(),
            fail_writes: fail_writes.clone(),
        },
        &config,
    )
    .unwrap();
    let key = RecordKey([0x42; 32]);
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))
            .attribute("price", CellValue::from_i32(50)),
    )
    .into_result()
    .unwrap();
    let pending = db
        .get(
            ReadTarget::Branch(branch),
            RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result()
        .unwrap();
    let info = db.branch_info(branch).unwrap();
    fail_reads.store(true, Ordering::SeqCst);
    diagnostic(
        db.get(
            ReadTarget::Head,
            RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX)),
        )
        .into_result()
        .unwrap_err(),
    );
    diagnostic(
        db.patch(
            branch,
            RecordOp::patch(key, golemdb_api::Budget::Limited(u64::MAX)).remove("price"),
        )
        .into_result()
        .unwrap_err(),
    );
    diagnostic(db.head().unwrap_err());
    fail_reads.store(false, Ordering::SeqCst);
    assert_eq!(db.branch_info(branch).unwrap(), info);
    assert_eq!(
        db.get(
            ReadTarget::Branch(branch),
            RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))
        )
        .into_result()
        .unwrap(),
        pending
    );

    fail_writes.store(true, Ordering::SeqCst);
    diagnostic(db.commit(branch).unwrap_err());
    assert_eq!(db.head().unwrap(), golemdb_branch::CommitId::new(0));
    assert!(db.branch_info(branch).unwrap().sealed);
    let sealed = db.seal(branch).unwrap();
    fail_writes.store(false, Ordering::SeqCst);
    assert_eq!(db.commit(branch).unwrap(), sealed.commit_id);
    assert_eq!(
        db.get(
            ReadTarget::Head,
            RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))
        )
        .into_result()
        .unwrap(),
        pending
    );
}

#[test]
fn memory_facade_retains_error_sources_and_can_retry_failed_commit() {
    failure_contract(MemoryStore::new());
}

#[test]
fn mdbx_facade_retains_error_sources_and_can_retry_failed_commit() {
    let dir = tempfile::tempdir().unwrap();
    failure_contract(golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap());
}

#[test]
fn full_mdbx_store_preserves_head_tables_and_sealed_branch() {
    let dir = tempfile::tempdir().unwrap();
    let store = golemdb_storage_mdbx::MdbxStore::open_with_options(
        dir.path(),
        MdbxOptions {
            max_map_size: 1024 * 1024,
            growth_step: 65536,
            ..Default::default()
        },
    )
    .unwrap();
    let genesis = Genesis::new(
        HashAlgorithm::Blake3,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 2 * 1024 * 1024,
        },
        golemdb_api::RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        None,
        golemdb_api::PricingConfig::zero(),
    );
    let db = Database::from_store(store.clone(), &OpenConfig::new(genesis)).unwrap();
    let before = super::opening::snapshot(&store);
    let key = RecordKey([0x42; 32]);
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))
            .field("payload", CellValue::from_bytes(&vec![1; 2 * 1024 * 1024])),
    )
    .into_result()
    .unwrap();
    let sealed = db.seal(branch).unwrap();
    for _ in 0..2 {
        assert!(matches!(db.commit(branch), Err(ApiError::StoreFull)));
        assert_eq!(db.head().unwrap(), CommitId::GENESIS);
        assert_eq!(super::opening::snapshot(&store), before);
        assert_eq!(db.seal(branch).unwrap(), sealed);
        assert!(db.branch_info(branch).unwrap().sealed);
        assert_eq!(
            db.get(
                ReadTarget::Branch(branch),
                RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))
            )
            .into_result()
            .unwrap()
            .cells[b"payload".as_slice()]
            .as_bytes()
            .unwrap()
            .len(),
            2 * 1024 * 1024
        );
        assert!(matches!(
            db.get(
                ReadTarget::Head,
                RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))
            )
            .into_result(),
            Err(ApiError::NotFound)
        ));
    }
    db.discard(branch).unwrap();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))
            .field("payload", CellValue::from_bytes(b"small")),
    )
    .into_result()
    .unwrap();
    assert_eq!(db.commit(branch).unwrap(), CommitId::new(1));
}
