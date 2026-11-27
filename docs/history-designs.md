# History designs explored

A running log of the history layouts prototyped against the history PR stack
(#133 → #172), what each costs, and why it was kept or dropped. Scratch
prototypes only; nothing is committed.

## Summary (2026-11-27)

All values are on churn, against the stack in the same run, as warm / cold
(cold = 768 MiB cap). Writes: median seal+commit change. Reads: geometric mean
of time ratios over the bench queries (head = live reads, historical = reads at
past commits; below 1 is faster), with the worst single query in brackets.
Cold read times before the a1–a3 runs (rows A, C–J) were taken with readahead on
and are noisy; later rows also have page counts in their sections.

| | Design | Writes | Head reads | Historical reads | Change | Status |
|---|---|---|---|---|---|---|
| A | Pre-flat index | +40 / +26 %¹ | 12.0 / 3.1 (16) | 3.4 / 1.8 (7.2) | — | dropped |
| B | Stack | baseline (history +72 / +290 ms) | 1 / 1 | 1 / 1 | — | baseline |
| C | IndexDelta | −1 / 0 % | 0.99 / 1.02 (1.2) | 2.5 / 1.1 (26) | index | dropped |
| D | Versions beside live rows | −5 / −9 % | 3.2 / 1.7 (5.9) | 1.25 / 1.06 (3.6) | `Index` | dropped |
| E | Embedded list, tombstones | −11 / −29 % | 1.46 / 0.70 (1.9) | 0.50 / 0.54 (0.94) | `Cell`+`Index` | blocked |
| F | Embedded list, side table | −15 / −22 % | 1.07 / 1.00 (1.6) | 0.57 / 0.74 (1.4) | `Cell`+`Index` | became a2 |
| G | Per-record cells (rec) | −11 / −23 % | 0.99 / 1.29 (1.5) | 0.95 / 1.10 (1.6)² | cells history | superseded by H |
| H | Per-record user cells (sys) | −7 / −16 % | 1.00 / 1.24 (1.6) | 0.96 / 1.07 (1.4) | cells history | part of a1 |
| I | Records as one row (blob) | −16 / −16 % | 1.09 / 1.29 (1.8) | 0.94 / 1.15 (2.1) | storage | parked |
| J | Creation flag | −5 / −6 % | 1.02 / 1.01 (1.1) | 0.95 / 0.92 (1.1) | history row | keep, composes |
| a1 | Lean (H + J) | −8 / −15 % | 0.96 / 0.99 (1.0) | 0.92 / 0.89 (1.0) | history tables only | safe step |
| a2 | Embedded, capped | −14 / −21 % | 1.01 / 0.92 (1.2) | 0.52 / 0.65 (2.0) | `Cell`+`Index` rows | candidate |
| a3 | Hybrid | −12 / −22 % | 0.99 / 0.92 (1.4) | 0.59 / 0.69 (2.0) | `Index` rows | best balance before sl |
| sl | Stored leaves | −27 / −42 % | 0.92 / 1.01 (1.04) | 2.3 / 1.36 (7.1) | trie leaf rows, ordered index trie | best writes |
| K | Keyed pre-images | −8 / −2 % | 0.99 / 1.00 (1.02) | 1.28 / 0.91 (3.0) | history tables only, roots unchanged | dropped |
| sl+ | sl + stamps, back-pointers | −27 / −42 % | 0.96 / 0.91 (1.27) | 1.19 / 1.23 (4.8) | sl + live-row stamp | **best overall** |
| P | sl + packed trie | +16 / −15 % | 0.97 / 0.98 (1.02) | 2.1 / 1.06 (6.9) | trie storage | rejected |
| sl+P | sl+ + packed trie | +17 / −17 % | 0.98 / 0.90 (1.26) | 1.15 / 0.99 (5.1) | both | rejected |
| ls | Log-structured state | −50 / −78 %³ | 7.6 / 2.5 (15) | 1.8 / 0.95 (5.4) | all storage, index root | rejected |

¹ Pre-flat base lacks #117 (cells check-and-update in one walk); at equal cells
code the flat-index doc measured flat only 2–10 % faster on writes.
² Excludes `#recordKeys`, which this read run lacked; a later run measured it
up to 82× slower warm on G.
³ Seal+commit only; ls moves work into operations, so its whole block is
0 / −25 %.

Direction (2026-11-27): aim for a simple base design; caches and batched or
merged structures are optimizations to consider on top, not part of the base.
The ls numbers include a top-4-level node cache, so without it its reads would
be slower still (smoke: operations 155 ms per commit without, 131 ms with).

Open items: the `Index` leaf-fill gap in a2/a3; the busy-record walk in a1/a3
(not benched); everything ran 300 commits without pruning; metering
(`read.commit_set`) needs adapting for a2/a3.

## Ground rules

- **Cost metric:** pages a commit or query touches, not bytes.
- **Read priorities:** head reads matter most, and head range queries above
  all. Historical queries matter about equally among themselves (ranges, points,
  cells), and less than head reads.
- **Must not break:** commit finality/atomicity (all history written in the
  commit's own transaction, no deferred batches), proofs (historical proofs need
  the exact canonical bytes at T and the versioned trie), deterministic metering,
  `#minRetention`.
- **Bench:** glade (Linux, NVMe, 4 KiB MDBX pages), conformance harness,
  seal+commit per commit, median over commits ≥ 200, mean of 2 rounds (round 2
  in reverse order). Warm = unrestricted page cache; cold =
  `systemd-run --user --scope -p MemoryMax=768M -p MemorySwapMax=0`.
  - churn: 6 attributes + 1 KiB payload; 100 × 1000 creates, then
    200 × 1000 ops at 35 % create / 30 % patch (one cell) / 35 % delete.
  - s300k: 300 × 1000 creates.
  - Reads (rbench, churn DB): ranges over a unique `created` attribute, busy and
    quiet container points, every cell of 200 records, 200 `#recordKeys`
    bindings; at head and 0/10/100/250 commits back; cold = first run after
    evicting the page cache, warm = median of 5.
- Every variant is checked for identical state and index roots and identical
  query results.

## Where the cost comes from

The stack's layout, per tracked item (a cell, or an index container
`term ‖ hi48`):

```
Live      Cell[key] / Index[key]           → value
History   *History[key]                    → Roaring CommitSet of modifying commits
ChangeSet *ChangeSet[commit ‖ key]         → value before that commit (empty = absent)
```

- `ChangeSet` is keyed by commit first, so a commit's rows are appended to
  adjacent pages: cheap.
- `History` is keyed by item, so every changed item costs a read-modify-write
  on a random page. This is almost all of history's write cost, and it grows
  when the data does not fit in memory.
- Measured on the stack: warm +39 % per commit (churn +70 ms, s300k +50 ms),
  cold +84–104 %, disk +44 % / +28 %. Index history alone is about +25 ms warm
  and +170–190 ms cold on churn; cell history about +47 ms warm and +100 ms cold.
- Reads at a past commit cost 1.7–7× the head read warm, even at back 0, because
  a range read merges the whole `IndexHistory` range.

## Designs

Numbers are "history cost" = variant minus the no-history build in the same run,
churn warm / cold, unless stated.

### A. Pre-flat index (bitmap trie per term)

- History per term (locator + CommitSet).
- History cheap (about +22 / +77 ms), because the versioned bitmap tries already
  keep old containers (pre-flat without index history used ~190 MiB more disk
  than flat without it).
- Writes: with the same cells code, flat is 2–10 % faster (flat-index doc). The
  30–40 % warm gap in run `hcx` compared a pre-flat base without #117 (cells
  check-and-update in one walk), so most of it is the cells change.
- Reads: head lookups 1.0–1.6× and ranges 3–32× slower on pre-flat
  (flat-index doc); historical reads 3–7× slower warm (`hcx`).
- **Dropped:** the flat index (#108) wins overall.

### B. The stack (baseline)

- Separate `History` + `ChangeSet` per item, for cells and index.
- +70 / +290 ms churn, +50 / +107 ms s300k. Disk 2096 MiB churn vs 1456 no history.

### C. IndexDelta

- `IndexDelta[key ‖ commit]` holds the flipped record IDs, or a full image as a
  checkpoint; replaces `IndexHistory` + `IndexChangeSet`.
- Writes tie the stack: still one random container-keyed page per change.
- Warm reads 2–3× slower on ranges, up to 25× on busy points.
- **Dropped:** saving bytes did not save pages.

### D. Versions beside live rows

- `Index[key ‖ commit]` holds the pre-image, next to the live row, so the history
  write lands on the page the commit already dirties.
- Index history cost about halved (+25 → +13 warm, +166 → +110 cold).
- Head range scans 5–6× slower warm, 1.5–3× cold: leaves fill with version rows
  (a creation version for every live item, two rows per deleted item).
- **Dropped:** moves cost from writes to head reads, which run far more often.

### E. Commit list embedded in the live row, tombstones

- `Cell`/`Index` row = `value ‖ commits ‖ u16 len`; only `value` is hashed.
  `*History` tables gone. A deleted item stays as a tombstone (empty value + list).
- History cost ≈ 0 churn (+3 warm / ≈ 0 cold), +9 / +57 s300k.
- Historical reads 1.5–3× faster warm.
- Head range scans 1.6–2× slower warm (tombstones).
- **Blocked:**
  - lists grow without bound in hot rows (`#alloc/#nextRecordID` changes every
    commit that creates; u16 overflow after about 65k such commits halts seal on
    every node);
  - tombstones make record scans and their metering depend on each node's
    retention window.

### F. Embedded list, deleted lists in a side table

- As E, but a deleted item's list moves to `CellDeleted`/`IndexDeleted`, so live
  tables never hold tombstones.
- History cost +36 / +147 churn, +6 / +48 s300k: each deletion costs one random
  page in the side table.
- Head scans back to normal (0.96–1.25×), historical reads 1.5–2.5× faster.
- **Blocked** by the same unbounded list growth as E.

### G. Per-record cell history (rec)

- `CellHistory[record_id]` = creation flag + commits that changed any cell of the
  record; `CellChangeSet` rows only for changed cells (a missing row = this cell
  unchanged at that commit). Index untouched.
- History cost +41 / +181 churn, +18 / +56 s300k (stack +68 / +332, +52 / +124).
- Reads equal or faster, except system records: `#recordKeys` holds one binding
  per user record and changes on almost every commit, so a binding read at T walks
  every commit since T: 4.8× / 37× / 82× slower warm at 10 / 100 / 250 back.
- **Superseded by H.**

### H. Per-record history for user records only (sys)

- As G for user records (id ≥ 64); system and admin records keep per-cell
  history.
- Fixes `#recordKeys` (1.0× at every depth).
- Loses part of G's gain on create-heavy loads: each create adds a binding cell
  with its own history row (warm s300k −8 % vs flat, G −17 %).
- Open risk: a busy user record read on a rarely changed cell walks every change
  to the record since T (not exercised by the bench).

### I. User records as one row (blob)

- `Record[record_id]` = all cells of the record; `RecordHistory` + whole-row
  `RecordChangeSet`; system records keep `Cell` with per-cell history.
- Best churn writes (−16 % warm vs flat), historical reads O(1).
- Costs: whole-row rewrite and pre-image for one-cell patches of large records
  (untested), a byte cap per record, metering by row bytes, forward-only cell
  cursor. (Its cold index reads looked 1.2–2× slower; that later proved to be
  readahead and page-cache noise, see the a1/a2/a3 results.)
- Taking it further (no `Cell` at all) needs `#recordKeys`, `#roots` and
  `#rootIndex` redesigned, and a record-leaf trie is a fork-level change.
- **Parked:** out of scope for alternatives that keep `Cell`.

### J. Creation flag (cf)

- History row starts with a flag byte: "the first listed commit created the item";
  creation writes no empty pre-image.
- On the stack: −5 % warm churn, −7 % warm s300k, −6 / −9 % cold; disk −80 /
  −96 MiB; `CellChangeSet` s300k 72.8 → 0 MiB; reads before creation skip a lookup
  (0.65–0.7× at 250 back).
- **Keep:** small, independent of layout. Pruning must clear the flag when it
  drops the first listed commit.

### Considered, not built

- **History built later in batches from the ChangeSet** (Erigon-style): ruled out,
  history must be complete in the commit's transaction.
- **ChangeSet only (commit-first), no History:** writes ideal, but a point read at
  T costs one seek per commit since T.
- **Back-pointers** (live row holds its last commit, each pre-image points to the
  previous change): fixed-size rows, but reads walk every change since T.
- **History through the versioned trie** (per-leaf commit stamps in branch nodes,
  post-image log): good for point reads and aligned with proofs, but index ranges
  cannot enumerate through a hashed trie path.
- **Sharded History (Erigon):** still one random page per change.
- **Tombstones that expire at `#minRetention`:** deterministic, but expiry is a
  random write later, and head scans still pay within the window.
- **Embedded list with ChangeSet replay instead of a side table:** the row keeps
  its recent changes; older and deleted items are found by seeking
  `ChangeSet[c ‖ key]` for each commit after T. Writes are ideal, but a creation
  cannot tell whether the key had an earlier, deleted life without an item-keyed
  lookup, so any read before a live item's first listed change must replay every
  commit back to T (up to `#minRetention` seeks per item). Dropped for the side
  table.

## Brainstorm: breaking designs (discussed; 1 prototyped)

1. **Log-structured state:** an ordered Merkle trie plus an append-only value
   log; no `Cell`/`Index`/History/ChangeSet tables. A trie leaf points to its
   value's log position; reading at T descends the trie at root(T), which
   historical proofs need anyway. Removes the live-table random writes too, not
   only history's. Changes the state root, proof format and read path.
   - **Review (2026-11-27):** writes should drop to about the no-history cost or
     below, history and its pruning become free, and proofs are unchanged if the
     hashed routing stays. The costs land on head reads:
     - every point read becomes a trie descent (about 5–6 versioned node lookups
       instead of 1 B-tree lookup);
     - scans lose locality, because values sit in write order;
     - index ranges need an ordered index trie, which forks the root and lets
       clients choose trie depth.

     Not recommended as a base while head reads are the priority. Two cheap
     experiments would size it: head reads through `Trie::get`, and a
     no-live-table write run.
   - **Prototype (`ls`, 2026-11-27):**
     - Trie leaves carry a `u64` value id outside the hash. Values live in
       `CellLog` / `IndexLog`, keyed `commit << 24 | seq`, each row
       `key_len ‖ key ‖ value`; a commit's values are appended in key order.
       `CellRoots` / `IndexRoots` hold the root after each commit.
     - Cells keep the hashed routing. The index trie is ordered: the term is
       zero-padded to 99 bytes, then `hi48` (105-byte paths).
     - No `Cell`, `Index`, History or ChangeSet tables. A read at commit T
       walks root(T); a read at head uses the current root.
     - Writes read every old leaf in one batched, ordered traversal. Readers
       skip branch hash checks and keep the top 4 trie levels decoded in
       memory; the read bench clears that cache whenever it evicts the page
       cache.
     - Checked by a model test (every cell and container at every commit, head
       scans, ranges, postings) and by identical bench query results. The
       state root differs from the stack's because the `#roots` cell stores the
       previous index root.
     - Smoke (30 commits, warm): seal+commit about half the stack's (80 vs
       147 ms), but operations before seal 131 vs 39 ms, because every read
       inside create/patch/delete descends the trie.
   - **Results (glade run `hls`, 2026-11-27):** every query result identical
     to the stack's.

     | per commit, median ms | stack | a3 | ls |
     |---|---|---|---|
     | warm churn: seal+commit / whole block | 250 / 290 | 217 / 256 | **125** / 290 |
     | warm s300k | 185 / 211 | 158 / 185 | **77 / 144** |
     | cold churn | 618 / 743 | 485 / 594 | **136 / 561** |
     | cold s300k | 288 / 330 | 229 / 267 | **74 / 141** |
     | operations before seal, warm / cold churn | 40 / 125 | 40 / 109 | 165 / 425 |
     | pages read per commit, cold churn / s300k | 5868 / 1504 | 4066 / 945 | 3110 / 71 |
     | disk churn / s300k MiB | 2096 / 1456 | 1840 / 1264 | 1552 / 1024 |

     Head reads (pages visited cold, readahead off; warm time), ratio to stack:

     | at head | pages | warm |
     |---|---|---|
     | range 1k / 10k / 50k | 6.5× / 2.5–3.4× / 3.0× | 6.3–6.8× |
     | quiet points | 4.1× | 13× |
     | busy points | 1.04× | 3.4× |
     | whole-record cell reads | 3.4× | 8.9× |
     | `#recordKeys` lookups | 3.8× | 15× |

     Historical reads: 1.2–2.6× the pages at 0–10 back, fewer at 250 back;
     warm ranges about equal, points 2–5× slower.

     **Verdict:** commits get 2–4× cheaper and disk is the smallest of any
     design, but head reads, ranges included, cost 2.5–6.5× the pages and
     6–15× the CPU. Inside writes that same read cost eats the commit gain on
     churn (whole block equal to the stack warm, −25 % cold vs a3's −20 %).
     Rejected as a base while head reads are the priority.
2. **Index posting flips under a2/a3:** the `Index` row's change list names the
   commits that changed it, so `IndexFlips[commit ‖ key]` (the record IDs added
   or removed) is read exactly for those commits. Replaces up-to-8 KiB container
   pre-images with a few bytes per changed record. Reads at T un-flip one entry
   per change since T, so a busy container read deep needs a periodic full
   image (for example at each spill).
3. **Retention ring:** ChangeSet slots keyed `commit mod R`, overwritten in
   place (R = `#minRetention`); no pruning or freelist churn, and pages keep
   their place. Archival beyond R becomes a separate store.
4. **Trie-stamped history:** branch nodes carry each leaf's last-change commit
   (outside the hash, rewritten whenever the leaf changes); values at T come
   from a post-image log. No History tables. Ranges need an ordered index trie,
   as in 1.

## Stored leaves (`sl`): history from the versioned trie

```
Cell[key] / Index[key]              → head value            (as with no history)
CellTrie / IndexTrie
  [commit ‖ route_len ‖ route]       → branch                (as today)
  [commit ‖ 0xFF ‖ path]             → key ‖ value of a leaf the commit changed
```

- Four tables, the same as a database with no history: no History or
  ChangeSet tables and no history crate.
- Trie leaves become stored rows, written by the commit that changes them and
  keyed by commit first, so they land beside the branch rows that commit writes
  anyway. The parent records each leaf's commit outside the hash, as it already
  does for branch children.
- Head reads and every read inside create/patch/delete use `Cell` / `Index`,
  unchanged.
- A read at commit T walks root(T) (from `Roots`) to the leaf row; a historical
  index range walks the ordered index trie. "Absent before creation" is just
  "no leaf at root(T)", so no creation flag is needed.
- History pruning is trie GC: superseded leaf rows go through `Stale` with the
  branch rows.
- Costs: the index trie is ordered (term zero-padded to 99 bytes, then `hi48`),
  which changes the index root and lets client terms shape its depth;
  historical point reads descend the trie (6–7 lookups); each value is stored
  twice within the window (live row and leaf row).
- Prototype: built from the stack with the ls trie changes and no reader cache.
  Model test passes (every cell and container at every commit, ranges, scans).
- Smoke (30 commits, warm): seal+commit 100 vs 145 ms on the stack, operations
  unchanged (40 vs 39 ms), whole block 139 vs 184 ms; every query result
  identical.
- **Results (glade run `hsl`, 2026-11-27):** every query result identical.

  | per commit, median ms | stack | no history | a1 | a3 | **sl** |
  |---|---|---|---|---|---|
  | warm churn: seal+commit / block | 249 / 289 | 182 / 222 | 230 / 270 | 218 / 259 | **182 / 221** |
  | warm s300k | 187 / 214 | 135 / 162 | 168 / 194 | 160 / 186 | **121 / 148** |
  | cold churn | 613 / 734 | 327 / 409 | 521 / 634 | 480 / 588 | **339 / 422** |
  | cold s300k | 285 / 327 | 166 / 200 | 259 / 296 | 236 / 271 | **137 / 168** |
  | pages read per commit, cold churn / s300k | 5977 / 1480 | 2530 / 556 | 5049 / 1158 | 4078 / 983 | 2778 / 338 |
  | disk churn / s300k MiB | 2096 / 1456 | 1456 / 1136 | 1872 / 1280 | 1840 / 1264 | 1936 / 1360 |

  - Writes cost what no history costs (s300k even less: the ordered index
    trie is smaller, 373 vs 453 MiB).
  - Head reads: identical pages on every query (1.00×), warm 0.85–0.98×.
  - Historical reads (pages cold / warm time vs the stack): ranges 1.2–3.3× /
    1.0–1.27× at 0–100 back, fewer pages at 250 back; quiet points 2× / 7×;
    busy points 1.0× / 2–2.5×; cell reads 1.2–1.9× / 3.5–4.5×; `#recordKeys`
    1.2–1.7× / 6–6.6×. Every point read descends the trie.
  - Reads at the head commit itself (back 0) walk the trie; answering them from
    `Cell` / `Index` would make them cost the same as head reads.
  - Disk: leaf rows add about 400 MiB to `CellTrie` and 90 MiB to `IndexTrie`
    on churn, about what the ChangeSets cost the stack.

## Brainstorm round 2: designs from other systems (discussed, not built)

The question underneath every design so far: history writes keyed by item cost
one random page per change; keyed by commit they are sequential but need an
item index to be found. The candidate item indexes are a separate History table
(stack, a1), the live row (a2, back-pointers), the versioned trie (sl), or a
key-ordered table that holds the values itself (K below).

### K. Keyed pre-images (temporal-table history)

Inspired by SQL Server temporal tables (history clustered on `ValidTo`), the
SeiDB state store and the `key ‖ version` MVCC schemas of RocksDB/Pebble users.

```
Cell[key]  / Index[key]              → head value            (unchanged)
CellPast[key ‖ until] / IndexPast[key ‖ until] → value valid before `until`, or ABSENT (created at `until`)
CellTrie / IndexTrie                 → as today (hashed index trie, roots unchanged)
```

- Read at T: one seek to `key ‖ T+1`; a row of the same key answers, else the
  live row does. Ranges at T merge the live scan with one seek per key.
- Replaces History + ChangeSet with one table: no CommitSet read-modify-write,
  no Roaring, roots identical to the stack, no history crate.
- Costs: still one random page per change, now in a table that carries values
  (ChangeSet-sized, so fewer cache hits cold than the small History rows);
  pruning by `until` needs a sweep or prune-on-write, since rows are key-first.
- Expectation: warm a little better than a1, cold close to the stack.

### sl+. Stored leaves with stamps and bounded back-pointers

Inspired by QMDB (each entry points to its previous version) and newest-to-oldest
version chains (Hekaton, InnoDB undo pointers), bounded by the sl trie.

- Live `Cell`/`Index` rows carry the commit of their last change (unhashed
  trailer); each sl leaf row carries the commit of the leaf's previous change.
- Read at T: live stamp ≤ T → the live value (one lookup, as at head); else
  follow leaf rows `(stamp, path)` → `(prev, path)` for at most two hops; else
  descend root(T) as sl does. Deleted items descend.
- Fixes sl's weak spot (quiet points 7×, busy points 2–2.5× warm) without new
  tables; the "walk every change" problem of plain back-pointers is capped by
  the trie fallback.
- Costs: a live-row format change (Index only, or both); 8 bytes per leaf row.

### P. Page-packed trie nodes

Inspired by NOMT (a depth-6 binary subtree per 4 KiB page) and MonadDb/Firewood
(the trie is the storage layout).

- Store two hex levels per trie row instead of one: a descent is 3–4 lookups
  instead of 6–7, for proofs, writes and sl's historical reads alike.
- Costs: each change rewrites a larger node row; a trie storage redesign, not a
  history design. Orthogonal to every option above.

### Rejected on sight

- **Versions in the live table** (`Cell[key ‖ !commit]`, CockroachDB/Pebble,
  Postgres append-only): head range scans step over every retained version.
- **Time-split / multiversion B-trees** (TSB-tree, MVBT): need control of page
  splits, which MDBX does not give.
- **Immutable history files with an inverted index** (Erigon 3): a buffered,
  merged structure, so an optimization on top, not a base.
- **Delta-encoded trie pages** (LETUS DMM-tree): shrinks versioned-trie bytes,
  but needs a custom page store.

### Results (glade run `hr2`, 2026-11-27)

Every query result identical to the stack; roots: K = stack, sl+/P/sl+P = sl.
Warm / cold, churn unless stated; reads as in the summary table.

| | seal+commit churn | seal+commit s300k | pages read/commit, cold churn | disk churn MiB |
|---|---|---|---|---|
| stack | 248 / 604 | 183 / 275 | 5835 | 2096 |
| no history | 181 / 340 | 131 / 144 | 2699 | 1456 |
| sl | 182 / 353 | 118 / 137 | 2779 | 1936 |
| K | 229 / 590 | 166 / 267 | 5866 | 1952 |
| sl+ | 182 / 348 | 124 / 141 | 2766 | 1952 |
| P | 288 / 516 | 164 / 205 | 3741 | 4288 |
| sl+P | 289 / 503 | 165 / 192 | 3549 | 4304 |

- **K:** history writes stay one random page per change (cold pages read equal
  the stack's), so it saves only the CommitSet work: −8 % warm, −2 % cold.
  Historical points 0.6–0.9×, but historical ranges 1.2–2.7× warm (one seek per
  key). Dominated by a1.
- **sl+:** writes equal sl. Reads at back 0–10 drop from sl's 4.5–7× warm to
  0.4–0.75× the stack for quiet points, record cells and `#recordKeys`; deeper
  (100–250) they are 1.2–4.8× warm, still below sl. Busy points get slightly
  worse than sl (3–4.5× vs 3×), since the hops fail and the walk follows.
  Ranges as sl. Head index pages move 0.53–1.27× (the stamp changes `Index` leaf
  fill, as in a2), cold head time 0.91× on average.
- **P:** packing two levels makes rows large (up to 17 nodes, many in overflow
  pages): trie tables 2.4× (4.3 GB total), warm writes +16 % vs the stack, and
  historical reads barely change (2.1 vs 2.3 warm). The descent was not the cost.
  Rejected, with or without sl+.

## Alternatives under test (no Cell → Record)

All three write everything in the commit's own transaction, hash only the
canonical value (roots identical to the stack), keep live tables free of
tombstones, and bound every stored list.

### 1. Lean (`a1`): separate tables, fewer and smaller rows

```
Cell[r ‖ name]                    → value                      (unchanged)
CellHistory[r]          (r ≥ 64)  → flag ‖ CommitSet            per user record
CellHistory[r ‖ name]   (r < 64)  → flag ‖ CommitSet            per system cell
CellChangeSet[c ‖ r ‖ name]       → pre-image, only for cells c changed, none on creation
Index[key]                        → container                  (unchanged)
IndexHistory[key]                 → flag ‖ CommitSet
IndexChangeSet[c ‖ key]           → pre-image, none on creation
```

- H + J: one history row per user record instead of one per cell, and no empty
  pre-image for anything a commit creates.
- No live-table or reader changes; head reads and record scans untouched.
- Costs: the busy-record walk of H; pruning must clear the flag.

### 2. Embedded, capped (`a2`): history on the page the commit already writes

```
Cell[key] / Index[key]    → value ‖ recent changes ‖ u16 len   (only value hashed)
CellOlder / IndexOlder    → older changes and deleted lives' changes, per item
*ChangeSet[c ‖ key]       → pre-image, none on creation
```

- Each change list entry is `gap << 1 | created`. A row keeps at most 16
  changes; when it would hold 17, the oldest 8 move to the `*Older` side table
  (one random write per 8 changes of a busy item). Deleting an item moves its
  list there too (one random write per deleted item).
- No `*History` tables. A read at T takes the first listed change after T from
  the live row; only when T precedes the row's first listed change does it read
  `*Older`. A change with the creation bit answers "absent" without a lookup.
- Fixes E/F's blocker: rows are bounded (at most 16 entries), so no u16
  overflow and no unbounded growth on hot rows such as `#alloc`.
- Costs: a row format change in `Cell` and `Index`, so every reader of those
  rows strips the trailer (overlay, record scans, seal, root validation,
  metering reads, queries); one random write per deleted item; reads at T before
  a live item's first listed change pay one side-table lookup.

### 3. Hybrid (`a3`)

- Cells as Lean (separate per-record tables, `Cell` rows unchanged), index as
  Embedded-capped.
- Only `Index` rows change format, and the index crate is the only one that
  reads them, so the change stays inside one crate. Index history was the larger
  cold cost on the stack (+170–190 ms of the +290 ms churn).

### Results (glade run `hal`, 2026-11-27)

Roots identical (7654fe83) and every query result identical across flat, a1,
a2, a3. Workspace test failures (all layout or fixture tests, none a wrong
result): a1 16, a2 68, a3 28 (a3 = union of a1 and a2).

**Writes**: median seal+commit ms; "hist" = minus no history; "rd pages" = disk
pages read per commit (cold misses, from `/proc/self/io`).

| | flat | a1 Lean | a2 Embedded | a3 Hybrid | no history |
|---|---|---|---|---|---|
| warm churn | 249 (+72) | 228 (+52), −8 % | **213 (+36), −14 %** | 218 (+42), −12 % | 177 |
| warm s300k | 183 (+46) | 169 (+33), −8 % | **139 (+3), −24 %** | 147 (+11), −19 % | 136 |
| cold churn | 637 (+290) | 586 (+240), −8 % | 500 (+154), −21 % | **463 (+116), −27 %** | 346 |
| cold s300k | 271 (+116) | 250 (+96), −8 % | **186 (+32), −31 %** | 219 (+65), −19 % | 155 |
| rd pages, cold churn | 5886 | 5063 | 4291 | **3874** | 2479 |
| rd pages, cold s300k | 1412 | 1111 | **758** | 953 | 589 |
| disk churn / s300k MiB | 2096 / 1456 | 1872 / 1280 | 1840 / 1216 | 1840 / 1264 | 1456 / 1136 |

The write counter (`write_bytes`) came out implausibly large (over 1 GiB per
commit warm), so only read pages are used.

Table sizes at commit 300 (churn): `CellHistory` 175.7 → 36.6 (a1, a3) or gone
(a2, `CellOlder` 29.9); `IndexHistory` 56.4 → gone in a2/a3 (`IndexOlder`
11.9); `Index` grows 35.6 → 42.4 with the embedded lists (s300k 20.8 → 38.1,
unique-value containers are small, so the list is a large share); `Cell` grows
273 → 275 in a2.

**Reads** (warm, ratio to flat; 1.0 = equal):

| | a1 | a2 | a3 |
|---|---|---|---|
| head ranges and points | 0.93–1.00 | 0.96–1.03 | 0.93–1.07 |
| historical ranges | 1.02–1.05 (0.44 at 250 back, empty) | **0.39–0.65** (0.15 empty) | **0.40–0.65** |
| historical points, quiet | 0.66–1.00 | **0.42–0.65** | **0.42–0.64** |
| historical points, busy | 0.80–1.02 | 0.68–0.96 | 0.64–0.92 |
| historical cell reads | 0.67–0.95 | **0.40–0.70** | 0.68–0.96 |
| `#recordKeys` bindings | 0.64–0.98 | **0.38–0.67** | 0.65–0.99 |

Cold reads at head first came out 1.2–1.8× slower for all three. A follow-up
showed this was noise, not layout: two builds of the same code differed as much
(stack build 1 vs 2: 2536 vs 4864 KiB on the same query), and one file changed
between runs. With readahead off and files flushed before eviction, two builds
read exactly the same pages, so cold reads are now measured that way (pages
visited). Clean head results, ratio to the stack in pages:

| at head | a1 | a2 / a3 |
|---|---|---|
| range 1k | 1.00 | 1.27 |
| range 10k early / mid | 1.00 / 1.00 | 0.98 / 0.56 |
| range 50k | 1.00 | 0.57 |
| busy / quiet points | 1.00 / 1.00 | 1.03 / 1.16 |
| cell reads, `#recordKeys` | 1.00 | a2 1.02, a3 1.00 |

The index differences come from leaf fill, not row size: on s300k the same
336k `Index` rows take 9371 leaf pages in a2/a3 against 5016 on the stack. Not
yet explained. Historical cold pages: a2/a3 ranges 40–64 % fewer; busy points
read 10–250 back 2× more (side-table lookup); a1 fewer at depth.

**Verdict**

- a2 is the most page-efficient: history costs +3 ms warm on s300k and half of
  flat's on churn, and historical reads are 1.5–2.5× faster. Price: `Cell` and
  `Index` row formats change, so every reader of live rows must strip the list.
- a3 gets most of it with the change confined to the index crate, and was the
  best cold churn result.
- a1 is the safe, small step (−8 % everywhere, no live-format change).
- What remains in a2/a3 on churn is one random side-table write per deleted
  item plus the ChangeSet appends; the creation bit removed the rest.
