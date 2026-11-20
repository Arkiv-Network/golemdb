//! Shared setup for the spec conformance tests in `tests/spec`.
//!
//! Everything here goes through the public `Api` only: a test writes with
//! `create` / `patch` / `delete` and checks by reading back with `get`.

pub use golemdb_api::*;

/// The limits of the spec's development genesis: 32 / 64 / 128.
pub const LIMITS: CellLimits = CellLimits {
    max_cell_name_len: 32,
    max_str_len: 64,
    max_bytes_len: 128,
};

/// Per-record caps, roomy enough for every test record.
pub const RECORD_LIMITS: RecordLimits = RecordLimits {
    max_cells: 32,
    max_indexed_cells: 24,
};

/// The budget every call states: tests check behaviour, not metering.
pub const B: Budget = Budget::Unlimited;

/// The spec's example genesis, with zero pricing.
pub fn genesis() -> Genesis {
    Genesis::new(
        HashAlgorithm::Keccak256,
        LIMITS,
        RECORD_LIMITS,
        None,
        PricingConfig::zero(),
    )
}

/// A fresh database on MDBX, the store hosts run on, in its own temporary
/// directory. Derefs to `Database`; the directory is removed after the database
/// is dropped.
pub struct TestDb {
    db: Database,
    _dir: tempfile::TempDir,
}

impl TestDb {
    /// A fresh database created from [`genesis`].
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let db = Database::open(
            dir.path(),
            &OpenConfig::new(genesis()).with_mode(OpenMode::CreateNew),
        )
        .expect("open MDBX database");
        Self { db, _dir: dir }
    }

    /// A full read of `key` at `target`.
    #[track_caller]
    pub fn read(&self, target: ReadTarget, key: RecordKey) -> Record {
        self.get(target, RecordOp::get(key, B)).result.unwrap()
    }
}

impl Default for TestDb {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for TestDb {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}

/// A user record key. Never equal to a reserved key, which is ASCII zero-padded.
pub fn key(n: u8) -> RecordKey {
    RecordKey([n; 32])
}

/// Asserts that an expression is an `Err` matching a pattern, e.g.
/// `assert_err!(db.get(..).result, ApiError::NotFound)`.
#[macro_export]
macro_rules! assert_err {
    ($result:expr, $pattern:pat $(if $guard:expr)?) => {
        match $result {
            Err($pattern) $(if $guard)? => {}
            Ok(_) => panic!("expected Err({}), got Ok(..)", stringify!($pattern)),
            Err(e) => panic!("expected Err({}), got Err({e:?})", stringify!($pattern)),
        }
    };
}
