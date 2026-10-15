use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use golemdb_merkle::{
    Blake3Hasher, BranchNodeCompact, HashProvider, Keccak256Hasher, LeafRef, RootRef, Trie,
};
use golemdb_storage::{MemoryStore, Store, Table, WriteTransaction};

const HASH: Keccak256Hasher = Keccak256Hasher;
const BENCH_LEAF_DOMAIN: u8 = 0x04;
const BENCH_BRANCH_DOMAIN: u8 = 0x05;
const BENCH_NODE_DOMAIN: u8 = 0x03;

const TABLE: Table = Table("BenchBranches");

fn branches(c: &mut Criterion) {
    let mut group = c.benchmark_group("merkle/branch");
    for count in [2, 8, 16] {
        for prefix_len in [0, 31, 63] {
            let mut prefix = vec![0xaa; (prefix_len as usize).div_ceil(2)];
            if prefix_len % 2 == 1 {
                *prefix.last_mut().unwrap() &= 0xf0;
            }
            let mut paths = Vec::new();
            for slot in 0..count {
                let mut path = [0xaa; 32];
                let i = prefix_len as usize / 2;
                if prefix_len % 2 == 0 {
                    path[i] = (slot << 4) | 0x0a;
                } else {
                    path[i] = 0xa0 | slot;
                }
                paths.push(path);
            }
            let hashes = paths.iter().map(|p| HASH.hash(p)).collect();
            let mask = ((1u32 << count) - 1) as u16;
            let node = BranchNodeCompact::new(prefix, prefix_len, mask, 0, hashes, paths).unwrap();
            let bytes = node.encode();
            assert_eq!(BranchNodeCompact::<32>::decode(&bytes).unwrap(), node);
            let parameter = format!("children={count}/prefix={prefix_len}");
            group.bench_with_input(BenchmarkId::new("encode", &parameter), &node, |b, node| {
                b.iter(|| black_box(node).encode())
            });
            group.bench_with_input(
                BenchmarkId::new("decode", &parameter),
                &bytes,
                |b, bytes| b.iter(|| BranchNodeCompact::<32>::decode(black_box(bytes)).unwrap()),
            );
            fn hash_node<H: HashProvider>(
                group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
                parameter: &str,
                name: &str,
                node: &BranchNodeCompact<32>,
                hasher: &H,
            ) {
                group.bench_function(BenchmarkId::new(format!("hash/{name}"), parameter), |b| {
                    b.iter(|| black_box(node).hash(BENCH_NODE_DOMAIN, hasher))
                });
            }
            hash_node(
                &mut group,
                &parameter,
                "keccak-256",
                &node,
                &Keccak256Hasher,
            );
            hash_node(&mut group, &parameter, "blake3", &node, &Blake3Hasher);
        }
    }
    group.finish();
}

fn leaf<const N: usize>(id: u32, clustered: bool, hasher: &impl HashProvider) -> LeafRef<N> {
    let digest = HASH.hash(&id.to_be_bytes());
    let mut path = [0; N];
    if !clustered {
        path.copy_from_slice(&digest[..N]);
    }
    // Unique keys even for six-byte paths; clustered keys share the leading bytes.
    path[N - 4..].copy_from_slice(&id.to_be_bytes());
    LeafRef {
        path,
        hash: hasher.hash_parts(&[&[BENCH_LEAF_DOMAIN], &path]),
    }
}

fn tries<const N: usize>(c: &mut Criterion, algorithm: &str, hasher: &impl HashProvider) {
    let mut group = c.benchmark_group(format!("merkle/trie/{algorithm}/path_bytes={N}"));
    for count in [64, 1024] {
        for clustered in [false, true] {
            let db = MemoryStore::new();
            let trie = Trie::<_, N>::new(TABLE, BENCH_BRANCH_DOMAIN, hasher);
            let mut tx = db.begin_write().unwrap();
            let mut root = RootRef::Empty;
            for i in 0..count {
                root = trie
                    .insert(&mut tx, root, leaf(i, clustered, hasher))
                    .unwrap();
            }
            tx.commit().unwrap();
            let read = db.begin_read().unwrap();
            let existing = leaf(count / 2, clustered, hasher);
            let absent = leaf(count, clustered, hasher);
            assert_eq!(
                trie.get(&read, root, &existing.path).unwrap(),
                Some(existing)
            );
            assert!(trie.get(&read, root, &absent.path).unwrap().is_none());
            let parameter = format!(
                "leaves={count}/{}",
                if clustered { "clustered" } else { "spread" }
            );
            for (name, path) in [("get_hit", existing.path), ("get_miss", absent.path)] {
                group.bench_function(BenchmarkId::new(name, &parameter), |b| {
                    b.iter(|| trie.get(&read, black_box(root), black_box(&path)).unwrap())
                });
            }
            group.bench_function(BenchmarkId::new("walk", &parameter), |b| {
                b.iter(|| {
                    for leaf in trie.walk(&read, black_box(root)) {
                        black_box(leaf.unwrap());
                    }
                })
            });
            for operation in ["insert", "replace", "delete"] {
                let mut replacement = existing;
                replacement.hash = hasher.hash(b"replacement payload");
                // Verify fixture semantics outside measurements.
                let mut check = db.begin_write().unwrap();
                let changed = match operation {
                    "insert" => trie.insert(&mut check, root, absent),
                    "replace" => trie.insert(&mut check, root, replacement),
                    _ => trie.remove(&mut check, root, &existing.path),
                }
                .unwrap();
                assert_ne!(changed, root);
                check.abort();
                group.bench_function(BenchmarkId::new(operation, &parameter), |b| {
                    // One live writer at a time. Setup copies memory state outside
                    // timing; teardown aborts it, preventing growth and no-op drift.
                    b.iter_batched_ref(
                        || db.begin_write().unwrap(),
                        |tx| {
                            black_box(
                                match operation {
                                    "insert" => trie.insert(tx, black_box(root), black_box(absent)),
                                    "replace" => {
                                        trie.insert(tx, black_box(root), black_box(replacement))
                                    }
                                    _ => {
                                        trie.remove(tx, black_box(root), black_box(&existing.path))
                                    }
                                }
                                .unwrap(),
                            )
                        },
                        BatchSize::PerIteration,
                    )
                });
            }
        }
    }
    group.finish();
}

// Input sizes cover small preimages, full branches and bitmap payloads.
fn hash_inputs(c: &mut Criterion, algorithm: &str, hasher: &impl HashProvider) {
    let mut group = c.benchmark_group(format!("merkle/hash/{algorithm}"));
    for size in [32, 65, 512, 8192] {
        let bytes = vec![0xa5; size];
        group.throughput(criterion::Throughput::Bytes(size as u64));
        group.bench_function(BenchmarkId::new("bytes", size), |b| {
            b.iter(|| hasher.hash(black_box(&bytes)))
        });
    }
    group.finish();
}

fn merkle(c: &mut Criterion) {
    branches(c);
    tries::<6>(c, "keccak-256", &Keccak256Hasher);
    tries::<32>(c, "keccak-256", &Keccak256Hasher);
    tries::<6>(c, "blake3", &Blake3Hasher);
    tries::<32>(c, "blake3", &Blake3Hasher);
    hash_inputs(c, "keccak-256", &Keccak256Hasher);
    hash_inputs(c, "blake3", &Blake3Hasher);
}
criterion_group!(benches, merkle);
criterion_main!(benches);
