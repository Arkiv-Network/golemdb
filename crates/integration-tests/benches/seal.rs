//! Seal latency over a preloaded MDBX database. Each iteration stages the
//! same batch on a fresh branch, times only `seal`, then discards the branch,
//! so the database and the measured work stay identical across samples.
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use golemdb_branch::{Branches, CommitId, Head, write_head};
use golemdb_cells::{CellKey, CellNameRef, CellType, CellValue, CellValueRef};
use golemdb_merkle::{HashProvider, Keccak256Hasher};
use golemdb_storage::{Store, WriteTransaction};
use golemdb_storage_mdbx::MdbxStore;

const RECORDS: u64 = 20_000;
const FIRST: u64 = 64;

fn put(id: u64, name: &str, text: &str, indexed: bool) -> (CellKey, CellValue) {
    (
        CellKey::new(id, CellNameRef::raw(name.as_bytes())),
        CellValueRef::new(CellType::Str, text.as_bytes(), indexed)
            .unwrap()
            .into(),
    )
}

fn record(id: u64, round: u64) -> [(CellKey, CellValue); 4] {
    [
        put(id, "status", &format!("s{}", (id + round) % 4), true),
        put(id, "owner", &format!("o{}", (id * 7 + round) % 1000), true),
        put(id, "tag", &format!("t{id}"), true),
        put(id, "payload", &"x".repeat(256), false),
    ]
}

// xorshift: deterministic targets without an RNG dependency.
fn targets(count: usize, mut state: u64) -> impl Iterator<Item = u64> {
    std::iter::repeat_with(move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    })
    .take(count)
}

fn seal(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let db = MdbxStore::open(dir.path()).unwrap();
    // Minimal empty-head fixture for this low-level cell benchmark.
    let mut tx = db.begin_write().unwrap();
    let empty = Keccak256Hasher.hash(&[]);
    write_head(
        &mut tx,
        &Head {
            commit_id: CommitId::new(0),
            state_root: empty,
            index_root: empty,
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let branches = Branches::new(db, Keccak256Hasher).unwrap();
    for batch in (FIRST..FIRST + RECORDS).collect::<Vec<_>>().chunks(1000) {
        let branch = branches.begin().unwrap();
        branches
            .write(branch, |cells| {
                for &id in batch {
                    for (key, value) in record(id, 0) {
                        cells.put(key, value);
                    }
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        branches.commit(branch).unwrap();
    }
    let mut group = c.benchmark_group("branch/seal");
    group.sample_size(20);
    for ops in [100, 1000] {
        // Half patch indexed fields, a quarter delete, a quarter create.
        let stage = |branch| {
            branches
                .write(branch, |cells| {
                    for (i, target) in targets(ops, 0x9e37_79b9_7f4a_7c15).enumerate() {
                        let id = FIRST + target % RECORDS;
                        match i % 4 {
                            0 | 1 => {
                                let [status, owner, ..] = record(id, 1);
                                for (key, value) in [status, owner] {
                                    cells.put(key, value);
                                }
                            }
                            2 => {
                                for (key, _) in record(id, 0) {
                                    cells.delete(key);
                                }
                            }
                            _ => {
                                for (key, value) in record(FIRST + RECORDS + i as u64, 0) {
                                    cells.put(key, value);
                                }
                            }
                        }
                    }
                    Ok::<_, ()>(())
                })
                .unwrap();
        };
        group.bench_function(BenchmarkId::new("indexed", ops), |b| {
            b.iter_custom(|iterations| {
                let mut total = Duration::ZERO;
                for _ in 0..iterations {
                    let branch = branches.begin().unwrap();
                    stage(branch);
                    let start = Instant::now();
                    branches.seal(branch).unwrap();
                    total += start.elapsed();
                    branches.discard(branch).unwrap();
                }
                total
            })
        });
    }
    group.finish();
}

criterion_group!(benches, seal);
criterion_main!(benches);
