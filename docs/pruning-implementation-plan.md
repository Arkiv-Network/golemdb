# Pruning implementation plan

Pruning discards everything a node no longer has to keep: data that only commits older than its
retention window can reach. It removes history, never live state: GolemDB records have no lifetime.
This is not Arkiv's pruning, which deletes expired entities. Until Arkiv has that, every entity stays
forever; pruning drops only old values, old trie nodes and old immutable data. It has three parts:

1. **Trie garbage collection (GC):** delete stored trie rows that no root in the window reaches. They
   serve only historical proofs. With only live state kept, these are today's sole source of write
   amplification. Versioned trie keys make them cheap to track, without reference counts.
2. **History pruning:** delete `CellChangeSet` / `IndexChangeSet` rows and trim `CellHistory` /
   `IndexHistory` entries for commits before the window. Needed once historical values are kept.
3. **Immutable-data pruning:** delete immutable-data files (`crates/immutable-data`: one file per
   segment per `#shardSpan` commits) whose whole commit span is before the window, such as old block
   headers, raw transactions and receipts. Deferred, likely closer to Mumbai.

A new `golemdb-pruning` crate owns all three behind one interface that the branch crate calls when it
commits.

**Scope:** iterations **1–3** deliver trie GC, its configuration in genesis and the pruning interface
for parts 2 and 3. Iteration **4** adds retention reporting and operations as a follow-up. Iterations
**5–7** cover enabling GC on an existing database, history pruning and immutable-data pruning, each
after its prerequisite exists.

**Starting point:** iterations 1 and 2 are implemented, stacked on the versioned trie keys
(`ev/tries-versioned-keys`): the crate on `ev/pruning-crate`, then superseded-row reporting and the
branch and API wiring on `ev/tries-gc`. Both still need a rebase onto the current versioned-keys
branch. Iterations 3–7 are proposals. Sources:
[design §9 Garbage Collection](golemdb-design.md#garbage-collection),
[D05](decisions_for_architect.md) (retention), and the [GC
comparison](https://github.com/golemfactory/golem-db/blob/c8e296fed94cf32685372ee3ecaff06caad4e530/docs/gc-options.md)
(`docs/gc-options.md` on `ev/sorted-keys-exp`).

## 1. Decisions and boundaries

- **One crate owns pruning.** `golemdb-pruning` holds the configuration and reads it (the window from
  the open configuration, `#minRetention` from genesis), the window rule, GC's bookkeeping and sweep,
  and later the history and immutable-data parts. The branch crate calls it; storage, trie and record
  crates do not know about pruning.
- **Tries report what a write replaced, as a general feature.** Every trie update returns the keys of
  the stored rows it superseded. Pruning is the first consumer; metering, observability and replication
  can use the same report. This is not pruning logic and does not live in the pruning crate.
- **Pruning runs inside the commit's transaction.** One fsync, and its progress is atomic with the
  commit: a crash never leaves GC half done. Because a pruning error fails the commit, per-commit work
  is capped and pruning can be switched off. Immutable-data file deletion is the exception: it is
  not transactional and runs after the commit. Running the sweep separately, when the GolemDB user
  chooses, is not planned but could be considered later.
- **Two retention numbers.** Genesis holds the consensus minimum, `#minRetention` in `#params`
  (D05): the consensus-path API refuses reads before `head − #minRetention` identically on every
  node, and every node keeps at least that much. Each node's actual window comes from its open
  configuration and must be at least the minimum (D05 5.2); archival nodes keep more or disable
  pruning, and serve the surplus through a separate archival surface. By default pruning is on, with
  `window = #minRetention`.
- **The window rule.** A row superseded at commit `s` is reached only by roots before `s`, so it is
  deletable once `s ≤ head − W`. `Roots` rows before `head − W` are dropped, so the first `Roots` row
  is the node's earliest retained commit.
- **No new committed state for GC.** Its bookkeeping (`Stale`) is uncommitted, like `Roots`. Two nodes
  with different windows agree on every root. `Stale` is node-local and not part of snapshot sync; a
  synced node starts listing from its sync point.
- **Out of scope:** historical cell and index reads and their `Pruned` / `NotFound` API contract (they
  need the history tables), separate windows for proof and query history (D05 option B), and rewind.
  Rewind (D02) must truncate `Stale` to the target, since its rows are appended in commit order,
  cannot go back past `W`, since older rows are deleted, and must not write at a commit that already
  has rows: the trie deletes rows of the write's own commit that it replaces.

## 2. Ownership and interfaces

| Layer | Responsibility |
| --- | --- |
| `api` | Passes the pruning settings from `OpenConfig` and the genesis `#minRetention` value through; exposes `retention()`. |
| `branch` | Calls the pruner in the commit transaction with the rows its seal superseded; owns `Roots` and drops rows before `cutoff`. |
| `pruning` (new) | `PruningConfig`; defines `#minRetention`, reads it from `#params` on open and rejects a window below it; the window rule, the `Stale` table, recording, the sweep and its cap; later history and immutable-data pruning. |
| `merkle` | `TrieUpdate { root, superseded }` from every write: the stored rows it replaced, as `(table, key)`: branch rows, and for `PayloadTrie` the old rows of leaves it replaced or removed. |
| `cells` / `index` | Pass superseded rows up in `CellsUpdate` / `IndexUpdate`. |
| `storage` | Unchanged: `append`, `put`, `delete` and cursors. |

### Superseded rows (general trie feature)

```rust
pub struct TrieUpdate<const N: usize> {
    pub root: RootRef<N>,
    /// Stored rows this write replaced, which only older roots still reach: branch rows and,
    /// for a `PayloadTrie`, leaf rows, each with its table.
    pub superseded: Vec<(Table, Vec<u8>)>,
}
```

A row the same commit wrote is never listed: no published root reaches it, so if the commit replaces
it, it is deleted. `Cells::apply` and `Index::apply` collect their tries' lists, container rows
included, into `superseded: Vec<(Table, Vec<u8>)>`. Seal already loads every row it replaces, so the
report costs a key copy per row and no extra reads.

### Pruning interface

Implemented on `ev/pruning-crate`, except `open` and `after_commit` (iteration 3).

```rust
pub struct PruningConfig { window: u64 }

impl PruningConfig {
    pub const fn new(window: u64) -> Self;
    pub const fn window(&self) -> u64;
}

pub struct Pruner { config: PruningConfig }

impl Pruner {
    /// Inside the commit transaction: register what `commit` superseded.
    pub fn register<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
    ) -> Result<(), PruningError>;

    /// Inside the commit transaction: delete a batch of the oldest due rows, once enough
    /// are due or the oldest has waited long enough.
    pub fn sweep(&self, tx: &mut impl WriteTransaction, head: u64) -> Result<(), PruningError>;

    /// Commits before this may be pruned: `head − window`. Not the earliest retained commit,
    /// which `retention()` reports from `Roots`.
    pub fn cutoff(&self, head: u64) -> u64;

    /// On open (iteration 3): read `#minRetention` and reject a window below it; later also the
    /// immutable-data configuration (`#shardSpan`, segments) that iteration 7 needs.
    pub fn open(tx: &impl ReadTransaction, config: PruningConfig) -> Result<Self, PruningError>;

    /// After the commit transaction (iteration 7): delete expired immutable-data files.
    pub fn after_commit(&self, head: u64) -> Result<(), PruningError>;
}
```

The settings:

- `window` (`W`): commits whose roots stay readable; rows only older roots reach are deleted.
- `MIN_BATCH_ROWS` (75,000, a constant): due rows a sweep waits for, so a batch is large enough
  for rows sharing a page to go together.
- `MAX_BATCH_ROWS` (150,000, a constant): most rows one sweep deletes, bounding the commit that
  carries a batch.
- `MAX_WAIT_COMMITS` (50, a constant): commits the oldest due row waits at most; past it a smaller
  batch goes, so a quiet node still drains.

`PruningError` converts into `BranchError::Pruning`. `branch` stores an optional `Pruner`
(`Branches::new` takes an optional `PruningConfig`) and calls `register` then `sweep` from `stage`, which both commit
paths share; without one, nothing is registered or deleted. `Roots` stays in `branch` because the head
depends on it; branch drops every `Roots` row before `pruner.cutoff(head)`, so none locates a trie
the sweep may already have deleted.

## 3. Trie GC design

**Recording.** Each commit writes one row per superseded row to the uncommitted `Stale` table:
key `commitNr ‖ table: u8 ‖ key`, empty value. The table code is fixed per versioned table
(`CellTrie` 0, `IndexTrie` 1, `BitmapTrie` 2, `BitmapContainer` 3). A commit's rows are one
contiguous run, in `(table, key)` order.

**Sweep.** A row superseded at `s` may go once `s ≤ head − W`: every root that reaches it has left
the window. Due rows are deleted in batches. Each commit reads the oldest due rows, at most
`MAX_BATCH_ROWS`; once at least `MIN_BATCH_ROWS` are due, or the oldest has waited
`MAX_WAIT_COMMITS` past the cutoff, it sorts them by table and key and deletes them with their
`Stale` rows; otherwise it deletes nothing. A trie row's key starts with the commit that wrote it,
so the rows one commit supersedes sit on scattered pages; sorting a batch puts rows that share a
page together, so most pages are rewritten once. At about 13,000 superseded rows per churn commit,
a batch goes every 6 commits or so, and that commit takes longer than the others. A malformed
`Stale` row is dropped, so it never blocks commits.

**Progress.** The `Stale` table itself is the sweep's progress: what is still listed is what is
left, with no separate progress row, no state in memory, and nothing lost on a crash.

**Measured** (mean seal + commit per commit over commits 200–300, two rounds, `W` = 100, against
the same build without GC; 300 commits of 1,000 operations each). *churn*: Arkiv-like records with
a 1 KB payload, 100 commits of creates, then 200 of 35% creates, 30% patches and 35% deletes.
*s300k*: small records (256 B payload), creates only. The designs tried, first macOS then Linux,
each the average of churn and s300k:

| Design | `Stale` layout | Epoch (pacing) | Mean overhead | Worst commit | Main complexity |
| --- | --- | --- | --- | --- | --- |
| Delete the one commit that left the window | one row per entry | none | +84% / +35% | no spikes, ~0.5 s | none |
| Same | one row per commit | none | +76% / +28% | no spikes, ~0.45–0.5 s | list encoding; multi-page values |
| Delete the whole due epoch, sorted, in one commit | one row per commit | 50, unpaced | +2% / +4% | 1.6 / 1.8 s once per epoch | list encoding; epoch sort |
| Same | one row per commit | 10, unpaced | +19% / +12% | ~1.0–1.1 s once per epoch | same |
| Delete from the front of `Stale`, kept sorted by its key | one row per entry, keyed by epoch | 50, paced | +29% / +20% | no spikes, ~0.5 s | epoch in the key |
| Re-read and sort the due epoch every commit, delete a slice | one row per entry | 50, paced | +24% / +26% | no spikes, ~0.5–0.7 s | per-commit load and sort |
| Sort the due epoch once, cache it, delete a slice | one row per entry | 50, paced | +12% / +12% | ~0.3–0.68 s; ~0.95 s once per epoch | the cache and its consecutive-head check; pacing tied to the recorded count |
| Batch the oldest due rows: 50k–100k | one row per entry | none | +48% / +28% | ~0.5–0.72 s every ~4 commits | min/max thresholds; wait limit |
| Same, 75k–150k (chosen) | one row per entry | none | +36% / +24% | ~0.56–1.17 / ~0.73–0.9 s every ~6 commits | same |
| Same, 125k–250k | one row per entry | none | +28% / +22% | ~0.6–1.07 / ~0.86–1.07 s every ~10 commits | same |
| Earlier build: k-way merge, partial-chunk rewrites, 2× quota | chunks per commit | 50, paced | +3% / +5% | ~0.5 s, half of each cycle up to 2× | merge, chunk rewrites, quota |

**Batch size.** Decision: keep 75k–150k, the smallest batch that still sorts enough rows to pay off,
with the slowest commit under 1 s on Linux. A grid over `min`–`max` (Linux, then macOS; churn and
s300k):

| `min`–`max` | Mean overhead | Worst commit |
| --- | --- | --- |
| 25k–50k | +32% · +30% / +69% · +71% | 0.62 / 0.98 s |
| 50k–100k | +30% · +26% / +54% · +47% | 0.73 / 0.87 s |
| 75k–150k (chosen) | +27% · +22% / +42% · +24% | 0.92 / 0.9 s (one 1.29 s outlier) |
| 100k–200k | +26% · +21% / +37% · +26% | 0.97 / 0.99 s |
| 150k–300k | +24% · +20% / +33% · +16% | 1.16 / 1.51 s |
| 100k–100k | +25% · +22% / +43% · +20% | 0.93 / 1.09 s |

Below 75k the cost climbs sharply: too few rows are sorted together to share pages. Above it,
Linux gains at most 3 points while the slowest commit grows towards half a 2 s block, so 75k–150k
stays. Steady-state batches hold about `min` plus one commit's rows, so `max` matters only for a
backlog, after the window shrinks or pruning is turned on; `min = max` gains nothing.

Most of the cost is page copies: deleting one commit's scattered rows touches thousands of pages,
and the larger the set sorted together, the more of those pages are shared. Batches keep no state
and bound memory, at about twice the cost of sorting a whole epoch once and deleting it in cached
slices (see the appendix). Arkiv commits are about 1,000–2,000 records, at the low end of which these
runs sit.

**Storage (tries only).** Each commit supersedes about `r` of trie rows (8–9 MiB per 1,000
operations in the workloads above). At head `h`, trie history takes about `r·h` without GC and
about `r·W` with it, plus up to a batch waiting; each commit frees about `r`. GC saves about
`r·(h − W)`, so its share of trie history tends to 100% as the node ages. Live state, `Stale` and other tables are not included.
Measured over 2,200 churn commits (`W` = 100): the trie tables stop growing after the window, and
the file ends at 4.1 GB against 25.8 GB without GC. MDBX's free list still grows with its default
settings (see the appendix).

## 4. Worked example

A database with `W` = 100. Commit 120 changes one cell besides `#roots`. The root
(route `[]`) was last written by commit 119, as every commit rewrites `#roots`, and the cell's branch
at route `0x9` by commit 95. Showing only these two of the rows seal reports (the `#roots` path
supersedes a few more):

```text
CellsUpdate.superseded = [(CellTrie, 119 ‖ []), (CellTrie, 95 ‖ 0x9), …]
```

Commit 120 writes one `Stale` row per superseded row:

```text
Stale[120 ‖ 0 ‖ 95 ‖ 0x9] = ∅
Stale[120 ‖ 0 ‖ 119 ‖ []] = ∅      // table code 0: CellTrie
```

The `0x9` row is still reached by roots 95–119 and the root row by root 119, all inside the window.
Both are due once 120 ≤ `head − W`, at head 220: every root before 120 has left the window. They go
with the next batch, once enough rows are due, sorted by table and key with every other due row, so
they go with the other `CellTrie` rows of their pages, along with their `Stale` rows. Independently, every commit drops `Roots` rows before
`head − 100`.

## 5. Iterations

Each iteration leaves the workspace compiling, with its own focused checks.

### Iteration 1 — Pruning crate

**Outcome:** `golemdb-pruning` registers superseded rows and deletes them once due, before anything
calls it.

1. Add `golemdb-pruning` with `PruningConfig`, `Pruner`, `PruningError` and the `Stale` table.
2. Implement registering and the batched sweep (section 3). Drop a malformed `Stale` row
   and continue, so it never blocks commits.

**Likely areas:** `crates/pruning`, workspace `Cargo.toml`.

**Status:** done on `ev/pruning-crate`.

**Acceptance:** fed rows over many commits, for several `W` and batch settings, no row is deleted
before it is due and every due row is eventually deleted; a batch waits for `MIN_BATCH_ROWS`, takes
the oldest rows up to `MAX_BATCH_ROWS`, and goes smaller once the oldest has waited
`MAX_WAIT_COMMITS`; malformed rows are dropped and the rest still swept.

### Iteration 2 — Superseded-row reporting and trie GC

**Outcome:** every trie write reports the stored rows it replaced, and a node configured with a window
deletes trie rows no root in it reaches.

1. `Trie::apply`, `insert` and `remove` return `TrieUpdate`. A private write context carries the
   commit and the list through the write helpers.
2. Record a superseded row wherever a stored branch is rebuilt, split or absorbed into its parent,
   except rows written by the same commit: those the write replaces and doesn't store again are
   deleted, so a trie (namespace: every bitmap trie shares one table) may be written more than once
   per commit.
3. `PayloadTrie::apply` lists the old row of each leaf it replaces or removes, never one it keeps
   unchanged; `BitmapTrie` no longer locates container rows itself.
4. `CellsUpdate` / `IndexUpdate` carry `superseded: Vec<(Table, Vec<u8>)>`.
5. `branch` calls `register` and `sweep` in the commit transaction before writing the head, on both commit paths,
   and drops `Roots` rows before `cutoff`. `Branches::new`'s pruning settings and
   `OpenConfig::with_pruning` enable it.

**Likely areas:** `crates/merkle/src/trie.rs`, `crates/cells/src/{cells.rs,cell_trie.rs}`,
`crates/index/src/{bitmap_trie.rs,index_trie.rs,index/writer.rs}`,
`crates/branch/src/{commit.rs,manager.rs,roots.rs}`, `crates/api/src/{config.rs,database.rs}`.

**Status:** done on `ev/tries-gc`.

**Acceptance:** a property test over random commits and several `W` checks after every commit that
every row in the window is stored, every other stored row is listed in `Stale`, nothing listed is
already gone or has waited past `MAX_WAIT_COMMITS`, and `Roots` holds exactly the window. It also runs on MDBX with a reopen halfway, so `Stale` carries the sweep's progress across a restart. Malformed `Stale` rows are
dropped and the commit succeeds. Several writes to a trie in one commit leave no unreachable row.
Roots match a run without GC, and GC also runs on the `publish` path.

### Iteration 3 — Configuration: genesis and open

**Outcome:** retention is configured where D05 places it.

1. Define `#minRetention` (`u64`, commits) in `golemdb-pruning`; genesis writes it to `#params`.
2. Add pruning settings to `OpenConfig` (`with_pruning`, done). `Pruner::open` reads `#minRetention`
   and rejects a window below it; `api` only passes the settings through.
3. Default: pruning on with `window = #minRetention`. An `OpenConfig` without pruning settings uses
   it; archival nodes switch pruning off explicitly (documented).
4. Expose the settings through the API crate and the conformance harness.
5. Add `after_commit` as a stub, and check that history pruning fits inside `sweep`, so parts 2 and 3
   plug in without changing `branch`.
6. A window changed between opens: shrinking prunes more; growing cannot restore deleted rows, so the
   earliest retained commit comes from `Roots`, not from `head − window`. The same applies when
   pruning is switched off, and to a snapshot-synced node (D05 5.7), whose `Roots` start at its sync
   point.
7. Opening without pruning stops listing superseded rows, so turning it back on would leak them; so
   does a second handle without pruning committing to the same store. `Superblock` records the commit
   since which rows are listed; reopening with pruning after a gap is refused, or runs iteration 5's
   pass.

**Acceptance:** a database reopened with the same settings prunes identically; one opened without
pruning settings prunes with `window = #minRetention`; a window below the minimum is refused; a
grown window does not claim commits already pruned; genesis identity covers `#minRetention`.

### Iteration 4 — Retention reporting and operations

**Outcome:** callers and operators can see what a node retains.

1. Add `retention() → { earliest, head }`; `earliest` is the first `Roots` key.
2. Expose the `Stale` backlog (due commits not yet swept), visible after a window shrink.

**Acceptance:** `earliest` advances with the window; a node reports its real earliest commit; the
backlog drains after a window shrink.

### Iteration 5 — Enabling GC on an existing database

**Outcome:** turning pruning on later, or back on after a gap (iteration 3, step 7), does not leak
rows superseded while it was off.

1. One-time pass: mark every row reachable from the roots in the window and list the rest in
   `Stale`, or document that pruning must be configured from genesis.

**Acceptance:** after the pass and one window, stored rows equal the rows the window's roots reach.

### Iteration 6 — History pruning (after bitemporality)

**Outcome:** history and change-set rows for commits before the window are deleted.

1. Delete `CellChangeSet` / `IndexChangeSet` rows with `commitNr < cutoff` (a key-range delete).
2. Remove those commits from `CellHistory` / `IndexHistory` entries; delete an entry when it empties.
3. Run in the same `sweep` call, paced by the same cap.

**Prerequisite:** the history tables are written, with `IndexChangeSet` storing bitmap-root locators.

### Iteration 7 — Immutable-data pruning

**Outcome:** immutable-data files whose whole commit span is before the window are deleted.

One rule covers files and metadata: a segment's file covering commits `[s, s + #shardSpan)`, and
those commits' `ImmutableDataCommitRanges` rows, are deleted once `s + #shardSpan ≤ cutoff`. The
exception is the segment's latest range and its file, which stay: `end_ordinal_at` needs that range
for the next ordinal (F03 in [decisions](decisions_for_architect.md)).

1. In the commit transaction, delete the range rows of expired spans.
2. After the commit, under the immutable writer lock `publish` still holds (so it can't race
   `storage.recover`), `after_commit` deletes the expired files, found from the file spans, so a rerun
   repairs a crash. Errors don't fail the
   published commit; they're logged and retried next commit.
3. Recovery starts from the first retained range rather than ordinal 0, and treats missing files
   below `cutoff` as pruned, not as a gap.
4. Reads of a pruned row return a new `Pruned` error: by ordinal when it is below the first retained
   range; by key only if bindings are kept (see open questions), since a deleted binding cannot be
   told from a key that never existed.

**Prerequisite:** `crates/immutable-data` (merged). Krzysiek adds this part over the crate's
interface.

## 6. Dependency order and completion criteria

```text
1 pruning crate
  -> 2 superseded-row reporting + trie GC
    -> 3 genesis / open configuration
      -> 4 retention reporting and operations
    -> 5 GC on an existing database
  bitemporality  -> 6 history pruning
  immutable data -> 7 immutable-data pruning
```

Iteration 1 lands first and has no caller until 2. Iteration 5 is independent of 3 and 4.

**Trie GC complete:** iterations 1–3. A configured node keeps exactly the window's trie rows and
`Roots`, deletes the rest at a measured cost, refuses a window below `#minRetention`, and exposes the
interface parts 2 and 3 plug into. Iteration 4 adds reporting of its earliest commit and backlog.

**Pruning complete:** additionally iterations 4–7, once their prerequisites exist.

## 7. Open questions

- **Reads across transactions:** a reader keeps its snapshot within one read transaction, so nothing
  breaks today. Historical reads or paging that reopen an old commit later could find its rows
  deleted, and `window = 0` (accepted now) deletes rows as soon as the next commit supersedes them.
  Is a minimum window above 0 enough (D05 rules out a cursor lease)?
- **Existing databases:** a one-time pass (iteration 5), or pruning only from genesis?
- **Immutable-data ranges:** delete `ImmutableDataCommitRanges` rows before the window (iteration 7),
  or keep them all, which avoids the recovery change at about 28 bytes per commit per segment?
- **Immutable-data keys:** are `ImmutableDataKeys` bindings of pruned rows kept (they make a reused
  key fail with `AlreadyExists`, and let reads by key return `Pruned`) or deleted with their files?
- **Separate proof window:** proof history (`CellTrie`, `IndexTrie`) is over 90% of retained bytes
  (D05's estimate); a shorter proof window would save most of it, but it is a requirements change
  (D05 option B). The table codes in `Stale` already separate the groups.

## Appendix: potential optimizations

Found while measuring, left out for simplicity. Revisit only if a workload shows the need.

- **Fastest measured design** (the earlier build, +3% / +5% mean, no commit much above twice the
  others): each commit stores its sorted list in `Stale` chunks of at most 32 entries, so each fits
  one MDBX page. Rows go by epochs of 50 commits, due once the last is at or before `head − W`. A
  k-way merge, one cursor per commit, reads the due epoch in table and key order without loading
  it; each commit deletes twice the rows it registered (at least 1,024), and rewrites any partly
  consumed chunk with what is left, which is the progress. It took about 220 lines against about
  70, for the merge, chunk encoding, partial rewrites and the quota. Combined with the two MDBX
  items below it would likely be the fastest overall (not measured together).
- **MDBX free-page reuse:** with MDBX's default `rp_augment_limit` its free list kept growing over
  2,200 commits (to 330 MiB, file 3.68 GB); with 4M pages it stayed flat at about 105 MiB (file
  3.46 GB), at the same commit time. `coalesce` + `liforeclaim` made it worse (493 MiB). Set in
  `golemdb-storage-mdbx`, not pruning; the strongest candidate.
- **MDBX table handles:** the wrapper opens a table handle on every `put` and checks the table
  catalogue on every `delete`. Caching handles would cut the cost of every write, the sweep's
  deletes included; in `golemdb-storage-mdbx`, not pruning.
- **Chunked `Stale` rows:** packing a commit's list into rows of up to `chunk` entries means fewer
  puts per commit and a few percent less `Stale` space. A chunk must fit one MDBX page (32 entries
  of under 50 B): 1,000-entry chunks spanned about 11 pages and fragmented the free list (730 MiB
  vs 330 MiB at 32 entries).
- **Cached batches:** sort a larger batch once (about 650,000 rows, a 50-commit epoch), keep it in
  memory, and delete a fixed slice of it per commit. Measured as cached epoch slices: +12% mean on
  both machines instead of +24–36%, and the frequent heavier commits become one per batch (~0.95 s).
  Costs the cache (bounded by the batch), a `Mutex` on the `Pruner`, and a check that the previous
  commit landed, so a rolled-back commit cannot leave the cache ahead of `Stale`. The `Stale`
  layout is unchanged, so it needs no migration.
- **`append` for `Stale` rows:** a commit's keys sort after every earlier one, so `register` could
  append them, falling back to `put` when a stray row sorts last. Measured with the batches: no
  gain (within 1–3%), so `put` is kept.
- **`window = 0`:** each commit writes its `Stale` rows and deletes them in the same transaction;
  deleting superseded rows directly would skip that.
- **Sweeping outside the commit:** see section 1; not planned.
