# Performance benchmarks

The `cells` and `merkle` crates, and the `integration-tests` `index`, `write`
and `query` suites, use Criterion. Benchmarks measure execution time;
correctness tests remain separate. Fixtures include sanity checks outside timed
sections. Inputs are deterministic and important inputs/outputs pass through
`std::hint::black_box` in the new suites.

## Running

```sh
cargo bench -p golemdb-cells --bench cells
cargo bench -p golemdb-merkle --bench merkle
cargo bench -p golemdb-integration-tests --bench index
cargo bench -p golemdb-integration-tests --bench write
cargo bench -p golemdb-integration-tests --bench query
```

Full runs include many cases and can take several minutes. Select a group or
case using the filter after `--`:

```sh
cargo bench -p golemdb-merkle --bench merkle -- 'merkle/trie/keccak-256/path_bytes=6/replace'
cargo bench -p golemdb-integration-tests --bench index -- 'index/keccak-256/memory/chunks=64/apply_only/one_posting'
cargo bench -p golemdb-integration-tests --bench index -- 'transaction_begin_apply_durable_commit'
cargo bench -p golemdb-integration-tests --bench write -- 'seal/create'
cargo bench -p golemdb-integration-tests --bench write -- 'commit/'
cargo bench -p golemdb-integration-tests --bench query -- 'query/100k/eq'
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

### Checking a change for regressions

Save a run of the write benchmark without your change, then compare runs with
your change against it. Make the two runs back to back. Runs of identical code
can differ by ~10%, even on one machine over a day, so write cases treat
smaller changes as noise. Without these flags, each run is compared with the
previous one.

```sh
cargo bench -p golemdb-integration-tests --bench write -- --save-baseline before
cargo bench -p golemdb-integration-tests --bench write -- --baseline before
```

After each commit case, the suite prints its storage work:

```
commit/patch: 2772992 bytes written and 1 sync(s) per commit, database 499 MiB
```

It comes from MDBX's own counters (`Store::stats`). Bytes written are estimated
from the pages a commit creates or copies; it also counts syncs and the
database's size. The same code reports the same numbers in every run, on every machine, so
any difference between two runs is real. Nothing compares them automatically:
compare the lines of the two runs. It catches a change in write
amplification even when it hardly moves the times.

## Cases

| Group | Workload |
| --- | --- |
| `merkle/branch` | Encode, strict decode, and hash branches with 2/8/16 leaf children and 0/31/63 prefix nibbles |
| `merkle/trie/<algorithm>/path_bytes=6` and `32` | Hit/miss lookup, full traversal, insertion, replacement and deletion at 64/1024 leaves |
| `index/container` | Canonical encoding, strict decoding and leaf hashing of sparse, dense-even, consecutive-run and full containers |
| `index/term` | String term construction, strict decoding and routing hash at 16/256/1024 value bytes |
| `index/<algorithm>/memory` and `index/<algorithm>/mdbx` | Materialization, four-term prefix scans, and posting batches at 64/256 containers per term |
| `index/<algorithm>/mdbx/transaction_begin_apply_durable_commit` | One posting change including writer acquisition and durable commit |
| `write/100k/{seal,commit}/{create,patch,delete,mixed}` | Sealing or committing a 200-operation batch through the public `Api`, on MDBX with 100,000 records |
| `query/100k/range` | `number > X` over the top 1%, 10% and 99% of its values, about five scattered records each; `unique > X` over the top 10% and 99%, values in record ID order |
| `query/100k/eq` | `unique = X`, one match; `quarter = 'q0'`, one term holding 25% of records, with the default page, a page 80% of the way in, and `count`; `skewed = 'big' AND quarter = 'q0'`, two large terms |
| `query/100k/and` | An empty `quarter = 'q9'` before the 99% `unique` range, which evaluation then skips |
| `query/100k/union` | `IN` over 8 and 512 values of about five scattered records each, and over 512 `cluster` values of 100 consecutive records each; the same 512 scattered values as an unordered `OR` of `Eq`s; `(q0 OR q1) AND skewed = 'small-1'` |
| `query/100k/sort` | `quarter = 'q0'` sorted by `number`, its first page and one 80% of the way in; every record by `number`, and by `quarter` then `number`; the latest 10 `q0` records, by `unique` descending |
| `query/100k/scan` | `Prefix` matching 11 terms, about 18% of records; `Exists` of any type over 4 terms |
| `query/100k/get` | `get` by key, a different record each call, and of keys no record has |

Write cases run on a database preloaded with 100,000 records, then 200 commits
of 70 creates, 60 patches and 70 deletes, so they measure a database with a
history rather than a freshly loaded one. Each case's batch is 200 creates, 200
patches or 200 deletes, or 70 creates, 60 patches and 70 deletes, so a regression
shows which operation it affects. The record shape is described in
`benches/write/workload.rs`. To add a case, add a line to `BATCHES` in
`benches/write/main.rs`.

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

Query fixtures are 100,000 records on MDBX with Keccak-256, created 1,000 per
commit with deterministic `quarter`, `skewed`, `number`, `cluster` and `unique`
attributes, described in `benches/query.rs`. They are a fresh bulk load: record
IDs are dense and nothing has been patched or deleted. Seeding is excluded from
timing. Query cases time one call through the public `Database` API on a warm
page cache. A query returns at most 100 records, so after each case the suite
prints how many its filter matches. Nothing else is printed: unlike commit
cases' storage work, a query's work has no counter, so a query regression shows
only in its time.

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
- Write cases time only the `seal` or `commit` call, in wall-clock time, so
  commit times include waiting for the disk sync, which CPU time would leave
  out. Planning and executing the batch,
  and for commit cases sealing it, happen outside timing. The preloaded database
  is built once per run. `seal` cases run on it and discard each batch, so it
  never changes. Each timed commit runs on a fresh copy of the preloaded
  database.
- Write cases use 3 s of warm-up and 10 s of measurement per case, which
  `--warm-up-time` and `--measurement-time` override. Criterion plans
  iterations from wall time, including each commit's untimed reset, so 10 s of
  measurement runs fewer commits than seals.
- Query cases time only the `query`, `count` or `get` call; building and
  cloning the request happen outside timing. They use Criterion's defaults of
  3 s of warm-up, 5 s of measurement and 100 samples, and treat changes under
  3% as noise, since warm reads vary less than disk-bound commits.

The suites collect timings, not allocation counts. Only the write suite measures
storage: for each commit case, the bytes written and syncs per commit and the
database's size, from MDBX's counters.
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
cargo bench -p golemdb-integration-tests --bench index -- 'index/(keccak-256|blake3)/memory/chunks=64/apply_only/one_posting' --warm-up-time 0.5 --measurement-time 1 --sample-size 20
```

Algorithm-labelled groups replace older unlabelled benchmark names, so establish
new baselines. Use `--test` for fixture smoke checks without timing statistics;
full default runs now include both algorithms and take longer.

## Sorted-vector grouping review (2026-09-28)

The writer now sorts a flat vector stably by `(term, container, offset)` and
retains the last operation per posting. The writer consumes consecutive container
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
