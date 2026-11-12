# Performance benchmarks

The `cells`, `merkle`, `index` and `query` suites use Criterion. Benchmarks measure
execution time; correctness tests remain separate. Fixtures include sanity checks
outside timed sections. Inputs are deterministic and important inputs/outputs
pass through `std::hint::black_box` in the new suites.

## Running

```sh
cargo bench -p golemdb-cells --bench cells
cargo bench -p golemdb-merkle --bench merkle
cargo bench -p golemdb-integration-tests --bench index
cargo bench -p golemdb-integration-tests --bench query
```

Full runs include many cases and can take several minutes. Select a group or
case using the filter after `--`:

```sh
cargo bench -p golemdb-merkle --bench merkle -- 'merkle/trie/keccak-256/path_bytes=6/replace'
cargo bench -p golemdb-integration-tests --bench index -- 'index/keccak-256/memory/chunks=64/apply_only/one_posting'
cargo bench -p golemdb-integration-tests --bench index -- 'transaction_begin_apply_durable_commit'
```

Compile without running, or execute every fixture once without collecting timing
statistics:

```sh
cargo bench --workspace --all-features --no-run
cargo bench -p golemdb-merkle --bench merkle -- --test
cargo bench -p golemdb-integration-tests --bench index -- --test
```

Criterion writes results under `target/criterion` (ignored by Git). For a deliberate
comparison, use the same command, features and filter with `--save-baseline before`,
then after the change use `--baseline before`. Keep machine, build settings and
background load comparable. CI compiles the suites; timing regressions are not
automatic pass/fail gates.

## Cases

| Group | Workload |
| --- | --- |
| `merkle/branch` | Encode, strict decode, and hash branches with 2/8/16 leaf children and 0/31/63 prefix nibbles |
| `merkle/trie/<algorithm>/path_bytes=6` and `32` | Hit/miss lookup, full traversal, insertion, replacement and deletion at 64/1024 leaves |
| `index/container` | Canonical encoding, strict decoding and leaf hashing of sparse, dense-even, consecutive-run and full containers |
| `index/term` | String term construction, strict decoding and routing hash at 16/256/1024 value bytes |
| `index/<algorithm>/memory` and optional `index/<algorithm>/mdbx` | Materialization, four-term prefix scans, and posting batches at 64/256 containers per term |
| `index/<algorithm>/mdbx/transaction_begin_apply_durable_commit` | One posting change including writer acquisition and durable commit |
| `query/range` | `number > X` matching the top 1% and 10% of its values, each value held by about five records; the 10% range with `LIMIT 10`, its `count`, and `skewed = 'small-1' AND` the range |
| `query/eq` | `quarter = 'q0'`, one term holding 25% of records, with the default page, `LIMIT 10` and `count`; `skewed = 'big' AND quarter = 'q0'`, two large terms |

Trie paths are either clustered (zero leading bytes and a unique four-byte suffix)
or spread (hash-derived leading bytes and the same unique suffix). Both widths
use the same ID sequence; six-byte spread paths have two hash-derived bytes.
These are controlled shapes, not a model of a production distribution.

Index fixtures contain four terms, each with 32 postings in every container.
Write cases add genuinely absent postings: one posting, 64 in one container,
64 across containers, or 64 across four terms. Additional cases cover 4096
shuffled postings across four terms and 4096 alternating removals/additions that
resolve to 16 new postings. Throughput for write groups is reported in input
posting changes per second, including duplicates. Deletes are covered by the Merkle
suite; the first index performance suite focuses on additions.

Query fixtures are 262,144 records (four bitmap containers of record IDs) on
MDBX with Keccak-256, created 1,000 per commit with deterministic `quarter`,
`skewed` and `number` attributes and no other cells.
Seeding takes about a minute and is excluded from timing. Query throughput is
reported in matches per second, the size of the whole match set, through the
public `Database` API on a warm page cache.

## Timing boundaries

- Codec/hash cases include the public operation's allocations. Container leaf
  hashing includes canonical encoding. Strict decoding includes its canonical
  re-encoding check. Branch hashing includes construction of its hash payload.
- Queries reuse an open read transaction and measure a warm snapshot. Bitmap
  materialization includes assembly; prefix scans decode rows without loading
  their bitmaps. Returned allocations are dropped as part of ordinary `iter` runs.
- Trie mutation and `apply_only` create a fresh write transaction outside timing.
  This excludes the memory backend's full state copy and MDBX writer acquisition.
  Input cloning is also outside timing. The measured work includes Merkle/storage
  operations and index batch grouping. Transaction teardown aborts outside timing.
- Mutation fixtures use `BatchSize::PerIteration`, so there is never more than
  one outstanding writer. Every iteration returns to the same committed state;
  no repeated insertion becomes a no-op and no immutable history grows indefinitely.
- The durable MDBX case creates a fresh temporary environment and seeds four
  terms with eight containers each outside timing. The timed section begins a
  transaction, applies one posting change and commits. Environment teardown and
  temporary-directory cleanup happen outside timing. It measures an isolated
  commit after seeding, not sustained ingestion or cold-disk throughput.

These suites collect timings, not allocation counts, database-size growth or
physical I/O statistics. Existing storage-work tests count logical reads/writes.
Future binary/256-way trie experiments should reuse the logical workloads but
add storage/proof-size measurements alongside timing comparisons.

## Comparing hash algorithms

Hashing and trie/index workloads run as separate generic instantiations for
`keccak-256` and `blake3`, with a fresh database and roots for each. Codec-only
cases run once. Merkle trie path fixtures always use Keccak-derived paths for
both algorithms, keeping topology fixed; leaf and branch digests use the selected
provider. Index fixtures use the same terms/postings, with routing naturally
changing with the protocol hash. These two comparisons isolate hash costs and
measure the complete index effect respectively.

`merkle/hash/<algorithm>/bytes/<size>` measures 32, 65, 512 and 8192-byte inputs.
BLAKE3 uses its normal SIMD-capable implementation without Rayon parallel hashing.
Raw hash speed is not a prediction of complete transaction speed.

For short comparisons across both providers:

```sh
cargo bench -p golemdb-merkle --bench merkle -- 'merkle/hash/' --warm-up-time 0.5 --measurement-time 1 --sample-size 20
cargo bench -p golemdb-integration-tests --bench index -- 'index/(keccak-256|blake3)/memory/chunks=64/apply_only/one_posting' --warm-up-time 0.5 --measurement-time 1 --sample-size 20
```

Algorithm-labelled groups replace older unlabelled benchmark names, so establish
new baselines. Use `--test` for fixture smoke checks without timing statistics;
full default runs now include both algorithms and take longer.

## Sorted-vector grouping review (2026-09-28)

The writer now sorts a flat vector stably by `(term, container, offset)` and
retains the last operation per posting. BitmapTrie consumes consecutive container
groups directly. This removes nested map allocations but retains all input
entries until deduplication; duplicate-heavy batches can cost more time and memory.

Local release-mode Keccak/MDBX measurements at 64 containers per term:

| Apply-only workload | Previous nested maps | Sorted vector |
| --- | ---: | ---: |
| One posting | 24.40 µs | 25.34 µs |
| 64 postings in one container | 28.85 µs | 27.49 µs |
| 4096 changes resolving to 16 postings | 111.53 µs | 251.18 µs |

These are preliminary local point estimates, not a general speedup claim. The
map baseline used 20 samples, 0.3 s warm-up and 1 s measurement; the vector
confirmation used 30 samples, 1 s warm-up and 2 s measurement. Transaction setup,
input cloning and abort are outside timing; apply includes grouping, hashing and
MDBX writes, but no durable commit. The first six-workload comparison also found
small slowdowns for changes spread across containers/terms. Preserve both the
shuffled and duplicate-heavy workloads when evaluating further optimizations.
