use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use golemdb_api::*;
use golemdb_storage::{MemoryStore, Store};

/// Clones share the wrapped store and the switches.
#[derive(Clone)]
struct Guarded<S> {
    database: S,
    forbid_io: Arc<AtomicBool>,
}

impl<S: Store> Store for Guarded<S> {
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
        assert!(
            !self.forbid_io.load(Ordering::SeqCst),
            "stub must not read storage"
        );
        self.database.begin_read()
    }
    fn begin_write(&self) -> golemdb_storage::Result<Self::Write<'_>> {
        assert!(
            !self.forbid_io.load(Ordering::SeqCst),
            "stub must not write storage"
        );
        self.database.begin_write()
    }
}

fn unsupported<T: Debug>(result: Result<T>, expected: &str) {
    match result.unwrap_err() {
        ApiError::NotImplemented { operation } => assert_eq!(operation, expected),
        other => panic!("expected NotImplemented, got {other:?}"),
    }
}

fn check(api: &dyn Api, branch: BranchId) {
    for segment in ["headers", "unknown", ""] {
        for key in [None, Some(ImmutableDataKey([0x42; 32]))] {
            for row in [vec![], vec![b"header".to_vec(), vec![0, 255]]] {
                unsupported(
                    api.immutable_data_append(branch, segment, key, row),
                    "immutable_data_append",
                );
            }
        }
        for address in [
            ImmutableDataAddress::Ordinal(0),
            ImmutableDataAddress::Ordinal(u64::MAX),
            ImmutableDataAddress::Key(ImmutableDataKey([0x42; 32])),
        ] {
            unsupported(
                api.immutable_data_get(segment, address),
                "immutable_data_get",
            );
        }
        for commit in [0, u64::MAX] {
            unsupported(
                api.immutable_data_range_of(segment, CommitId::new(commit)),
                "immutable_data_range_of",
            );
            unsupported(
                api.immutable_data_rows_of(segment, CommitId::new(commit)),
                "immutable_data_rows_of",
            );
        }
    }
}

fn contract(database: impl Store + Clone + Send + Sync + 'static, hash_function: HashAlgorithm) {
    let forbid_io = Arc::new(AtomicBool::new(false));
    let config = OpenConfig::new(Genesis::new(
        hash_function,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 128,
        },
        golemdb_api::ImmutableDataConfig::new(500_000, vec![]),
    ));
    let db = Database::from_store(
        Guarded {
            database,
            forbid_io: forbid_io.clone(),
        },
        &config,
    )
    .unwrap();
    let key = RecordKey([1; 32]);
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(key)
            .field("price", CellValue::from_i32(50))
            .unwrap(),
    )
    .unwrap();
    let record = db
        .get(ReadTarget::Branch(branch), RecordOp::get(key))
        .unwrap();
    let info = db.branch_info(branch).unwrap();
    let sealed_branch = db.begin().unwrap();
    let seal = db.seal(sealed_branch).unwrap();
    let sealed_info = db.branch_info(sealed_branch).unwrap();
    let api: Arc<dyn Api + Send + Sync> = Arc::new(db.clone());

    forbid_io.store(true, Ordering::SeqCst);
    for id in [
        branch,
        sealed_branch,
        golemdb_branch::BranchId::new(u64::MAX),
    ] {
        check(api.as_ref(), id);
    }
    forbid_io.store(false, Ordering::SeqCst);
    assert_eq!(db.head().unwrap(), golemdb_branch::CommitId::new(0));
    assert_eq!(db.branch_info(branch).unwrap(), info);
    assert_eq!(db.branch_info(sealed_branch).unwrap(), sealed_info);
    assert_eq!(db.seal(sealed_branch).unwrap(), seal);
    assert_eq!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(key))
            .unwrap(),
        record
    );
    assert_eq!(
        db.commit(sealed_branch).unwrap(),
        golemdb_branch::CommitId::new(1)
    );

    // Unsupported calls must not validate/invalidate stale handles either.
    forbid_io.store(true, Ordering::SeqCst);
    check(api.as_ref(), branch);
    check(api.as_ref(), sealed_branch);
    forbid_io.store(false, Ordering::SeqCst);
    assert!(matches!(db.commit(branch), Err(ApiError::Conflict)));
    assert_eq!(db.head().unwrap(), golemdb_branch::CommitId::new(1));
}

#[test]
fn memory_stubs_return_explicit_errors_without_io_or_state_changes() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        contract(MemoryStore::new(), hash);
    }
}

#[test]
fn mdbx_stubs_return_explicit_errors_without_io_or_state_changes() {
    for hash in [HashAlgorithm::Keccak256, HashAlgorithm::Blake3] {
        let directory = tempfile::tempdir().unwrap();
        contract(
            golemdb_storage_mdbx::MdbxStore::open(directory.path()).unwrap(),
            hash,
        );
    }
}
