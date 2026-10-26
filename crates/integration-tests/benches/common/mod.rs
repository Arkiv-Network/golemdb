//! Setup shared by the benchmarks.

use golemdb_api::{
    Budget, CellLimits, Genesis, HashAlgorithm, OpenConfig, PricingConfig, RecordKey, RecordLimits,
};
use tempfile::TempDir;

/// Benchmarks measure the work itself, so operations are never cut short by gas.
pub const BUDGET: Budget = Budget::Unlimited;

/// On a real disk, unlike `/tmp`, which is often RAM-backed and hides writes.
const TMP: &str = env!("CARGO_TARGET_TMPDIR");

/// Keccak-256 with limits that fit every benchmark.
pub fn config() -> OpenConfig {
    let cells = CellLimits {
        max_cell_name_len: 32,
        max_str_len: 128,
        max_bytes_len: 128 * 1024,
    };
    let records = RecordLimits {
        max_cells: 16,
        max_indexed_cells: 16,
    };
    let no_immutable_data = None;
    OpenConfig::new(Genesis::new(
        HashAlgorithm::Keccak256,
        cells,
        records,
        no_immutable_data,
        PricingConfig::zero(),
    ))
}

/// A temporary directory under [`TMP`].
pub fn bench_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("golemdb-bench-")
        .tempdir_in(TMP)
        .unwrap()
}

/// Bit-reversed, so consecutive records get keys far apart, as hashes would.
pub fn key(n: u64) -> RecordKey {
    let mut key = [0; 32];
    key[..8].copy_from_slice(&n.reverse_bits().to_be_bytes());
    RecordKey(key)
}
