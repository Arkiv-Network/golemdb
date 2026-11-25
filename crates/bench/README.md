# Load benchmarks

Scenarios that load Golem DB through its public `Api` the way a host does: blocks of creates, updates and
deletes on a database that grows from empty to hundreds of GiB, with queries run in between. They show what
the Criterion suites (`docs/benchmarks.md`) cannot: how seal, commit and query times and the file size
develop as the database outgrows RAM.

```sh
just bench smoke                        # seconds; results in target/bench-out/smoke
just bench arkiv /data/out /data/db     # where the results and the database go
```

`<scenario>` is a name under `crates/bench/scenarios` or a path to a `.toml` file; relative paths are taken
from the repository root. The database directory is deleted and recreated first. Needs `just` and clang (MDBX's bindings are generated at build time).

## Results

Every run writes the same files to `<out>`:

| File | Contents |
|---|---|
| `commits.csv` | One row per commit, flushed as it goes |
| `tables.csv` | Every MDBX table's size, every `table_log_every` commits and after the last |
| `queries.csv` | One row per query each time the queries run |
| `summary.json` | The headline numbers: `{ "schema": 1, "scenario": ..., "started_unix_ms": ..., "metrics": [{ "name", "value", "unit" }] }` |

**`commits.csv`**: `commit`, `elapsed_ms` (since the workload started), `ops`, `bytes` (the cell values
written plus 32 bytes of key per operation), `seal_ms`, `commit_ms`, `branch_ms` (the whole begin-to-commit),
`db_mib` (database files after the commit), `bytes_written` and `syncs` (what MDBX wrote for this commit, from
its page counters: the same in every run of the same code), and
`state_root` and `index_root` (hex, as seal returned them; equal between two runs only if they stored the
same state).

**`tables.csv`**: entries, B-tree depth, pages and MiB of each table (`Cell`, `CellTrie`, `Index`, ...), from
MDBX's own statistics, plus the file space outside the tables: `free` (pages released by earlier commits,
waiting for reuse), `other` (MDBX metadata), `unused` (grown ahead of need) and `file` (the total). A separate
read-only process reads it between commits, so it is not timed.

**`queries.csv`**: `commit`, `records`, `query`, `matches` and `returned` (medians over the
runs; pages always ask for the total, which costs nothing as the whole match set is built anyway), `runs`,
`first_ms` (the first run, which may read pages the page cache does not hold yet), and `p50_ms`, `p99_ms`, `max_ms` of
all runs.

**`summary.json`**: the p50 and p99 of seal, commit and whole-block time, the bytes written per commit and
the file size at the end of each group of blocks and wherever the queries run (a point covers the 1,000
commits ending there, fewer in a short run), the final size of the file and of each table, and each
query's times. `started_unix_ms` is when the workload started, the zero of `elapsed_ms` in `commits.csv`.

## Suites

`crates/bench/suites.toml` names groups of scenarios for a benchmark runner, with the memory cap and time limit
each runs under: `standard` is the default, `large` takes hours. A runner needs only that file,
`just bench_build`, `just bench <scenario> <out> <db_dir>`, and the result files above.

While it runs, a scenario prints a line of progress every 30 seconds.

## Scenario format

```toml
name = "example"
seed = 1
# table_log_every = 100         # tables.csv every N commits

[database]
hash = "keccak-256"             # or "blake3"
limits = { max_cell_name_len = 32, max_str_len = 64, max_bytes_len = 1024 }
# max_map_size_gib = 64
# pruning_window = 128          # garbage-collect trie rows older than this many commits

[shape]                         # every record; an update rewrites every cell except `unique` ones
attributes = [                  # indexed
    { name = "status", type = "str", cardinality = 4, len = 8 },
    { name = "owner", type = "u64", cardinality = 10000, distribution = "zipf" },
    { name = "id", type = "str", distribution = "unique", len = 36 },
]
fields = [{ name = "payload", type = "bytes", len = 256 }]                 # not indexed

[workload]                      # one writer, blocks of these sizes in order
blocks = [
    { ops = 1000, repeat = 20 },
    { ops = 100, repeat = 500, mix = { create = 35, patch = 30, delete = 35 } },   # its own mix
]
mix = { create = 1 }                                                               # the default mix
# query_at = [20, 520]          # run `queries` after these commits, between blocks
# query_runs = 50               # per query; a query stops after 10 s
# queries = [
#     { name = "owner + status", filter = { and = [{ eq = { owner = 0 } }, { eq = { status = 1 } }] } },
#     { name = "top owners", filter = { range = { owner = { lt = 10 } } }, count = true },
# ]
```

**Cell options:**

- `type`: `u64`, `str` or `bytes`. `bytes` is for fields only.
- `cardinality`: the number of distinct values. `1` means every record has the same value.
- `distribution`: `uniform` (the default), `zipf` (a few hot values), or `unique` (never repeats).
- `len`: the size of `str` and `bytes` values. Either a number, or weighted ranges drawn uniformly, e.g.
  `[{ min = 1024, max = 4096, weight = 90 }, { min = 4096, max = 102400, weight = 10 }]`.

**Queries** name values by rank, not literally, so a query means the same at any database size: for a
`uniform` or `zipf` attribute, rank `n` is its n-th value (zipf's 0 is the most common); for a `unique` one,
the n-th value generated (from 1). `"random"` instead of a rank draws a new value for every run, from the
attribute's own distribution (`{ eq = { "$owner" = "random" } }`: hot wallets often, the long tail rarely),
so a query isn't just its previous run read back from the page cache. `u64` values are the rank itself, so
`range` bounds on a uniform `u64` are plain numbers. Attributes used in queries need a fixed `len`. `prefix`
takes a literal string (`str` values are the rank in hex, zero-padded to `len`). Leaves: `eq`, `in`, `range`
(`gt`/`gte`/`lt`/`lte`), `prefix`, `exists`; `and` and `or` combine them. Query options: `limit`, `offset`,
`only = [names]`, and `count = true` to count instead of returning a page.
