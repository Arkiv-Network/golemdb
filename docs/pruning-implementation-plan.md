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
branch, with `prune` moved before `advance_head` in `stage`. Iterations 3–7 are proposals. Sources:
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
| `pruning` (new) | `PruningConfig`; defines `#minRetention`, reads it from `#params` on open and rejects a window below it; the window rule, the `Stale` table, recording, the sorted sweep, pacing and caps; later history and immutable-data pruning. |
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
pub struct PruningConfig { window: u64, batch: u64, max_deletes: usize, chunk: usize }

impl PruningConfig {
    pub const fn new(window: u64) -> Self;
    pub const fn with_batch(self, batch: u64) -> Self;
    pub const fn with_max_deletes(self, max: usize) -> Self;
    pub const fn with_chunk(self, chunk: usize) -> Self;
    pub const fn window(&self) -> u64;
}

pub struct Pruner { config: PruningConfig }

impl Pruner {
    /// Inside the commit transaction: record what `commit` superseded, then run every
    /// in-commit pruning step.
    pub fn prune<'a>(
        &self,
        tx: &mut impl WriteTransaction,
        commit: u64,
        superseded: impl IntoIterator<Item = (Table, &'a [u8])>,
    ) -> Result<(), PruningError>;

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
- `batch` (`K`, default 50): commits per epoch. An epoch's rows are deleted together in key order,
  so rows sharing a page go together.
- `max_deletes` (default 100,000): most rows one commit deletes, bounding the work pruning adds to
  a commit.
- `chunk` (default 1,000): entries per `Stale` row, keeping each row small and letting the sweep
  consume a commit's list piece by piece.

`PruningError` converts into `BranchError::Pruning`. `branch` stores an optional `Pruner`
(`Branches::new` takes an optional `PruningConfig`) and calls it from `stage`, which both commit
paths share; without one, nothing is recorded or deleted. `Roots` stays in `branch` because the head
depends on it; branch drops every `Roots` row before `pruner.cutoff(head)`, so none locates a trie
the sweep may already have deleted.

## 3. Trie GC design

**Recording.** Each commit sorts its superseded rows by table and key and appends them to the
uncommitted `Stale` table as `commitNr ‖ chunk` rows of up to `chunk` entries. Each entry is
`len: u16 ‖ table: u8 ‖ key`, where the table code is fixed per versioned table (`CellTrie` 0,
`IndexTrie` 1, `BitmapTrie` 2, `BitmapContainer` 3). A commit's rows are one contiguous, appended
run.

**Sweep.** An epoch is due once its last commit is at or below `head − W`, so every root that
reaches its rows has left the window. The sweep merges a due epoch's lists into one `(table, key)`
order, one cursor per commit, and deletes a slice per commit. Rows of one table that share a page
are deleted one after another, so each page is copied once. Each commit deletes up to twice as many
rows as it listed (at least 1,024), capped by `max_deletes`.

**Progress.** Consumed chunks are deleted and a partly consumed chunk is rewritten with what is left,
in the commit's transaction. The `Stale` table itself is the sweep's progress: no separate progress
row, nothing lost on a crash.

**Measured** (mean seal + commit per commit over commits 200–300, two rounds, `W` = 100, `K` = 50,
against the same build without GC; 300 commits of 1,000 operations each). *churn*: Arkiv-like
records with a 1 KB payload, 100 commits of creates, then 200 of 35% creates, 30% patches and 35%
deletes. *s300k*: small records (256 B payload), creates only.

| | churn | s300k |
| --- | --- | --- |
| Linux | +5% | +6% |
| macOS | +1% | +7% |

The design comparison's prototype measured +3–6%. Two alternatives were benchmarked first:
per-entry `Stale` rows keyed by epoch (+19–32%, scattered inserts) and per-entry rows appended per
commit (+6–14%). One list per commit performed the same as chunks. An earlier build measured +6% /
+11% (Linux) and +5% / +9% (macOS); the s300k gap was in the sweep or the deletes, not in the list
layout, and narrowed once the sweep seeks only commits with `Stale` rows. If a larger workload
brings it back, a candidate is each commit rewriting the rest of every partly consumed chunk, up to
about `K` × 40 KB.

**Storage (tries only).** Each commit supersedes about `r` of trie rows (8–9 MiB per 1,000 operations
in the workloads above). At head `h`, trie history takes about `r·h` without GC and `r·(W + K)` with
it, plus sweep lag; each due epoch frees about `r·K`. GC saves about `r·(h − W − K)`, so its share of
trie history tends to 100% as the node ages. Live state, `Stale` and other tables are not included.

## 4. Worked example

A database with `W` = 100 and `K` = 50. Commit 120 changes one cell besides `#roots`. The root
(route `[]`) was last written by commit 119, as every commit rewrites `#roots`, and the cell's branch
at route `0x9` by commit 95. Showing only these two of the rows seal reports (the `#roots` path
supersedes a few more):

```text
CellsUpdate.superseded = [(CellTrie, 119 ‖ []), (CellTrie, 95 ‖ 0x9), …]
```

Commit 120 appends one `Stale` row:

```text
Stale[120 ‖ 0] = [ (0, 95 ‖ 0x9), (0, 119 ‖ []), … ]     // sorted by table code, then key
```

The `0x9` row is still reached by roots 95–119 and the root row by root 119, all inside the window.
Commit 120 belongs to epoch 2 (commits 100–149), which is due once 149 ≤ `head − W`, at head 249:
root 149 is still kept, but every root older than 149 has left the window. From then on, each commit
merges the lists of commits 100–149 in key order and deletes a slice: the two rows above go together
with every other `CellTrie` row on their pages. When the chunk `Stale[120 ‖ 0]` is fully consumed it
is deleted. Independently, every commit drops `Roots` rows before `head − 100`.

## 5. Iterations

Each iteration leaves the workspace compiling, with its own focused checks.

### Iteration 1 — Pruning crate

**Outcome:** `golemdb-pruning` records superseded rows and deletes them once due, before anything
calls it.

1. Add `golemdb-pruning` with `PruningConfig`, `Pruner`, `PruningError` and the `Stale` table.
2. Implement recording, the merged sweep, pacing, the per-commit cap and partial-chunk rewrites
   (section 3). Drop a malformed `Stale` row and continue, losing only the rows it listed, so it never
   blocks commits.

**Likely areas:** `crates/pruning`, workspace `Cargo.toml`.

**Status:** done on `ev/pruning-crate`.

**Acceptance:** fed rows over many commits, for several `W`, `K`, chunk sizes and caps, a row is
deleted only once its epoch is due, and every due row is eventually deleted; malformed rows are
dropped and the rest still swept.

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
5. `branch` calls `prune` in the commit transaction before writing the head, on both commit paths,
   and drops `Roots` rows before `cutoff`. `Branches::new`'s pruning settings and
   `OpenConfig::with_pruning` enable it.

**Likely areas:** `crates/merkle/src/trie.rs`, `crates/cells/src/{cells.rs,cell_trie.rs}`,
`crates/index/src/{bitmap_trie.rs,index_trie.rs,index/writer.rs}`,
`crates/branch/src/{commit.rs,manager.rs,roots.rs}`, `crates/api/src/{config.rs,database.rs}`.

**Status:** done on `ev/tries-gc`.

**Acceptance:** a property test over random commits and several `W`, `K` and caps checks after every
commit that every row in the window is stored, every other stored row is listed in `Stale`, nothing
listed is already gone, and `Roots` holds exactly the window. With a steady number of rows per
commit, a due epoch is expected to drain within about `K/2` commits (section 3); this is an
estimate, not a tested property. The test checks the stricter "no due row is left" when epochs are
small and `max_deletes ≥ 1,024`, and also runs on MDBX with the default chunk size and a reopen
halfway, so `Stale` carries the sweep's progress across a restart. Malformed `Stale` rows are
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
5. Add `after_commit` as a stub, and check that history pruning fits inside `prune`, so parts 2 and 3
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
2. Expose the `Stale` backlog (due rows not yet deleted), so a too-low cap is visible.

**Acceptance:** `earliest` advances with the window; a node reports its real earliest commit; the
backlog drains when the cap allows.

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
3. Run in the same `prune` call, paced by the same cap.

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
