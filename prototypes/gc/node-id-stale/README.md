# Node IDs with stale-list GC

This experiment compares the version/path layout from #60 with monotonic `u64`
node IDs. Both layouts use the same cache, batch updates, APPEND publication and
durable stale-list collector. See [RESULTS.md](RESULTS.md) for measurements.

**The measured ID implementation does not improve throughput.** It reduces
logical key bytes but uses more MDBX table pages. These results do not support
replacing the version/path layout with this implementation.

## Apply and check

The patch applies to #60 at `6ac18eb2e9845e8d199267c90c78cd02daf6761e`.
It is a separate alternative to `versioned-keys-gc.patch`; do not apply both.
The default engine code remains unchanged until the patch is applied.

From a clean checkout of this PR, run:

```sh
bash prototypes/gc/node-id-stale/check.sh
```

The script applies the patch, checks the workspace and runs the GC tests for
both layouts at W=0/8/64. The tests authenticate retained branches and bitmap
containers after 200 commits. They also check allocation rollback, competing
seals, bounded GC across a process restart, and readers held across GC commits.
The ignored `durable_worker` test runs only through its parent restart test.

## Design

- `GOLEMDB_LAYOUT=version-path` selects #60's addressing. `node-id` selects IDs.
  Selection is fixed for the process. Both variants keep the same Merkle hashes.
- Each new physical branch or bitmap container gets a global ID. Unchanged
  subtrees retain their locators. Equal content at different positions is not
  deduplicated. The existing `commit` locator fields carry IDs in ID mode.
- The counter lives in the seal buffer. Publication writes its final value in
  the same transaction as nodes, roots, stale lists and head. Discarded and
  losing seals do not advance it. Published IDs are not reused.
- Each replaced node enters `Stale[replacement_commit, chunk]`. Chunks contain
  at most 128 sorted entries. GC consumes chunks with replacement commit at or
  below `head - W`. W counts old retained versions in addition to head.
- `GOLEMDB_GC_CHUNKS` can bound work per publication. The remaining committed
  chunks are the continuation state. No queue position is held only in memory.
  The benchmark leaves this limit unset and collects all eligible chunks.
- Node-ID databases use experimental format ID `0x4e494401`. Opening a database
  with the other layout fails. No migration is provided. Keep layout and
  retention settings fixed for the database.

Only node and root retention is tested. Retained trie nodes do not provide
historical flat record values or historical per-term bitmap-root metadata.

## Reproduce the benchmark

The included harness is the exact source used for this comparison, with its
resolved lockfile. It runs one scenario per process. The workload creates
20,000 active records, runs 65 warmup commits and measures 60 mixed commits.

After applying the patch, use the recorded Docker image if it is available.
The included Dockerfile provides the same Rust minor version and native build
dependencies; rebuilding it does not reproduce the recorded image digest.

```sh
docker build -t golemdb-bench:1.97 -f prototypes/gc/node-id-stale/Dockerfile .
BENCH_CPUS=28-31 bash prototypes/gc/node-id-stale/benchmark.sh
bun prototypes/gc/node-id-stale/summarize.mjs target/node-id-stale-<timestamp>
```

The script creates fresh databases and results under a new
`target/node-id-stale-<timestamp>` directory. Set `BENCH_CPUS` to CPUs
available on the host. `BENCH_IMAGE`, `BENCH_MEMORY`, `BENCH_CARGO_HOME` and
`BENCH_RESULTS` can override the defaults. `BENCH_RESULTS` is relative to the
checkout. The measurement host used CPUs 28–31
and a 24 GiB container limit.

The result CSV contains each run's metrics. `raw-results.sha256` identifies the
raw files held at `/home/ubuntu/node-id-stale-bench` on `gf-prov-1`. The source
manifest uses the measurement directory's `engine/` and `harness/` paths.
