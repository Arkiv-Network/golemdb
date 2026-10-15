# Review of GC options for Trie storage

How to delete dead trie rows, on today's hash keys and on the [versioned keys](versioned-keys.md)
proposal. All results come from prototypes ([`prototypes/gc/`](../prototypes/gc/)).

**Summary.**
- With a retention window, a stale list with a sorted sweep (C) on versioned keys is by far the
  cheapest: it adds about 4–6% to each commit on Linux and 3–5% on macOS.
- Our prototypes adding a window to PR #35's reference counts (on hash keys) add 47–159% to each
  commit; an optimized version would still likely add 30–45% on Linux (estimate).
- Overall, with a window, versioned keys + C commit 2.8–4.4× faster than hash keys with windowed
  reference counts on Linux and 7.5–9.2× faster on macOS. Against an optimized version (estimate),
  versioned keys + C would still be about 2× faster on Linux.
- Any window keeps the old trie rows each commit replaces: 7–10 MiB per commit of 1,000 operations,
  whichever GC is used. Over 90% are only needed for proofs at past commits, so a shorter window for
  proofs would save most of that space ([details](#appendix-how-much-history-costs)).
- The recommendation is provisional: if the design moves to versioned keys, C is the candidate to
  build next.

## Problem

Every commit rewrites the trie paths it changes. The old rows stay for older roots in the retention
window (`W` commits) and become garbage once no retained root reaches them; GC deletes them. Pruning
the history tables and segment shards is separate.

In MDBX a delete costs like a write: it copies the page holding the row (and the branch pages above
it) and writes the copy at commit. The cost is per page touched, not per row, so deleting rows
spread over many pages costs about one page write each, while rows on the same page share one.

D05 requires every node to keep at least `#minRetention` commits, so GC must keep every row
reachable from any root in `[head − W, head]`, with `W ≥ #minRetention`. Head-only GC (PR #35's
reference counts as they are now, or mark-and-sweep) meets that only if `#minRetention` is 0. The
windowed candidates are reference counts on hash keys (A1, A2), and a stale list (B, C) or diff walk
(D) on versioned keys.

## Techniques

- **A. Reference counts on hash keys (PR #35).** Count each node's parents; delete it at zero. The
  PR keeps only the head. Two prototypes add a window:
  - **A1** releases a commit's roots `W` commits later (the PR's planned approach).
  - **A2** counts as the PR does but delays each delete by `W` commits.
- **B. Stale list.** Seal already knows which rows it replaced; it lists them, and they are deleted
  once their last root leaves the window. This is the Jellyfish Merkle Tree's stale-node index.
- **C. Stale list + sorted sweep.** As B, but deletes are bundled by page: the lists of `K` commits
  are merged in key order, so dead rows on the same page are deleted together and share one page
  copy ([design](#design-of-c)).
- **D. Diff walk.** No list: compare the tries of two consecutive commits and delete what only the
  older one has (as Cosmos IAVL v1 does). **D + sweep** adds C's sorted deletion.
- **E. Mark-and-sweep.** Every `N` commits, mark everything the head reaches and delete the rest. No
  bookkeeping, but each run walks the whole database. Prototyped head only; with a window it would
  have to mark from every retained root.

B, C and D need versioned keys. On hash keys a key names content: a row listed as dead may still be
used by another term's bitmap, or become live again when the same content is written later. Only
reference counts (A) or a full mark (E) can tell. Reference counts would also work on versioned
keys, but aren't needed there: each row is replaced or removed at most once, at a commit seal knows
(seal loads every row it replaces while rewriting the path), so seal can list it directly.

**Also considered on hash keys** (not prototyped):
- **Stale list with a revival check:** works for `CellTrie`/`IndexTrie`, but not for bitmaps, whose
  rows different terms can share, unless bitmap keys include the term.
- **Batched reference counts:** applying count changes in sorted batches cuts count writes, not
  scattered deletes or cold reads.
- **Mark-and-sweep with a Bloom filter** (Geth's offline pruner): less memory than E, same full
  walk.
- **Copying live state** into a fresh table: sequential writes, but each cycle copies all live
  state.
- **Reference counts in memory** (Geth's old approach): short-lived nodes never reach disk, but
  commits are no longer fully durable.

All still pay scattered deletes or a full walk, since dead rows sit on random pages.

## How the techniques compare

| | Finding dead rows | Deleting them | What must stay exact |
|---|---|---|---|
| A1 | count changes in seal, plus a walk of the released roots | scattered pages | every node's count, across the whole database and every commit |
| A2 | count changes in seal, as PR #35 | scattered pages | as A1, plus revived nodes |
| B | seal lists them (almost free) | scattered pages | each commit's list of replaced rows, built where seal already has them |
| C | as B | bundled by page | as B, plus the sweep's progress across crashes |
| D | re-reads the changed paths of two old tries | as B or C | the diff of two tries, across node splits and merges |
| E | marks every live row, scans all trie tables | in key order | nothing beyond marking from the roots; its cost is the problem (high latency spikes) |

All techniques target the same rows: those no root in the window reaches. A, B, C and D handle about
the same number of dead rows per commit; they differ in how many reads and page rewrites each one
costs. E's cost grows with the database, not with the commit. Supporting a window is free for B, C
and D (deletes just wait `W` commits), but not for A (deaths are found late from cold nodes in A1,
or held with markers in A2) or E (it must mark from every retained root).

## Results

Seal + commit per commit with `W` = 100 (plain A is head only), as *churn / s300k*. *churn*: 100k
records, then commits of 1,000 mixed creates, patches and deletes. *s300k*: 300 commits of 1,000
creates. GC cost is against a no-GC run in the same benchmark batch, so totals don't always equal
the no-GC row plus the cost.

**Linux** (AMD Ryzen 7 2700X, 15 GiB RAM, ext4 on NVMe, 4 KiB pages):

| Technique | Keys | Seal + commit | GC cost |
|---|---|---|---|
| *No GC* | hash | *595 / 555 ms* | |
| *No GC* | versioned | *373 / 314 ms* | |
| A. Reference counts (PR #35; no window, head only) | hash | 674 / 651 ms | +79 / +96 ms |
| A1. Reference counts, window | hash | 1,510 / 1,437 ms | +915 / +882 ms |
| A2. Reference counts, delayed deletes | hash | 1,093 / 1,018 ms | +498 / +463 ms |
| B. Stale list | versioned | 464 / 384 ms | +96 / +73 ms |
| **C. Stale list + sorted sweep** | versioned | **394 / 328 ms** | **+21 / +14 ms** |
| D. Diff walk | versioned | 525 / 436 ms | +152 / +130 ms |

D + sweep is left out on Linux; see [benchmark notes](#appendix-benchmark-notes).

**macOS** (Apple M3 Pro, 18 GiB RAM, 16 KiB pages):

| Technique | Keys | Seal + commit | GC cost |
|---|---|---|---|
| *No GC* | hash | *1,141 / 910 ms* | |
| *No GC* | versioned | *226 / 169 ms* | |
| A. Reference counts (PR #35; no window, head only) | hash | 490 / 497 ms | −651 / −413 ms |
| A1. Reference counts, window | hash | 2,002 / 1,333 ms | +862 / +424 ms |
| A2. Reference counts, delayed deletes | hash | 2,155 / 1,334 ms | +1,014 / +425 ms |
| B. Stale list | versioned | 375 / 275 ms | +163 / +116 ms |
| **C. Stale list + sorted sweep** | versioned | **233 / 177 ms** | **+8 / +9 ms** |
| D. Diff walk | versioned | 403 / 304 ms | +184 / +140 ms |
| D + sweep | versioned | 254 / 197 ms | +35 / +33 ms |

On macOS, head-only A (and E, in the appendix) makes hash keys faster than no GC: the smaller file
means fewer scattered page writes.

- **C is by far the cheapest:** +4–6% per commit on Linux, +3–5% on macOS (three interleaved rounds
  varied by a few percent). Its tail grows more, p99 +15–21% on Linux and +10–17% on macOS, from the
  commit that opens a batch; the proposed cursor merge removes that step.
- **Reference counts with a window add 47–159%.** Old and new versions no longer cancel out, so seal
  reads dying nodes from disk and updates many more counts. Batching A2's deletes makes it worse
  (very large spikes): hash keys scatter the dead rows.
- **B and D add 23–86%.** B deletes each commit's rows on scattered pages; D also re-reads old tries
  to find them, which costs about 11–18% more than B.
- **Files** (s300k): C 1.7 GiB, A1/A2 2.4–2.5 GiB. Without GC: versioned 2.3–2.4 GiB, hash 3.4 GiB.

**Comparison with PR #35's own report.** On the one scenario both ran (20k records, blocks of 100),
PR #35 adds 13% over no GC in its report and 18% in ours; the gap is its stacked node cache (PR
#38), which our runs don't include. Its other scenarios stay below about 45k records, all in RAM; at
100k–300k records its counting in seal adds 21–25% per commit on Linux; the net cost is lower
(13–17%) because the smaller database makes the commit step cheaper.

**Optimizing reference counts.** A2 is already the efficient form of A1: it learns deaths while seal
has the nodes in memory. It could detect revived nodes more cheaply, and the node cache would speed
up counting. But deletes stay scattered (about 12–17% of a hash-key commit on Linux, judging by B)
and counting adds about 21–25% in seal at these sizes (head-only A gets some of that back from a
smaller database, which a window largely loses), so an optimized version likely adds 30–45% per
commit on Linux. This is an estimate, not a measurement.

## Design of C

On [versioned keys](versioned-keys.md), following the Jellyfish Merkle Tree's stale-node index.

**Rule.** A row replaced at commit `s` is used only by roots before `s`. So it can be deleted once
root `s − 1` leaves the window, i.e. once `s ≤ head − W`. The row's own commit says nothing: an old
row the head still reaches is live.

**Recording.** For every row commit `s` replaces, seal appends `Stale[s ‖ group ‖ table ‖ key]`
(empty value) in the same transaction. The table is in the key because tables share key formats.
Groups separate rows that only proofs need (`CellTrie`, `IndexTrie`) from bitmap rows, which
historical queries and bitmap-membership proofs need. So the two could be kept for different
windows, with bitmaps kept at least as long as `IndexChangeSet`.

**Sweep.** Each commit's stale rows are written in key order. Every `K` commits, the sweep merges
`K` commits' lists, one cursor per commit, into one key-ordered stream, with no sort needed. It
deletes a `1/K` slice per commit, each dead row together with its `Stale` row. Dead rows on the same
page then come one after another and share one page copy. (Aptos prunes the same index on RocksDB,
where deletes are cheap; MDBX copies a page per delete, which is why C deletes in key order.)

**Progress.** `Superblock/gc` records, per group, how far the sweep has got: which commits' lists
are fully deleted, and which batch is open. A batch opens only once all its commits have left the
window. A deleted row's `Stale` row goes in the same transaction, so after a crash the sweep just
reopens its cursors and continues with what's left.

**Where it runs.** Inside the commit's transaction: one fsync, and progress stays atomic with the
commit. A GC failure then fails the commit, so cap GC work per commit and keep an off switch. If GC
falls behind (capped or switched off), stale rows pile up and storage grows until it catches up. A
separate transaction could run between blocks instead; that isn't measured.

**Rewind** (considerations for the future). Versioned keys would let rewind undo a commit by
range-deleting that commit's rows, `Stale` rows and `Roots` row: one sequential delete per table,
with nothing to read first. With reference counts on hash keys, the undone commit's rows can't be
found by key, and a node it wrote may be identical to one an older state still uses, so every count
change would have to be reversed, with cascades through cold nodes. This comparison is speculative,
since rewind is not designed.

## Changes to other documents

| Item | Change |
|---|---|
| `golem-db-design.md` §2, §4, §8 | Add the `Stale` table and the `Superblock/gc` key; both are uncommitted. |
| `golem-db-design.md` §7, §10 | GC via stale rows; persist writes the stale rows and the sweep's deletes. |
| D03 crash recovery (l. 618–663) | Resume the sweep; a GC failure fails the commit. |
| D04 concurrency (l. 578–617) | GC slices run inside the commit critical section, capped, with an off switch. |
| D05 §5.3 (l. 862–870) | GC via stale lists and a sorted sweep inside the commit. |

## Appendix: how much history costs

Any window keeps the dead rows of `W` commits, whichever GC is used. Measured per commit (key and
value bytes of every row that dies, 1,000 operations per commit):

| Database size | Dead rows | Proof history (`CellTrie`, `IndexTrie`) | Query history (bitmaps) | Total |
|---|---|---|---|---|
| 100k–200k records | 14,100 | 6.4 MiB | 0.2 MiB | 6.6 MiB |
| ~250k | 15,700–18,400 | 7.4–7.5 MiB | 0.2–0.7 MiB | 7.6–8.2 MiB |
| 900k–1M | 19,800 | 9.3 MiB | 0.5 MiB | 9.8 MiB |

- About 7–10 KiB per operation, growing with database size.
- `W` = 10,000 commits keeps about 65–100 GiB; a day of 2-second blocks about 290–420 GiB, before
  history tables and page overhead.
- **Proof history is over 90% of it, so a shorter proof window would save most of the cost.**
  Historical point reads use `CellHistory`/`CellChangeSet`, not the tries, and historical queries
  need only the bitmaps. A node could keep reads and queries for a long window and proofs for a
  short one (D05 §5.2 option B), as long as bitmaps are kept at least as long as `IndexChangeSet`.
- **This should drive `#minRetention`.** Today it covers proofs too (design §1 property 5), so using
  this lever means splitting it, for example into a separate proof-retention minimum: a requirements
  change for product. `#params` is fixed at genesis, so the choice can't change later for an
  instance.

## Appendix: head-only results

Only relevant if `#minRetention` is 0. Seal + commit per commit, churn / s300k; E's p99 is its sweep
spike.

| Technique | Keys | Linux | macOS |
|---|---|---|---|
| *No GC* | hash | *595 / 555 ms* | *1,141 / 910 ms* |
| A. Reference counts (PR #35) | hash | 674 / 651 ms | 490 / 497 ms |
| E. Mark-and-sweep, every 10 | hash | 653 / 714 ms, p99 2.3 / 3.5 s | 516 / 571 ms, p99 1.3 / 1.9 s |
| E. Mark-and-sweep, every 100 | hash | 624 / 607 ms, p99 3.9 / 3.8 s | 645 / 615 ms, p99 2.4 / 2.4 s |
| *No GC* | versioned | *371 / 309 ms* | *212 / 161 ms* |
| C. Stale list + sorted sweep, `W` = 0 | versioned | 395 / 331 ms | 218 / 170 ms |
| E. Mark-and-sweep, every 100 | versioned | 418 / 367 ms, p99 3.5 / 3.1 s | 241 / 195 ms, p99 1.9 / 1.7 s |

## Appendix: benchmark notes

- **Setup:** golem-db-conformance-testing, release builds; commits 200 onward, two or three rounds
  in alternating order; `K` = 50.
- **Correctness:** property tests over 200–300 random commits check that A1, A2, B and D keep
  exactly the rows the window's roots reach, E exactly the head's, and C never deletes a reachable
  row. Not yet covered: a no-leak check for C, rewinds, crashes.
- **Baselines** drift by a few percent between batches, so each technique is compared with its own
  batch.
- **Linux D + sweep** is left out of the Linux table: its seal is about 17% slower on every commit
  than D's, and each batch opening causes a large spike, with the same build. The prototype keeps
  two batches of keys in memory; C keeps one, and the proposed design none.
- **Small biases:** the no-GC baseline also builds seal's dead-row lists (favours B–D slightly); E's
  sweep copies values it doesn't need (against E).
- **The A1/A2 prototypes** were written for this comparison and haven't been reviewed by PR #35's
  author.

Sources: [PR #35](https://github.com/golemfactory/golem-db/pull/35), [cosmos/iavl
`nodedb.go`](https://raw.githubusercontent.com/cosmos/iavl/master/nodedb.go), [`jmt` storage
docs](https://rustdoc.penumbra.zone/main/jmt/storage/index.html).
