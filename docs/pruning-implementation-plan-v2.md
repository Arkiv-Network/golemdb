# Pruning implementation plan

Pruning discards what only commits older than a database's retention window reach. It has three parts:

1. **Trie GC:** delete trie rows no root in the window reaches. **Done** ([#84](https://github.com/Arkiv-Network/golemdb/pull/84), [#85](https://github.com/Arkiv-Network/golemdb/pull/85), [#118](https://github.com/Arkiv-Network/golemdb/pull/118)).
2. **History pruning:** delete `*ChangeSet` rows before the window and trim `*History` sets.
3. **Immutable-data pruning:** delete range rows and key bindings before the window, and shard
   files once wholly before it.

**Pruning is not purging.** Removing records, as Arkiv's purging of expired entities does, is an
ordinary `delete`: it changes state and the root. Pruning frees what deletes and updates leave
behind, never changes state, and is not part of the commitment: the same commits give the same
roots whatever the window.

The previous plan, with the trie GC design and measurements, is
[pruning-implementation-plan.md](pruning-implementation-plan.md).

## Status

| # | Iteration | Status | Depends on |
| --- | --- | --- | --- |
| 1–2 | Pruning crate, trie GC | merged ([#84](https://github.com/Arkiv-Network/golemdb/pull/84), [#85](https://github.com/Arkiv-Network/golemdb/pull/85), [#118](https://github.com/Arkiv-Network/golemdb/pull/118)) | — |
| 3 | History pruning | in review ([#189](https://github.com/Arkiv-Network/golemdb/pull/189), stacked on [#172](https://github.com/Arkiv-Network/golemdb/pull/172); [#128](https://github.com/Arkiv-Network/golemdb/issues/128), [#170](https://github.com/Arkiv-Network/golemdb/issues/170)) | history stack ([#133](https://github.com/Arkiv-Network/golemdb/pull/133)–[#172](https://github.com/Arkiv-Network/golemdb/pull/172)) |
| 4 | Immutable-data pruning | not started ([#146](https://github.com/Arkiv-Network/golemdb/issues/146)) | its prerequisites |
| 5 | Retention configuration | `with_pruning` merged; `#minRetention` in genesis proposed in [#189](https://github.com/Arkiv-Network/golemdb/pull/189), [#154](https://github.com/Arkiv-Network/golemdb/issues/154) open | — |
| 6 | Offline `compact` tool | open, unreviewed ([#116](https://github.com/Arkiv-Network/golemdb/pull/116), [#144](https://github.com/Arkiv-Network/golemdb/issues/144)) | 3–5 |
| 7 | Gap detection (optional) | not started | 6 |
| 8 | Performance improvements (optional) | not started | benches ([#155](https://github.com/Arkiv-Network/golemdb/issues/155)) |

Related: [#131](https://github.com/Arkiv-Network/golemdb/issues/131) (pruning safety
validation), [#155](https://github.com/Arkiv-Network/golemdb/issues/155) (pruning benches).

## Principles

- **One crate.** `golemdb-pruning` owns the configuration, the floors and every sweep; `branch`
  calls it. The offline tool is a separate crate (`compact`), as it needs `branch`.
- **Inside the commit.** Sweeps run in the commit's transaction, after the sealed rows are replayed,
  under one per-commit cap. Only immutable-data file deletion runs after the commit.
- **Floors.** `Superblock.history_floor` is the highest commit whose history was pruned, written in
  the same transaction as the deletions; historical reads before it are refused, so it never claims
  more than what remains. Each immutable segment has its own floor.
- **The window is local and runtime** (`OpenConfig::with_pruning`). Growing it cannot restore
  what is gone; the floor keeps that honest.
- **No committed state.** `Stale`, the floors and the gap marker are uncommitted, like `Roots`.
- **Only the trie needs listing.** Change sets and immutable range rows are keyed by commit, so
  history and immutable data catch up online after pruning was off; trie rows superseded meanwhile
  are found only by the offline tool.

## 3. History pruning (in review)

[#189](https://github.com/Arkiv-Network/golemdb/pull/189) implements it. In each commit, after the trie sweep, change-set rows at or below
`head − W` go oldest first, in batches like trie GC's, per table: their commits leave each item's
set (each set rewritten once, emptied sets deleted), then the rows are deleted. The floor becomes
the highest commit removed, in the same transaction: deleting change set `c` only affects reads
before `c`, which the floor refuses. Reads accept `max(head − W, floor)`, so a grown window or
disabled pruning serves exactly what remains.

Sets are trimmed: every commit creates a new `#roots ‖ (c − 1)` cell, so untrimmed history grows
by a row per commit forever. Trimming makes historical-read costs depend on retention, which
[#159](https://github.com/Arkiv-Network/golemdb/pull/159)'s metering spec already accepts.

## 4. Immutable-data pruning

Immutable rows are not old versions: a row appended at commit `c` stays readable at every later
head, and only `rows_of(commit)` is addressed by commit. Pruning one removes data, not a past view,
so each segment has its own floor, independent of the history floor. For now every segment uses the
pruning window `W`; per-segment windows (design §11 expects headers kept longer than bodies) can
come later without changing the design.

**Prerequisites:**

- **Newest key wins.** `bind_key` returns `AlreadyExists` for a bound key; design §11 says the newest
  row wins, "so no commit outcome depends on how much history a node retains". With pruned
  bindings, a reused key would succeed on a pruned database and fail on an unpruned one. Binding
  again overwrites the forward row, and `key_for_ordinal` treats a superseded reverse row as `None`,
  not `Corrupt`.
- **Ordinals from metadata.** With no file left, `append` and `end_ordinal` restart at 0. Take the
  next ordinal from `end_ordinal_at`, falling back to the segment floor; `recover` hands each
  segment's end to the file backend. The memory backend never deletes rows, so it is unaffected.
- **Recovery from the floor.** `recovery_ranges` requires ranges from ordinal 0, and `recover` a
  file per range starting at that range. Start from the floor; for a partly swept oldest shard,
  use the file header's `first_ordinal`.

**Design:**

- **Segment floor:** one row per segment (commit, ordinal), written with the deletions. It answers
  F03 (where a shard's first ordinal survives once older metadata is gone).
- **Metadata sweep** (reth's model): in the commit, after the other sweeps, under
  [#189](https://github.com/Arkiv-Network/golemdb/pull/189)'s batch cap. Take the segment's range
  rows at or below `head − W`, oldest first; for each, walk its reverse bindings
  `1 ‖ segment ‖ [start, end)` (contiguous), delete them, and delete a forward row only if it still
  points at that ordinal. The floor becomes the highest commit swept. It runs last, as `publish`
  checks `end_ordinal_at` after `stage`.
- **Files:** a shard `[s, s + #shardSpan)` goes once `s + #shardSpan ≤` the floor's commit; first
  at open through `recover`, then after each commit under the cleanup lock (`.jar` / `.off` before
  `.conf`). A per-segment "pruned below" ordinal on `FileSet`, set under the write lock before
  deleting and checked by `read_range` under the read lock, gives a reader on an older snapshot
  `Pruned`, not `Corrupt`.
- **Reads below the floor:** checked before routing to a shard. `rows_of` and `get` by ordinal
  return `Pruned` (today an empty list and `Corrupt`); a key not yet swept returns `NotFound`.

Key lookups stay one seek whatever the retention, and reads stop exactly at the window; the cost is
random forward-row deletes. The alternative, keys grouped by shard (`segment ‖ shard ‖ key`), drops
the reverse rows and deletes a shard's keys as one range, but changes the storage format and makes
a key lookup probe every retained shard, worst for the segments kept longest.

**Span.** `#shardSpan` only sets how much data stays on disk past the window: up to one span of
commits. Recommend a span well below typical windows (around `W/4`). This reverses the assumption in
[#72](https://github.com/Arkiv-Network/golemdb/pull/72) of a span of at least `W`, made to bound
the read walk.

**Order:** newest-wins; floors and refused reads, deleting nothing; the metadata
sweep; file deletion at open, then online.

## 5. Retention configuration

The window is set with `OpenConfig::with_pruning` (merged). [#189](https://github.com/Arkiv-Network/golemdb/pull/189) adds `#minRetention` to
genesis (option A, 0 allowed): opening refuses a smaller window, and the design (§1 property 5,
`#params`, D05) drops the guarantee that every node refuses at `head − #minRetention`. The host
sets the minimum for its own needs, and a database may serve further back. Open in [#154](https://github.com/Arkiv-Network/golemdb/issues/154):
whether GolemDB holds it at all.

| | A: in genesis, default 0 ([#189](https://github.com/Arkiv-Network/golemdb/pull/189)) | B: none; the host enforces it | C: host minimum in `OpenConfig` |
| --- | --- | --- | --- |
| For | shared by every database of a deployment | no network policy in GolemDB | checked in GolemDB, outside genesis |
| Against | enters genesis identity ([#106](https://github.com/Arkiv-Network/golemdb/pull/106) objection); a network concept in GolemDB | every host re-checks | each host must pass the same value |

## 6. Offline `compact` tool

Shrinking the MDBX file needs the database closed, and only the tool finds trie rows leaked while pruning
was off: a commit without pruning lists nothing in `Stale`, so its superseded trie rows are never
deleted.

[#116](https://github.com/Arkiv-Network/golemdb/pull/116) marks rows reachable from the retained roots, deletes the rest, and copies the store
compacted. To land it:

- Rebase on `main`; drop the bitmap code the flat index removed.
- Open through the API's genesis validation, not its own `hash_fn` parsing; it then checks the
  window against `#minRetention`, as iteration 5 decides.
- Run the history and immutable-data metadata sweeps uncapped before compacting; the history sweep
  advances the floor.

## 7. Gap detection (optional)

Tells an operator when the tool is needed. `Superblock.listed_since` is 0 at genesis; a commit
without pruning deletes it, a commit with pruning and no marker writes its own commit. Opening with
pruning and a marker above 0, or none (any store from before the marker), warns and points to the
tool, which resets it to 0 after a full run.

## 8. Performance improvements (optional)

Mostly trie GC, where improvements were explored most; the history and immutable-data sweeps can
be measured the same way once they exist.

The fastest trie GC design measured was dropped in review ([#84](https://github.com/Arkiv-Network/golemdb/pull/84)) for a simpler one: one `Stale` row
per superseded row, and a batch of the oldest due rows once `MIN_BATCH_ROWS` are due. The reasons:
simplicity first, tuning parameters that are hard to choose, issues found in review, and a target
scale (1,000–2,000 records per commit) where a few percent matter little. The cost of the simpler
design: higher mean overhead, and spiky commits, since GC runs on few commits and makes those
several times slower ([#118](https://github.com/Arkiv-Network/golemdb/pull/118) halved the spikes with smaller batches).

Revisit only if benches ([#155](https://github.com/Arkiv-Network/golemdb/issues/155)) at the target load show GC overhead or spikes matter. Candidates,
from the [previous plan's appendices](pruning-implementation-plan.md#appendix-potential-optimizations):

- **The original design:** sorted `Stale` chunks per commit, epochs of 50 commits drained by a k-way
  merge, deleting twice the rows registered per commit. +3% / +5% mean overhead against +24–36% for
  the batches as first merged, and no commit much above twice the others; about 220 lines against 70.
- **Cached batches:** sort a larger batch once and delete a slice per commit (+12% mean), at the
  cost of an in-memory cache and a check that the previous commit landed.
- **MDBX:** a larger `rp_augment_limit` keeps the free list flat (330 MiB to 105 MiB over 2,200
  commits); cached table handles cut every `put` and `delete`. Both help every write, not only GC.

## Open questions

- A minimum window above 0, for readers that reopen an old commit across transactions?
- A separate, shorter proof window (D05 option B)? It would need a second floor.
