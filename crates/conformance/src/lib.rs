//! Shared setup for the spec conformance tests in `tests/spec`.
//!
//! Everything here goes through the public `Api` only.

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

/// The spec's example genesis, with `limits` and priced by `pricing`.
pub fn genesis(limits: CellLimits, pricing: PricingConfig) -> Genesis {
    Genesis::new(
        HashAlgorithm::Keccak256,
        limits,
        RECORD_LIMITS,
        None,
        pricing,
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
    /// A fresh database created from [`genesis`] with [`LIMITS`] and zero pricing.
    pub fn new() -> Self {
        Self::open(LIMITS, PricingConfig::zero())
    }

    /// A fresh database created from [`genesis`] with `limits` and `pricing`.
    pub fn open(limits: CellLimits, pricing: PricingConfig) -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let db = Database::open(
            dir.path(),
            &OpenConfig::new(genesis(limits, pricing)).with_mode(OpenMode::CreateNew),
        )
        .expect("open MDBX database");
        Self { db, _dir: dir }
    }

    /// A full read of `key` at `target`.
    #[track_caller]
    pub fn read(&self, target: ReadTarget, key: RecordKey) -> Record {
        self.get(target, RecordOp::get(key, B)).result.unwrap()
    }

    /// `#nextRecordID` as `branch` sees it.
    #[track_caller]
    pub fn next_record_id(&self, branch: BranchId) -> Option<u64> {
        self.read(ReadTarget::Branch(branch), reserved_key("#alloc"))
            .cells
            .get("#nextRecordID".as_bytes())?
            .as_u64()
    }

    /// The record ID `#recordKeys` binds `k` to, as `branch` sees it.
    #[track_caller]
    pub fn record_id(&self, branch: BranchId, k: RecordKey) -> Option<u64> {
        self.get(
            ReadTarget::Branch(branch),
            RecordOp::get(reserved_key("#recordKeys"), B).only([k.0.as_slice()]),
        )
        .result
        .unwrap()
        .cells
        .get(k.0.as_slice())?
        .as_u64()
    }

    /// The number of undo entries `branch` retains: rollback lowers it, and an
    /// identical write to an existing overlay entry adds none.
    #[track_caller]
    pub fn version(&self, branch: BranchId) -> u64 {
        self.branch_info(branch).unwrap().version
    }

    /// Seals `branch` and returns its state and index roots.
    #[track_caller]
    pub fn sealed_roots(&self, branch: BranchId) -> ([u8; 32], [u8; 32]) {
        let sealed = self.seal(branch).unwrap();
        (sealed.state_root, sealed.index_root)
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

/// A user record key, for `n ≥ 1`: never equal to a catalogue key, which is ASCII
/// zero-padded. `key(0)` is the all-zero key, which the spec leaves open.
pub fn key(n: u8) -> RecordKey {
    RecordKey([n; 32])
}

/// A record's cells as `(name, value)` pairs, in name order.
pub fn cells(record: &Record) -> Vec<(&[u8], &CellValue)> {
    record
        .cells
        .iter()
        .map(|(name, value)| (name.as_bytes(), value))
        .collect()
}

/// A record's cell names, in name order.
pub fn cell_names(record: &Record) -> Vec<&[u8]> {
    record.cells.keys().map(|name| name.as_bytes()).collect()
}

/// A record's user cells, without `#key` and `#meta`, in name order.
pub fn user_cells(record: &Record) -> Vec<(&[u8], &CellValue)> {
    cells(record)
        .into_iter()
        .filter(|(name, _)| !name.starts_with(b"#"))
        .collect()
}

/// `value` as an attribute, the kind `.attribute` stores.
pub fn attribute(value: CellValue) -> CellValue {
    value.with_kind(CellKind::Attribute).unwrap()
}

/// The names of the catalogue's reserved records.
pub const RESERVED_NAMES: [&str; 7] = [
    "#params",
    "#alloc",
    "#roots",
    "#recordKeys",
    "#rootIndex",
    "@meteringModel",
    "@modelWeight",
];

/// A reserved record's key: its name, ASCII, zero-padded to 32 bytes.
pub fn reserved_key(name: &str) -> RecordKey {
    let mut key = [0; 32];
    key[..name.len()].copy_from_slice(name.as_bytes());
    RecordKey(key)
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
