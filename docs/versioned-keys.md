# [PROPOSAL] Versioned Trie Keys

Key trie rows by the commit that wrote them and their position, instead of by node hash. Roots,
proofs and every committed byte stay the same. No stores exist yet, so nothing needs migrating.

**Summary.**
- Each commit's trie rows become one sorted append. This removes most of the trie write cost: seal +
  commit is 1.6–2.3× faster per commit on Linux and 3.9–5.8× faster on macOS.
- GC follows from the layout: seal knows every row it replaces, lists it, and a sorted sweep deletes
  the lists cheaply (about +8 ms per commit on macOS, +14–21 ms on Linux).
- Cost: anything that points to a stored trie keeps a locator (hash + commit), not just the hash.
  Locators sit inside rows that are read anyway, so walks need no extra lookups (only one `Roots`
  read to find a commit's roots).

## Problem

Trie rows are keyed by node hash. Hashes are random, so each commit's ~20k new rows land on ~20k
different pages, and MDBX copies and writes every touched page.

- Commit cost follows pages touched, not bytes: 250–330 MiB written per commit for ~4 MiB of rows
  (macOS, 16 KiB pages). In a standalone MDBX test, the same rows cost 9 ms per commit with
  sequential keys and 488 ms with random keys.
- GC has the same problem: replaced rows sit on random pages, so deleting them rewrites about one
  page per row.

## Design

| Table | Key before | Key now |
|---|---|---|
| `CellTrie`, `IndexTrie` | `hashedNode` | `commit ‖ route` |
| `BitmapTrie` | `hashedNode` | `commit ‖ termPath ‖ route` |
| `BitmapContainer` | `hashedContainer` | `commit ‖ termPath ‖ hi48` |

- `commit` (`u64` BE) keeps each version of a position in its own row, so older roots stay walkable.
  Coming first, it makes each commit's writes one append at the end of the table. It is also the
  engine's existing label for states, and puts each commit's rows in one key range.
- `route` is the node's position: the nibbles of its routing path consumed to reach it, prefixed
  with their count, so shallow nodes (rewritten almost every commit) sort first and die together.
- `termPath` (the term's 32-byte routing path) keeps different terms' bitmap rows apart.
- The trie is unchanged: routing paths are still hashes (`CellTrie`, `IndexTrie`) or record-ID
  regions (`BitmapTrie`), so the tries stay balanced. Updates stay copy-on-write: a changed node
  gets a new row, and old rows stay for older roots.

This is the Jellyfish Merkle Tree's layout (Aptos: version and nibble path). Cosmos IAVL v1 also
puts the version first, but keys by version and nonce, not by position.

**Stored-only additions (not hashed).** A row used to be found by its hash; now its key is `commit ‖
position`. Walking down from a root gives the position for free (it is the path followed), but not
the commit. So every pointer to a row also stores what the key needs; the hashes stay, since they
are what is committed and what each loaded row is checked against.
- `child_commits` in each branch node: the commit that wrote each child row (8 B each in the draft;
  a varint would take 2–3 B, and the encoding can change later).
- `leaf_paths`, now also filled in `BitmapTrie` nodes: a container's key needs its full 48-bit
  region, which the walk only partly knows (6 B entries).
- `Index` value: `bitmapHash ‖ commit [‖ hi48]`, so a query can load a term's bitmap root.
- `Roots` table: the same for each commit's state and index roots. `#roots` still commits the
  hashes; `Roots` says where they are stored. It is mostly a cache: the state root at `T` is always
  the row `T ‖ empty route`, since every commit changes the state trie (invariant 4).

**Write path.** Seal stamps its rows with the next commit number (`head + 1`); the existing head
check ensures only one branch commits with it. Commit writes the rows in key order with MDBX `APPEND
| NO_OVERWRITE`, which skips the B-tree search (3–6% less commit time than `UPSERT`) and fails on a
reused key.

**Invariants.**
1. Within a commit, each position whose content changed gets one new row; rows are never modified.
2. A row is reachable from a contiguous range of roots `[c, s − 1]`: written at `c`, replaced at
   `s`, because each commit copies its pointers from the previous commit's trie.
3. Every row a commit replaces is one seal loaded (emptying a subtree walks every node in it), so
   seal can list them. A write of a cell's current value leaves its leaf hash unchanged, so no row
   is written or replaced.
4. Commit `T` writes the `#roots` cell of `T − 1`, so the state trie changes at every commit.

**Hash checks on load stay.** The engine already re-hashes every row it loads; with versioned keys
this must never be skipped, since a wrong commit can land on an older, valid row at the same
position.

## Results

Harness: golem-db-conformance-testing; no GC and no history tables. Commits hold 1,000 operations
(100 in "blocks of 100"); the 1M row averages its last 100 commits.

**Linux** (AMD Ryzen 7 2700X, 15 GiB RAM, ext4 on NVMe, 4 KiB pages; two rounds within 0–6%):

| Scenario | Hash keys, seal + commit | Versioned keys | Speed-up | File |
|---|---|---|---|---|
| 20k records, blocks of 100 | 71 ms | 35 ms | 2.0× | −29% |
| 100k creates + 200k churn (*churn*) | 594 ms | 367 ms | 1.6× | −28% |
| 300k creates (*s300k*) | 551 ms | 301 ms | 1.8× | −30% |
| 1M creates | 835 ms | 366 ms | 2.3× | −27% |

The commit step alone is 3.5–5.9× faster; seal is mostly unchanged and is now 75–84% of the time.
The gap widens with size (1.5× at the start of the 1M run, 2.3× at the end).

**macOS** (Apple M3 Pro, 18 GiB RAM, 16 KiB pages; two rounds):

| Scenario | Hash keys, seal + commit | Versioned keys | Speed-up | File |
|---|---|---|---|---|
| 20k records, blocks of 100 | 67 ms | 17 ms | 3.9× | −30% |
| 100k creates + 200k churn (*churn*) | 1,141 ms | 208 ms | 5.5× | −29% |
| 300k creates (*s300k*) | 910 ms | 158 ms | 5.8× | −31% |

Hash keys are much slower on macOS than on Linux, and not because of MDBX's page size: forcing 4 KiB
pages on macOS makes hash keys 15–61% slower still, while versioned keys change by only 2–8%.

## Alternatives considered

- **`commit ‖ hash` keys.** A wrong commit becomes a plain miss, so hash checks on load would be a
  defence rather than a requirement. Benchmarked on Linux (churn and s300k): no speed difference,
  and files 7–8% larger (longer keys).
- **Path-only keys (reth).** Without the commit prefix, rows are keyed by hashed paths alone, so
  each commit's writes still land on random pages; and overwriting in place loses old roots, which
  the design requires.
- **Tuning MDBX.** Neither page size nor batching changes the cause: random keys touch one page per
  row.

## Trade-offs

- **Locators.** Anything that points to a stored trie (`Roots`, `Index`, `IndexChangeSet`) must keep
  the commit with the hash; a node can't be found by hash alone.
- **No dedup.** Identical rows of different terms or commits are stored separately (+8% container
  rows, though the table is still 21% smaller).
- **Child commits** add up to 128 B per branch row; bitmap keys grow to 41–47 B (trie keys shrink to
  about 12 B).

**Open.**
- The read path (queries, historical queries, proofs) is not benchmarked. Walks make the same
  lookups, plus one `Roots` read per request at a past commit, so it should be unchanged.
- Behaviour beyond RAM.

## GC

The layout makes GC much cheaper and somewhat simpler. Each row is replaced or removed at most once,
at a commit seal knows (invariants 2 and 3), so seal can list the rows it replaces instead of
keeping reference counts, and a sweep deletes them once they leave the retention window. The rest of
GC (batching, progress, crash recovery) is similar work either way. This is the Jellyfish Merkle
Tree's stale-node index. Design, options and measurements: [gc-options.md](gc-options.md); GC code
exists only as prototype patches in [`prototypes/gc/`](../prototypes/gc/).

## Changes to `golem-db-design.md`

| Section | Change |
|---|---|
| Glossary (l. 174, 179) | Trie rows keyed `commit ‖ route`; `Superblock` is no longer the only uncommitted table. |
| §2 Table Dictionary (l. 315–398) | New trie keys; locator bytes in `Index` and `IndexChangeSet`; add `Roots`; update table counts. |
| §2 `BranchNodeCompact` (l. 496–527) | Add `child_commits`; `BitmapTrie` fills `leaf_paths` (6 B entries). |
| §4 Superblock (l. 867–893) | Bump the format version. |
| §4 Roots as Cells (l. 1174–1221) | Historical walk reads `Roots[T]` and checks it against `#roots`. |
| §6 (l. 1344–1493), Figures 5, 6 | Containers keyed `commit ‖ termPath ‖ hi48`; drop cross-term dedup; `IndexChangeSet` stores locators. |
| §7 (l. 1530–1630), Figure 7 | Historical bitmaps found through locators. |
| §8 (l. 1729–2004), Figure 8 | Bare-leaf "probe and verify" (l. 1799) no longer applies; proofs need `Roots[T]`; add `Roots` to the uncommitted list; future path 2: physical layouts stay interchangeable without touching the commitment, and the default becomes versioned keys, which are already path-based and keep history. |
| §9 (l. 2016–2108) | Physical layout becomes `commit ‖ route`; add JMT as precedent; add write locality to the trade-off table. |
| §10 (l. 2512–2649) | Seal writes at the next commit (`head + 1`); persist appends in key order; write `Roots[commitNr]`. |
| Appendix A (l. 3358) | Physical key layouts are free (non-normative). |

## Changes to `decisions_for_architect.md`

| Item | Change |
|---|---|
| D03 crash recovery (l. 618–663) | Check `Roots[head]` against the head on open. |
| D05 service matrix (l. 829) | Historical queries use locators. |
| D06 proofs (l. 969–1078) | Proofs read `Roots[T]`; bitmap proofs need `IndexChangeSet` unless commits are stored beside `leaf_value_hashes`. |
| D09 / S05 (l. 1087–1145) | Locators, `child_commits` and keys are free bytes; item 9.7 must allow 40–46 B `IndexChangeSet` values. |

Also: register the decision in `CHANGES.md`; metering formulas are unchanged, but the physical cost
behind `w_*_trie_update` has moved.
