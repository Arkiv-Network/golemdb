use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use golemdb_cells::CellType;
use golemdb_index::{BitmapContainer, INDEX_TRIE_PATH_BYTES, Index, IndexTerm, PostingChange};
use golemdb_merkle::{Blake3Hasher, HashProvider, Keccak256Hasher, RootRef};
use golemdb_storage::{MemoryStore, Store, WriteTransaction};

fn codecs(c: &mut Criterion) {
    let mut group = c.benchmark_group("index/container");
    for (name, values) in [
        ("sparse", (0..64).map(|i| i * 997).collect::<Vec<u16>>()),
        ("dense_even", (0..=u16::MAX).step_by(2).collect()),
        ("run", (1000..21000).collect()),
        ("full", (0..=u16::MAX).collect()),
    ] {
        let container = BitmapContainer::from_values(42, values).unwrap();
        let bytes = container.canonical_bytes().unwrap();
        assert_eq!(BitmapContainer::decode(&bytes).unwrap(), container);
        group.bench_function(BenchmarkId::new("canonical_encode", name), |b| {
            b.iter(|| black_box(&container).canonical_bytes().unwrap())
        });
        group.bench_function(BenchmarkId::new("strict_decode", name), |b| {
            b.iter(|| BitmapContainer::decode(black_box(&bytes)).unwrap())
        });
        // Includes canonical encoding, as required by the public leaf_hash API.
        fn hash_container<H: HashProvider>(
            group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
            name: &str,
            algorithm: &str,
            container: &BitmapContainer,
            hasher: &H,
        ) {
            group.bench_function(
                BenchmarkId::new(format!("leaf_hash_with_encoding/{algorithm}"), name),
                |b| b.iter(|| black_box(container).leaf_hash(hasher).unwrap()),
            );
        }
        hash_container(&mut group, name, "keccak-256", &container, &Keccak256Hasher);
        hash_container(&mut group, name, "blake3", &container, &Blake3Hasher);
    }
    group.finish();
    let mut group = c.benchmark_group("index/term");
    for length in [16, 256, 1024] {
        let value = vec![b'x'; length];
        let term = IndexTerm::new("name", CellType::Str, &value).unwrap();
        group.bench_function(BenchmarkId::new("encode_string", length), |b| {
            b.iter(|| IndexTerm::new("name", CellType::Str, black_box(&value)).unwrap())
        });
        group.bench_function(BenchmarkId::new("decode_string", length), |b| {
            b.iter(|| IndexTerm::decode(black_box(term.as_bytes())).unwrap())
        });
        fn hash_term<H: HashProvider>(
            group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
            length: usize,
            algorithm: &str,
            term: &IndexTerm,
            hasher: &H,
        ) {
            group.bench_function(
                BenchmarkId::new(format!("routing_hash/{algorithm}"), length),
                |b| b.iter(|| black_box(term).routing_path(hasher)),
            );
        }
        hash_term(&mut group, length, "keccak-256", &term, &Keccak256Hasher);
        hash_term(&mut group, length, "blake3", &term, &Blake3Hasher);
    }
    group.finish();
}

fn term(i: u8) -> IndexTerm {
    IndexTerm::new("tag", CellType::Str, &[b'a' + i]).unwrap()
}
fn add(t: u8, hi: u64, lo: u16) -> PostingChange {
    PostingChange::Add {
        term: term(t),
        record_id: (hi << 16) | u64::from(lo),
    }
}

fn seed(
    db: &impl Store,
    chunks: u64,
    hasher: &impl HashProvider,
) -> RootRef<INDEX_TRIE_PATH_BYTES> {
    let mut tx = db.begin_write().unwrap();
    let changes =
        (0..4).flat_map(|t| (0..chunks).flat_map(move |hi| (0..32).map(move |lo| add(t, hi, lo))));
    let root = Index::new(hasher)
        .apply(&mut tx, RootRef::Empty, changes)
        .unwrap()
        .root;
    tx.commit().unwrap();
    root
}

fn workloads(chunks: u64) -> Vec<(&'static str, Vec<PostingChange>)> {
    vec![
        ("one_posting", vec![add(0, 0, 128)]),
        (
            "same_chunk_64",
            (128..192).map(|lo| add(0, 0, lo)).collect(),
        ),
        (
            "across_chunks_64",
            (0..64).map(|hi| add(0, hi % chunks, 128)).collect(),
        ),
        (
            "across_terms_64",
            (0..4)
                .flat_map(|t| (0..16).map(move |hi| add(t, hi % chunks, 128)))
                .collect(),
        ),
        (
            "shuffled_across_terms_4096",
            (0..4096)
                .map(|i| {
                    // An odd multiplier permutes all 4096 inputs deterministically.
                    let key = (i * 2053) % 4096;
                    add(
                        (key / 1024) as u8,
                        (key / 16) % chunks,
                        128 + (key % 16) as u16,
                    )
                })
                .collect(),
        ),
        (
            "duplicate_heavy_4096",
            (0..2048)
                .flat_map(|i| {
                    let lo = 128 + (i % 16) as u16;
                    [
                        PostingChange::Remove {
                            term: term(0),
                            record_id: u64::from(lo),
                        },
                        add(0, 0, lo),
                    ]
                })
                .collect(),
        ),
    ]
}

fn backend(
    c: &mut Criterion,
    name: &str,
    db: &impl Store,
    chunks: u64,
    algorithm: &str,
    hasher: &impl HashProvider,
) {
    let index = Index::new(hasher);
    let root = seed(db, chunks, hasher);
    let read = db.begin_read().unwrap();
    let selected = term(0);
    let bitmap = index.bitmap(&read, &selected).unwrap().unwrap();
    assert!(bitmap.treemap().contains(0));
    assert!(!bitmap.treemap().contains(128));
    assert_eq!(bitmap.treemap().len(), chunks * 32);
    let mut group = c.benchmark_group(format!("index/{algorithm}/{name}/chunks={chunks}"));
    group.bench_function("materialize", |b| {
        b.iter(|| index.bitmap(&read, black_box(&selected)).unwrap())
    });
    let prefix = IndexTerm::prefix("tag", CellType::Str).unwrap();
    group.bench_function("scan_four_terms", |b| {
        b.iter(|| {
            for entry in index
                .terms_with_prefix(&read, black_box(prefix.clone()))
                .unwrap()
            {
                black_box(entry.unwrap());
            }
        })
    });
    // Read transaction lifetime is excluded from these warm-snapshot queries.
    drop(read);
    for (label, changes) in workloads(chunks) {
        let mut check = db.begin_write().unwrap();
        let updated = index.apply(&mut check, root, changes.clone()).unwrap();
        assert_ne!(updated.root, root);
        assert!(!updated.changed_terms.is_empty());
        check.abort();
        group.throughput(Throughput::Elements(changes.len() as u64));
        group.bench_function(BenchmarkId::new("apply_only", label), |b| {
            b.iter_batched_ref(
                || (db.begin_write().unwrap(), changes.clone()),
                |(tx, changes)| {
                    black_box(
                        index
                            .apply(tx, black_box(root), std::mem::take(changes))
                            .unwrap(),
                    )
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn durable_commit(c: &mut Criterion, algorithm: &str, hasher: &impl HashProvider) {
    let index = Index::new(hasher);
    c.bench_function(
        &format!("index/{algorithm}/mdbx/transaction_begin_apply_durable_commit"),
        |b| {
            // Fresh seeded environment for EVERY iteration. Setup and environment
            // destruction are excluded; begin/apply/commit are all inside timing.
            b.iter_batched_ref(
                || {
                    let dir = tempfile::tempdir().unwrap();
                    let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
                    let root = seed(&db, 8, hasher);
                    (db, dir, root, vec![add(0, 0, 128)])
                },
                |(db, _, root, changes)| {
                    let mut tx = db.begin_write().unwrap();
                    let update = index
                        .apply(&mut tx, black_box(*root), std::mem::take(changes))
                        .unwrap();
                    tx.commit().unwrap();
                    black_box(update)
                },
                BatchSize::PerIteration,
            );
        },
    );
}

fn algorithm(c: &mut Criterion, algorithm: &str, hasher: &impl HashProvider) {
    for chunks in [64, 256] {
        backend(c, "memory", &MemoryStore::new(), chunks, algorithm, hasher);
        {
            let dir = tempfile::tempdir().unwrap();
            let db = golemdb_storage_mdbx::MdbxStore::open(dir.path()).unwrap();
            backend(c, "mdbx", &db, chunks, algorithm, hasher);
        }
    }
    durable_commit(c, algorithm, hasher);
}

fn index(c: &mut Criterion) {
    codecs(c);
    algorithm(c, "keccak-256", &Keccak256Hasher);
    algorithm(c, "blake3", &Blake3Hasher);
}

criterion_group!(benches, index);
criterion_main!(benches);
