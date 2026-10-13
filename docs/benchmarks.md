# Performance benchmarks

The `cells`, `merkle`, and `index` crates use Criterion. Benchmarks measure
execution time; correctness tests remain separate. Fixtures include sanity checks
outside timed sections. Inputs are deterministic and important inputs/outputs
pass through `std::hint::black_box` in the new suites.

## Running

```sh
cargo bench -p golemdb-cells --bench cells
cargo bench -p golemdb-merkle --bench merkle
cargo bench -p golemdb-index --bench index
cargo bench -p golemdb-index --bench index --features mdbx
cargo bench -p golemdb-integration-tests --bench batch
```

Full runs include many cases and can take several minutes. Select a group or
case using the filter after `--`:

```sh
cargo bench -p golemdb-merkle --bench merkle -- 'merkle/trie/keccak-256/path_bytes=6/replace'
cargo bench -p golemdb-index --bench index -- 'index/keccak-256/memory/chunks=64/apply_only/one_posting'
cargo bench -p golemdb-index --bench index --features mdbx -- 'transaction_begin_apply_durable_commit'
cargo bench -p golemdb-integration-tests --bench batch -- 'seal/create'
cargo bench -p golemdb-integration-tests --bench batch -- 'commit/'
GOLEMDB_BENCH_RECORDS=100000 cargo bench -p golemdb-integration-tests --bench batch
```

Compile without running, or execute every fixture once without collecting timing
statistics:

```sh
cargo bench --workspace --all-features --no-run
cargo bench -p golemdb-merkle --bench merkle -- --test
cargo bench -p golemdb-index --bench index --features mdbx -- --test
```

Criterion writes results under `target/criterion` (ignored by Git). For a deliberate
comparison, use the same command, features and filter with `--save-baseline before`,
then after the change use `--baseline before`. Keep machine, build settings and
background load comparable. CI compiles the suites; timing regressions are not
automatic pass/fail gates.

### Checking a change for regressions

Run the batch benchmark before and after your change. It ends with a summary that
lists each case's time and whether it got slower or faster than in the previous run,
then the bytes each commit case writes per commit:

```
Summary for 10000 records, compared with the previous run:
  seal/create       25.3 ms   no change
  commit/patch      15.4 ms   19% slower

Bytes written per commit, compared with the previous run:
  commit/patch      5.63 MiB  2.1% more
```

Times are CPU time, so waiting for the disk hardly affects them. They still vary
with CPU speed and other load, and runs of identical code can differ by ~10%, so
changes under 10% count as no change. Process CPU time adds up all threads, so
if seal or commit ever runs on several threads, a change that makes them faster
can show as no change or slower. The lines above the summary are Criterion's
detail: each time is shown as a 95% confidence interval (low, estimate, high),
which covers only the variation within that run.

Bytes written are counted over 10 commits made before timing starts, so the same
code writes the same bytes in every run, and any change is real. They catch a
change in write amplification even when it hardly moves the times. They count
what the process passes to the OS, before the filesystem and the disk add their
own writes, and are reported on Linux only.

Each run is compared with the previous one. For a careful comparison, save the run
without your change under a name and compare runs with your change against it.
Make both runs back to back, since times on the same machine can drift by ~10%
over a day:

```sh
cargo bench -p golemdb-integration-tests --bench batch -- --save-baseline before
cargo bench -p golemdb-integration-tests --bench batch -- --baseline before
```

## Cases

| Group | Workload |
| --- | --- |
| `merkle/branch` | Encode, strict decode, and hash branches with 2/8/16 leaf children and 0/31/63 prefix nibbles |
| `merkle/trie/<algorithm>/path_bytes=6` and `32` | Hit/miss lookup, full traversal, insertion, replacement and deletion at 64/1024 leaves |
| `index/container` | Canonical encoding, strict decoding and leaf hashing of sparse, dense-even, consecutive-run and full containers |
| `index/term` | String term construction, strict decoding and routing hash at 16/256/1024 value bytes |
| `index/<algorithm>/memory` and optional `index/<algorithm>/mdbx` | Materialization, four-term prefix scans, and posting batches at 64/256 containers per term |
| `index/<algorithm>/mdbx/transaction_begin_apply_durable_commit` | One posting change including writer acquisition and durable commit |
| `<N> records/seal/{create,patch,delete,mixed}` and `<N> records/commit/{patch,mixed}` | Sealing or committing a 200-operation batch through the public `Api`, on MDBX with N preloaded records |

Batch cases run on N preloaded records (`GOLEMDB_BENCH_RECORDS`, default 10,000).
Each case's batch is 200 creates, 200 patches or 200 deletes, or 70 creates, 60
patches and 70 deletes, so a regression shows which operation it affects. The
record shape is described in `golemdb_integration_tests::workload`. To add a
case, add a line to `CASES` in `benches/batch.rs`.

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
- Batch cases time only the `seal` or `commit` call, in CPU time; building and
  executing the batch happen outside timing. Commit times therefore leave out
  waiting for the disk. `seal` cases share one database and discard their batch.
  Each `commit` case gets its own database, whose record count stays at N while
  the file grows with every commit, so commits get slower as a case runs.
  Criterion picks how many iterations to run from their elapsed time, which
  includes waiting for the disk, so the number of commits, and with it the commit
  time, can vary between runs. A longer `--measurement-time` runs more commits
  and gives higher commit times, so compare only runs made with the same
  settings. Each commit case first makes the 10 commits that count bytes written.
  The suite uses 1 s of warm-up and 2 s of measurement per case, about 30 s in
  total; `--warm-up-time` and `--measurement-time` override them.

These suites collect timings, not allocation counts, database-size growth or
physical I/O statistics; the batch suite also reports bytes written per commit.
Existing storage-work tests count logical reads/writes.
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
cargo bench -p golemdb-index --bench index -- 'index/(keccak-256|blake3)/memory/chunks=64/apply_only/one_posting' --warm-up-time 0.5 --measurement-time 1 --sample-size 20
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
