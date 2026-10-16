# GC prototypes

Benchmark prototypes behind [docs/gc-options.md](../../docs/gc-options.md), not production code.
Apply one to a clean checkout of its base with `git apply <patch>`; techniques are selected by
environment variables.

| Patch | Base | Contents |
|---|---|---|
| `versioned-keys-gc.patch` | this branch | B, C, D, D + sweep, E on versioned keys (`GOLEMDB_GC` = `stale`, `sweep`, `diffwalk`, `diffsweep`, `marksweep`; `GOLEMDB_GC_W`, `_K`, `_N`); test `gcproto` |
| `pr35-window.patch` | PR #35 (`ed77b05`) | A1 and A2, reference counts with a window (`GOLEMDB_GCA` = `window`, `defer`; `GOLEMDB_GCA_W`, `_K`); test `gcwin` |
| `hash-marksweep.patch` | `57c48ca` | E on hash keys (`GOLEMDB_GC=marksweep`, `GOLEMDB_GC_N`) |
| `mdbx-page-size.patch` | this branch | `GOLEMDB_PAGE_SIZE` for new databases |
| `node-id-stale.patch` | #60 at `6ac18eb` | Same cache, APPEND and durable stale lists for version/path and node-ID layouts; [method and measured results](node-id-stale/README.md) |

Example: `GOLEMDB_GC=sweep GOLEMDB_GC_W=5 GOLEMDB_GC_K=4 cargo test -p golemdb-integration-tests --test gcproto`
