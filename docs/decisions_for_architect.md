# Decisions for the Architect

Spec decisions raised by the design change programme (`CHANGES.md`, kind K2), written so that a
reviewer who has not followed the programme can judge each one from this file alone. One entry per
decision. An entry moves from **proposed** to **decided** when the architect records a choice below;
the spec text then lands in `golem-db-design.md` and the entry's *Outcome* is filled in.

Each entry has the same shape: the question as the current text leaves it, with citations · the
serious alternatives with their trade-offs · a recommendation and its rationale · what would change
the recommendation · the fixture or trace that closes the decision.

Conventions: `§n` refers to `golem-db-design.md`; "API" to `golem-db-api.md`, both in this
directory.

## Index

Status values: **waiting** (brief written, architect's choice pending) · **decided** (Outcome
recorded; spec text pending) · **done** (spec text landed, register row closed). `[API]` marks a
decision whose recommendation changes `golem-db-api.md`. "Blocks" lists what cannot proceed until
the decision lands. **#** is the suggested order of decision — dependencies first — and is stable:
a new decision takes the next free integer; entries below appear in this order and carry it in
their heading.

| # | Decision | Status | Question in one line | Recommendation in one line | Depends on | Blocks | `[API]` | Pri |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | [D08](#1--d08--recordkeys-on-delete) | DONE as recommended | Does the `#recordKeys` binding survive a delete? | No — delete removes it; re-create is an ordinary `create` | — | D07, S10 | — | P1 |
| 2 | [D07](#2--d07--branch-transitions-delete-visibility-exact-rollback-net-diff-cancelled-changes) | partly resolved (2026-10-10): 7.1-A implemented on `feature/golem-db-api` (delete tombstones every live cell, no deleted-record set); 7.3 appears implemented; 7.2, 7.4 to verify; design §10 text pending | How do delete, rollback and the net diff interact in a branch? | Drop the deleted-record set (tombstone every cell); three-valued pre-image; net diff = entries ≠ origin; cancelled changes leave nothing | D08 | S10, Epic CRUD | — | P1 |
| 3 | [D01](#3--d01--filter-evaluation) | waiting, needs more design work | How is the API's ordered DNF evaluated over the two-tier index, and what about negation, match-all, caps, cost? | Mechanism as pseudocode; caps in `#params`; flat DNF only; canonical cost includes region pruning. **Negation/match-all: live-set index term (1.1-B/1.2-B) if P08 confirms the live DSL's standalone `!=` must survive** — else ANDNOT-in-group, no match-all. 1.5: negation MongoDB-style (absent and other-typed cells match) | P08 | S07, Epic 8, D10 | yes — strike "nesting"; "≥ 1 positive literal" only if P08 = no | P1 |
| 4 | [D04](#46--d04--d03--d02--concurrency-contract-crash-recovery-rewind) | partly resolved (2026-10-10): read half implemented and in the API spec (handle checked against head in the operation's own snapshot; commit rechecks head inside the writer); segment ordering waits for segments | What guarantees a read never mixes two states, and where is the commit race decided? | One MDBX read txn per operation with head check inside; commit mutex over guard → segment fsync → MDBX txn | D13 (sync mode), P05 | D03, D02, S08, S09 | — | P1 |
| 5 | [D03](#46--d04--d03--d02--concurrency-contract-crash-recovery-rewind) | partly resolved (2026-10-10): deterministic vs environmental errors exist in the API spec via `StoreFull`; `Io` error and restart procedure open | What happens on restart and on failure mid-commit? | Five-step restart; failure table by point; deterministic vs environmental errors | D04 | S08 | yes — new `Io` error | P1 |
| 6 | [D02](#46--d04--d03--d02--concurrency-contract-crash-recovery-rewind) | waiting (2.1 decided), likely v2 (not necessary with bft) | Does `rewind` exist, what does it undo, and what does "commit 100" mean afterwards? | **Semantics specified; feature deployment-optional, not in v1** (aligned with requirements OQ4); undo by change-set replay, one txn per commit, MDBX then segments; root-based identity for cursors and `at` | D04, D03 | D11, D05, S08 | yes — `Stale` error; optional root on `at`; `rewind` definition | P1 |
| 7 | [D05](#7--d05--retention-and-historical-discovery) | ok to go, needs impl (5.2 minimum decided) | Retention: what survives per read class, how far back, what a read past the window returns, how deleted terms/cells are discovered; history after snapshot sync | Service matrix; **`#minRetention` instance parameter; consensus path refuses beyond it on every node; surplus via an archival surface** (aligned with CS-5/DI-7); GC root = every `#roots` entry retained; discovery by scanning the history tables; no cursor lease; synced node exempt for one window | P06, D02 | D06, D12, D17 | yes — `retention()`; archival surface | P1 |
| 8 | [D06](#8--d06--proof-scope-and-the-non-inclusion-witness) | waiting, matthias tries to work on this next week | Which statements are provable, and how is absence of a virtual leaf witnessed? | Inclusion/non-inclusion of cells, bindings, terms, memberships; range completeness a non-goal; nested leaf preimage `Hash(0x00 ‖ key ‖ Hash(tag ‖ value))` with the inner hash stored beside `leaf_paths` | D05, P07 | D18, D09 | yes — proofs section | P1 |
| 9 | [D09](#9--d09--normative-encoding-profile) | waiting  | The exact bytes an independent encoder must reproduce | 13 items pinned: Roaring portable 32-bit with `runOptimize` rule; pad nibble 0; `EMPTY_ROOT = Hash(0x07)`; reserved cells use grid types; zero-length = absent; decoders reject non-canonical; 10 vector sets | D01, D06 | S05, Epic 6 | — | P1 |
| 10 | [D13](#10--d13--environment-assumptions) | waiting | What is assumed of MDBX, the filesystem and RAM, and who may reject on memory? | State `SYNC_DURABLE`, MVCC readers, no clock/randomness; engine imposes **no** node-local memory caps (they would fork consensus) — publishes the formula, host bounds via gas limit | — | D04, S06 | — | P2 |
| 11 | [D10](#11--d10--assumed-metering-shape-and-activation-semantics) | decided (2026-10-10): 10.2 = A (branch base); 10.3 adopted (`#minActivationDelay`); 10.4: missing weights are unpriced, warn logs, names fixed per model version | What metering shape does the design assume, and what does "activation at A" mean? | Import the API's cost contract as a premise; price by the commit being produced (fork semantics); `#minActivationDelay` in `#params`; check weight completeness at install, not at A | D01, D13 | §5/§10/§13 cost claims | yes — install-time check, `priced_at` | P2 |
| 12 | [D14](#12--d14--cost-qualifications) | partly decided (2026-10-10): 14.1 = canonical `⌈N log₂ N⌉ × S` as in metering D6; 14.3 partly covered by metering D2; 14.2, 14.4 open | Which cost claims are lookup counts rather than totals? | Sort = N fetches + O(N log N) compares charged per record; warm cache: IDs only or IDs + keys, both stated; history bitmap growth and rewrite stated; quantities table | D10, D13 | — | — | P2 |
| 13 | [D11](#13--d11--cursor-contract) | decided for proposal, waits for impl | Is the cursor deterministic, what does the fingerprint cover, what wins between cursor / offset / `at`? | Cursor is a pure function of (query, state, position) — `machineId` leaves the engine; same-sequence fingerprint as an exclusion list; cursor + offset = `InvalidQuery`; root check → `Stale` | D02 | — | yes — cursor fields, precedence | P2 |
| 14 | [D12](#14--d12--live-paging-guarantee) | waiting, should be clear, needs check/re-reading | What does a cursor guarantee under live paging? | Freedom from position-shift anomalies only; membership and values may change; no retention lease — narrow the adjective, keep the mechanism | D05 | — | — | P2 |
| 15 | [D18](#15--d18--is-the-reserved-record-layout-part-of-the-commitment-contract) | done | Must a second engine reproduce §4's bookkeeping to match the root? | **Yes — by requirement** (DI-2 puts engine state under the root; NF-8 makes commitment vectors part of swappability); the partitioned-root alternative is withdrawn | P07 (decided) | S05 | — | P2 |
| 16 | [D15](#16--d15--conflict-vs-handleinvalid-for-a-stale-handle) | decided (2026-10-10): B, `HandleInvalid` everywhere; landed in the API spec, design §10 and metering; implementation to follow | Two errors for one condition? | One: `HandleInvalid` everywhere, `commit` included; `Conflict` only for `expected_version` | D04 | — | yes | P3 |
| 17 | [D16](#17--d16--refuse-a-second-consecutive-rollback) | done in the API spec (2026-10-10): A, multi-frame rollback, `NoFrameToRollback` once every frame is undone; register row to close | Refuse a second consecutive `rollback()`? | `rollback()` on an empty open frame is an error and pops nothing; multi-frame undo dropped | D07 | — | yes | P3 |
| 18 | [D17](#18--d17--typed-segment-columns) | waiting, needs more thinking | Typed segment columns? | No — raw bytes; typing would pin every host format at genesis | P05 (decided: adopted) | — | — | P3 |
| 19 | [D19](#19--d19--per-commit-log-digest-what-it-is-and-where-it-is-committed) | waiting | SE-1 requires a per-commit digest of appended log rows: what is it and who commits it? | Domain-separated digest over row hashes with the start ordinal bound in, returned by `seal`; **engine commits it lag-one** in a new `#logDigests` system record, so `AppHash` stays one root | P05, D09 | §11 final | yes — `seal`/`commit` outputs | P2 |

Cross-cutting `[API]` changes accumulated so far, to land as one `[API]` PR once the decisions above
are taken: strike "nesting" from `LimitExceeded`; require ≥ 1 positive literal per group and ≥ 1
group (D01) · add `Io` and `Stale` errors; optional expected root on `at`; a definition paragraph for
`rewind` (D02/D03) · `retention()` introspection (D05) · provable-statement table and completeness
non-goal in *Proofs* (D06) · `#minActivationDelay` in the install rule (D10, landed); `priced_at` = branch base (D10 10.2-A, landed) · cursor fields (`root` in, `machineId` out), fingerprint rule, precedence table (D11) ·
`HandleInvalid` on `commit`, `Conflict` narrowed (D15, landed) · `rollback` stays multi-frame with `NoFrameToRollback` when exhausted (D16, landed) · per-commit log digest returned by `commit`/`seal` (D19).

Product decisions (K4 rows and product questions surfaced by the briefs) are collected in one table
at the [end of this document](#product-decisions), after the spec decisions.

---

## 1 · D08 — `#recordKeys` on delete

**Status:** waiting_on_additional_input — written up, awaiting the architect's choice. **Register:** `CHANGES.md` D08, P1, track L.
**Bears on:** §4 `#recordKeys`, §10 change-set log and rollback, D07 (branch transitions), S10
(per-operation table).

### Question

`#recordKeys` (system record 3) holds one cell per record key, `recordKey → recordID`. When a record
is deleted, does that binding cell stay in committed state or is it removed? The document currently
says both:

| Passage | Claim | Implies |
| --- | --- | --- |
| §4 *Historised re-creation*: "re-creating a deleted key `patch`es it to the new `recordID`" | the cell is still there to be patched | binding **survives** delete |
| §4 *Non-existence proofs*: "no record with key K is a trie non-inclusion proof at `Hash(#recordKeys ‖ K)`" | a deleted key must be absent from the trie for this proof to exist | binding **removed** by delete |
| §10 rollback table, inverse of `delete`: "restore the key binding" | there is something to restore | binding **removed** by delete |
| API `delete`: "Re-creating the same key later is an ordinary `create`" | not a patch | binding **removed** by delete |

So §4 contradicts itself as well as §10 and the API. One rule has to be picked and the other passages
brought in line.

### Alternatives

**A — `delete` removes the binding.** The `#recordKeys[K]` cell is deleted like the record's own
cells: a `0x00` tombstone in the branch overlay, a `CellChangeSet` pre-image holding the old
`recordID`, a `CellHistory` entry. Re-creating K is an ordinary `create` that writes a fresh binding
cell with an absent pre-image.

**B — the binding survives; re-create patches it.** `delete` leaves `#recordKeys[K] = R1` in place.
"Does K exist" is answered by following the binding to `R1` and checking `(R1, #key)` or a
deleted-marker. Re-creating K `patch`es the binding to `R2`.

**C — `delete` patches the binding to a sentinel** (e.g. `recordID 0`, which no user record can
hold). Existence is one lookup plus a value test; re-create patches the sentinel away.

| | A — removed | B — survives | C — sentinel |
| --- | --- | --- | --- |
| "Does K exist?" | one point read | two point reads | one read + value test |
| "No record with key K" proof | non-inclusion at `#recordKeys`, as §4 states | inclusion of the binding **plus** non-inclusion of `(R1, #key)`; "never existed" and "deleted" are different proofs | inclusion proof showing the sentinel; the sentinel becomes a normative encoding (D09 grows) |
| Historical key read at T | time-travel the binding cell → `R_T` → cells | same | same |
| Re-create | ordinary `create` (matches API) | `patch` of a system cell (contradicts API wording) | `patch` |
| Cost per delete | +1 cell tombstone, +1 trie path | none extra | +1 cell write, +1 trie path |
| Cost per re-create | a create | a patch (≈ same) | a patch |
| Agrees with today's text | §4 proofs bullet, §10, API | §4 re-creation bullet only | none |

### Recommendation: A

**Rationale.**

1. **It is what two of the three passages and the API already assume.** Only one bullet in §4 has to
   change; §10's rollback rule and the API's "ordinary `create`" are already correct under A.
2. **It keeps the stated proof property.** §4 promises that "no record with key K" is a single
   non-inclusion proof. Under B that promise is false after the first delete of K; under C it becomes
   an inclusion proof of a sentinel, which is a different and weaker statement to a verifier who does
   not know the sentinel convention.
3. **Existence is one lookup.** Every key-addressed operation begins by resolving the key. B adds a
   second read to every such operation on a deleted key, and complicates the branch overlay's
   fall-through rule (§10) for the same case.
4. **History is not lost.** Under A the binding cell's history is `[create, delete, create]` with
   pre-images `∅, R1, ∅`. "Which IDs has K ever had" is answered from the same cell as under B, at the
   cost of one extra history entry per delete/re-create cycle.
5. **The extra cost is marginal.** A `delete` already writes one tombstone and one trie-path rewrite
   per cell of the record. A adds one more of each: for a record with *n* cells, +1/(n+1) — about 10 %
   for a ten-cell record. Per create/delete cycle A and B do the same total work; A pays at delete, B
   at re-create.

**Steelman for B.** A single unbroken value timeline per key (`R1, R2, R3 …`) with no absences is
tidy, and a delete that is never followed by a re-create is one trie path cheaper. It loses because it
breaks the proof property to buy that path, and because Arkiv's expiry model does not re-create
expired keys at any rate that would make the saving visible.

**C is rejected** as B's proof problem in a different form plus a new normative constant.

**What would change the recommendation:** a workload dominated by delete-without-re-create where the
one extra trie path per delete is measurable against the *n* paths the delete already writes; or a
requirement that "K was deleted" be distinguishable from "K never existed" *by proof against the
current root alone* (A distinguishes them only through history).

### Consequence for other open items

Under A, an in-branch `delete` must write a **tombstone for the binding cell in the overlay**, not
only add the record to the deleted-record set. §10 today relies on the deleted set alone for in-branch
lookups — sufficient for reads, but the committed binding would otherwise be missing from the net diff
at commit. This is recorded for D07 (branch transitions) and S10 (per-operation table); it is not
decided here.

### Fixture that closes the decision

```
commit 1   create K → R1            #recordKeys[K] = R1        CellChangeSet(1, #recordKeys‖K) = ∅
commit 2   delete K                 #recordKeys[K] absent      CellChangeSet(2, #recordKeys‖K) = R1
commit 3   create K → R2            #recordKeys[K] = R2        CellChangeSet(3, #recordKeys‖K) = ∅

CellHistory(#recordKeys ‖ K) = [1, 2, 3]

get(K) at 1  →  R1's cells           get(K) at 2  →  NotFound          get(K) at 3  →  R2's cells
non-inclusion proof for Hash(#recordKeys ‖ K) against root_2 verifies
inclusion proof for the same path against root_1 yields R1, against root_3 yields R2
```

### Text to change on acceptance

- §4 `#recordKeys`, bullet *Historised re-creation* → rewrite: delete removes the binding (pre-image
  = old ID); re-create writes a new one; historical key reads resolve through cell time-travel.
- §10 rollback table, `delete` row: already correct; add "(the binding tombstone among the restored
  cells)".
- §10 *Tombstones and the deleted-record set*: one sentence that `delete` tombstones the binding
  cell as well as adding the ID to the deleted set.
- `CHANGES.md` D08 → `done`; D07 scope notes the overlay consequence.

### Outcome

_(architect fills in: A / B / C / variant, date, one line of reasoning if it departs from the
recommendation)_

- **2026-10-10: A, as recommended.** `delete` removes the `#recordKeys` binding; re-creating the
  key is an ordinary `create` with a fresh `recordID`. Implemented on `feature/golem-db-api`, and
  stated in the API spec, metering's Record Model and design §4 (*Historised re-creation*), which
  now agrees with §10.

---

## 2 · D07 — Branch transitions: delete visibility, exact rollback, net diff, cancelled changes

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D07, P1, track L.
**Bears on:** §10 *The In-Memory Overlay*, *Tombstones and the deleted-record set*, *The Change-Set
Log and Rollback*, *Committing a Branch* step 4; S10 (per-operation table). Assumes D08 = A (delete
removes the `#recordKeys` binding); if D08 goes another way only 7.1's binding sentence changes.

### Question

§10 claims exact restoration on `rollback()` and says the overlay "already *is* the net diff" at
seal (step 4). Four gaps stop those claims from being checkable as written (Astra D05):

| # | Gap | Where |
| --- | --- | --- |
| 7.1 | **Delete visibility.** The read algorithm checks the overlay *before* the deleted-record set. A cell patched in-branch and then deleted with its record still has a real overlay value, so step 1 returns it and step 2 is never reached. | §10 read algorithm, steps 1–2 |
| 7.2 | **Pre-image is two-valued, restoration needs three.** `oldCells` records a value or "absent". But "absent" conflates *the overlay had no entry* (fall through to committed) with *the overlay held a tombstone* (deleted in-branch). Rule 3 says the undo of an added cell "removes it" — which is wrong if committed state has a value for that cell and an earlier surviving frame had tombstoned it. | §10 `OperationLog`, rule 3 |
| 7.3 | **Overlay ≠ net diff.** A write-back-to-original (`50 → 60 → 50`) or a restored entry after rollback leaves the overlay holding a value equal to the committed base. Step 4 would emit a `CellChangeSet` row and `CellHistory` entry for a cell that did not change. Step 4 also takes pre-images from "the oldest log entry", which does not exist for a cell whose only log entries were popped by rollback. | §10 *Committing a Branch* step 4 |
| 7.4 | **Cancelled and self-cancelling changes.** Nothing states what a rolled-back operation, or a create-then-delete inside one branch, leaves in committed history, the allocator, the index and `#recordKeys`. | §10, §4 `#alloc` |

A fifth observation feeds 7.1: the text motivates the deleted-record set by saying a `delete` "has no
idea how many cells the record has in committed state", yet log rule 2 says a `delete` "logs the whole
record" — and it must, because every removed cell needs a `CellChangeSet` pre-image at commit or a
historical read at T < delete would find no commit > T and return the live (absent) value. So the
delete already enumerates the record; the set does not save that work.

### Alternatives and recommendation, per sub-question

#### 7.1 Delete visibility

| | A — drop the deleted-record set; `delete` tombstones every cell | B — keep the set; check it before the overlay | C — keep the set; `delete` also removes the record's overlay entries |
| --- | --- | --- | --- |
| Read algorithm | overlay → committed (two steps) | set → overlay → committed | unchanged, now correct |
| `delete` writes | one tombstone per cell of R (committed ∪ overlay), plus the `#recordKeys ‖ K` tombstone (D08-A) | set entry + binding tombstone | set entry + removal of overlay entries under R + binding tombstone |
| Rollback of `delete` | restore `oldCells` (already logged) | remove from set | remove from set + restore overlay entries from `oldCells` |
| Net diff at seal | tombstones **are** the change-set rows; nothing to derive | must expand the set into per-cell rows from `oldCells` | same as B |
| Memory | O(cells of deleted records) in overlay — but the log already holds `oldCells` for the same cells | O(deleted records) + log | as B |
| Mechanisms | one (tombstones) | two | two |

**Recommendation: A.** The set was justified by an enumeration cost the log already pays. With it
gone there is one deletion mechanism, the read algorithm is two steps, the net diff needs no
expansion step, and D08-A's binding tombstone is just one more cell. Memory is not worse in order:
the tombstone is a tag byte beside a value the log already holds. **Steelman for B:** the set is
O(records) and a read is one hash-set probe cheaper on the miss path when nothing is deleted; but
reads already probe the overlay, and the set's probe is the same operation on a smaller structure —
a saving that does not survive contact with the log's O(cells) footprint.

#### 7.2 Exact rollback: three-valued pre-image

| | A — `PreImage = Untouched \| Tombstone \| Value(tagged)` | B — keep two states; treat "absent" as tombstone; filter at seal |
| --- | --- | --- |
| Rollback | exact: the overlay afterwards holds entries only for cells surviving operations touched | overlay accumulates tombstones over cells that were never in committed state |
| Reads after rollback | correct | correct (tombstone over nothing reads as absent) |
| Net diff | 7.3's comparison still needed for write-back-to-original, but restored entries are already gone | 7.3's comparison must also remove junk tombstones |
| Log cost | +1 discriminator byte per logged cell | none |

**Recommendation: A.** It makes "state is genuinely restored, not compensated" (Figure 10, point 4)
literally true and lets step 4 read the overlay without a cleanup pass for restored entries. The
byte is negligible against the value the entry already carries. `create` logs every cell as
`Untouched` (its record has no committed cells and no prior overlay entries); rollback of `create`
also returns the `recordID` to the branch allocator, which is safe because newest-first replay makes
allocation LIFO.

#### 7.3 Net diff: what enters the commit

| | A — every overlay entry, including those equal to the origin | B — overlay entries whose value ≠ value at origin |
| --- | --- | --- |
| Write-back-to-original | change-set row (pre-image = value), history entry, no trie rewrite (leaf hash unchanged) | nothing |
| Semantics of `CellHistory` | "commits that *touched* the cell" | "commits that *changed* the cell" — what §7 says it is |
| Cost | one comparison saved; one row + one history entry spent per no-op | one comparison per overlay entry, no reads |
| Determinism | yes | yes |

**Recommendation: B.** §7 defines history as the commits at which an item was *modified*, and
time-travel would behave identically either way — so A pays storage for rows that say nothing. The
comparison is free: with 7.2-A the oldest surviving log entry for every overlay cell holds its
origin value (rule 1 makes the oldest entry's pre-image the pre-frame state, and for the first touch
in the branch that is the committed value). The same rule applies to index terms: a posting list
that returns to its origin root yields no `IndexChangeSet` row. Step 4's sentence becomes: *the net
diff is the set of overlay entries whose value differs from the origin value held by their oldest
surviving log entry.*

#### 7.4 Cancelled and self-cancelling changes

Not an alternative but a set of statements that follow from 7.1–7.3 and should be written down:

- A rolled-back operation leaves **nothing** in committed history, index, bindings or roots; only
  the receipt it already returned survives, and receipts are not state.
- A record created and deleted in the same branch leaves exactly one trace: the **allocator
  advance**. Its `recordID` was minted and is never reused (`delete` does not return IDs; only the
  rollback of a `create` does). Its cells and binding compare equal to origin (absent → absent) and
  do not enter the net diff; its index terms were added and removed and their posting lists are at
  origin.
- A `delete` immediately rolled back leaves nothing; a `delete` that survives writes one
  `CellChangeSet` row per cell of the record plus one for the binding, all in the same commit.

### What would change the recommendations

- 7.1: a measured overlay-memory budget so tight that O(cells-of-deleted-records) tag bytes matter
  while the log's O(cells) values do not — implausible, but it is the only lever.
- 7.3: a downstream consumer that needs "touched but unchanged" as a signal (audit of write
  intent). None is known; the receipt already records that the operation ran.

### Fixtures that close the decision

Inspect after each step; then `seal`, `commit`, reopen, and check committed `Cell`, `#recordKeys`,
`#alloc`, `Index` and the roots against the expected surviving state.

```
T1  patch → delete → get         committed B{x:1}.  patch B.x=2; delete B; get B.x → absent (7.1)
                                 seal: CellChangeSet(c, B‖x)=1, (c, B‖#key)=K_B, (c, #recordKeys‖K_B)=R_B

T2  delete → rollback            committed B{x:1}.  cp; delete B; rollback; get B.x → 1
                                 overlay holds no entry for B (7.2-A); net diff empty

T3  write-back-to-original       committed B{x:1}.  patch B.x=2; patch B.x=1
                                 net diff empty; CellHistory(B‖x) unchanged (7.3-B)

T4  tombstone survives rollback  committed B{x:1}.  patch B.x=∅ (tombstone); cp; patch B.x=3; rollback
                                 get B.x → absent (7.2-A restores Tombstone, not Untouched)
                                 seal: CellChangeSet(c, B‖x)=1; live B.x absent

T5  create → delete same branch  create A (id 64); delete A
                                 net diff: only #alloc.#nextRecordID = 65; no cells, no binding, no terms

T6  create → rollback            cp; create A (id 64); rollback
                                 #alloc.#nextRecordID back to 64; next create mints 64

T7  failed multi-cell op         patch B{x:2, y:2} with budget for one cell → OutOfBudget
                                 inspect: B.x=1, B.y absent (call-level atomicity; F08); log has no entry
```

### Text to change on acceptance

- §10 *Tombstones and the deleted-record set* → retitle *Tombstones*; remove the set; state that
  `delete` writes a tombstone for every cell of the record (committed and overlay) and for the
  record's `#recordKeys` binding; two-step read algorithm; full-record read = overlay non-tombstones
  ∪ committed cells not covered by an overlay entry.
- §10 `OperationLog`: `oldCells: Map<CellKey, PreImage>`, `PreImage = Untouched | Tombstone |
  Value(TaggedValue)`; rule 3 rewritten around the three states; rollback table rewritten (no set).
- §10 *Committing a Branch* step 4: net diff = overlay entries whose value ≠ origin value from the
  oldest surviving log entry; same rule for index terms.
- §10, new short subsection or callout *What a cancelled change leaves behind* (7.4).
- §4 `#alloc`: one sentence — IDs consumed by a surviving create-then-delete are never reused;
  rollback of a `create` returns the ID (LIFO).
- `CHANGES.md` D07 → `done`; S10 builds its table from this.

### Outcome

_(architect fills in per sub-question 7.1–7.4)_

- **7.1 (2026-10-10):** A, as implemented on `feature/golem-db-api`: `delete` tombstones every
  live cell of the record and its binding; there is no deleted-record set. The API spec describes
  it. Design §10 still describes the set and needs updating.
- **7.3:** the implementation adds no undo entry for an identical write and derives index
  postings at seal from actual before/after values, which matches the recommendation; to verify
  against the fixtures. 7.2 and 7.4 to verify.

---

## 3 · D01 — Filter evaluation

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D01, P1, track Q. Produces S07.
**Bears on:** §5 (new closing subsection), §12 opening sentence, §13 costs, API *Filtering*, Epic 8
(implementation plan issue 58), D04 (one snapshot per query), D10 (metering shape), D09 (caps'
encoding if they become `#params` cells).

### Question

§5 defines a single predicate — equality, range, prefix — and its `Index` seek. §12 opens with
"filter evaluation ends with a Roaring bitmap of `recordID`s". Nothing in the design says how the
bitmap is produced from more than one predicate. The API does fix the *caller-visible* shape:

> `filter : ordered DNF — OR of AND-groups, negated literals allowed` · AND-order intersects in
> submitted order with early stop on an empty intermediate · OR-order unions in submitted order ·
> predicates are typed · `LimitExceeded` caps group count, predicates per group and nesting · no
> statistics-based planning.

So D01 is not "invent a query algebra"; it is "specify the mechanism that realises the API's DNF
over the two-tier index, and settle four points the API leaves to the design":

| # | Open point | Why the API cannot settle it |
| --- | --- | --- |
| 1.1 | **Negation universe.** `NOT price = 5` is a complement — of what set? | Depends on what structures exist to enumerate "all records" |
| 1.2 | **Match-all.** A filter with no positive literal (empty filter, or a group of only negations): allowed, and if so how is "every live record" produced? | Same dependency |
| 1.3 | **Where the caps live** and what "nesting" means for a flat DNF | Chain parameter vs. code constant is a §4 question |
| 1.4 | **Canonical evaluation and its cost shape.** What work is the metered work, in particular whether the second tier is descended fully or only into regions the running intermediate still occupies | Cost is a design property (invariant 4: estimation is simulation) |

Plus statements that follow from §5/§6 but are not written: how a range or prefix predicate becomes
a bitmap, the typed-predicate rule at the bitmap level, and the read snapshot.

### The mechanism (common to all options)

```
evaluate(filter) at snapshot S:
  result = ∅
  for group in filter.groups (submitted order):
      positives, negatives = partition(group)             // by literal sign
      inter = posting(positives[0])
      for p in positives[1..]:                            // AND-order
          inter = inter ∧ posting(p, restrictTo = regions(inter))
          if inter = ∅: break                             // early stop, per API
      for n in negatives:                                 // after all positives
          inter = inter ∧ ¬posting(n, restrictTo = regions(inter))   // ANDNOT
      result = result ∨ inter                             // OR-order
  return result                                           // Roaring over recordIDs

posting(p):
  equality       one Index seek → bitmapHash → BitmapTrie walk → containers
  range / prefix  Index cursor over [lo, hi) in key order → ∪ of each term's posting list
```

- **Typed predicates at the bitmap level.** A literal names `(cellKey, typeTag, value…)`; its `Index`
  key range lies entirely inside one type's run ([§5](golem-db-design.md#structural-index-key-formulation)), so a cell
  of another type under the same name is simply not in any posting list the literal touches — the
  API's "treated as absent" falls out of the key layout.
- **Range and prefix are unions.** The cursor visits every term in the key range in ascending order;
  cost is proportional to the number of distinct values in range, not to the number of matching
  records — the same shape §12 rejects for index-ordered emission, now bounded by budget and
  deterministic in abort point because the visit order is the key order.
- **One read snapshot** for the whole evaluation and the record fetch that follows (D04 will state
  the MDBX form).
- Bitmap operations are `Roaring32` per container region; ∧, ∨, ∧¬ are the library's, and the
  result is canonicalised per the D09 serialization rule before anything derives from it.

### 1.1 Negation universe

| | A — negation only inside a group with ≥ 1 positive literal; applied as ANDNOT to the running intermediate | B — universe = all live records, from a maintained "live set" posting list | C — universe = records holding *any* value of the literal's `(name, type)` |
| --- | --- | --- | --- |
| Needs | nothing new | a system index term every `create` adds to and every `delete` removes from — the live-set bitmap | a union over all terms of the name: cost ∝ distinct values |
| Semantics of `a=1 AND NOT b=2` | records with `a=1` whose `b` is not the `i32` 2 — includes records without `b` and records whose `b` is a `str` | same | records with `a=1` that *have* an `i32` `b` ≠ 2 |
| Cost | bounded by the intermediate's regions | one more container touch per create and delete, forever, for every deployment | unbounded in V |
| Group of only negations | `InvalidQuery` | evaluates against the live set | evaluates against C's union |

**Recommendation: A.** Set-difference from the intermediate is what the API's ordered-AND already
implies ("negated literals allowed" in an AND-group), costs nothing new, and gives the semantics a
caller expects from `NOT` over sparse cells. B is a real capability — see 1.2 — but should be
adopted for its own reasons, not as a by-product of negation. C changes the meaning of `NOT` to
"has-a-different-value", which callers can express directly with a range if they want it.

### 1.2 Match-all

| | A — not supported: a filter must contain ≥ 1 group with ≥ 1 positive literal; otherwise `InvalidQuery` | B — supported via a live-set index term maintained by the engine | C — supported by scanning `Cell` for `#key` cells |
| --- | --- | --- | --- |
| New mechanism | none | one system index term `#live` (or equivalent), a `BitmapTrie` over all live IDs; updated on create/delete | none |
| Steady cost | none | +1 container write per create and per delete (the container for that ID's 48-bit region) — roughly +1/(n+1) on a record of n cells, same order as D08-A | none |
| Query cost | — | one `BitmapTrie` walk: O(containers) = O(live IDs / 65 536) | O(all cells in the store): `Cell` interleaves every record's cells, so finding the `#key` rows is a full scan |
| Also gives | — | a universe for 1.1-B; a free `count(*)`; a natural "all records" export | — |
| Arkiv need | Arkiv queries by owner or attribute; "list everything" is unbounded and not a product surface | as left | — |

**Recommendation: A for v1**, with B recorded as the upgrade path. A "list all" over a store built
for millions of records is a budget-exhausting query by construction, and Arkiv does not need it.
The live-set term is cheap and would be the right way to add it — and 1.1-B with it — if a product
need appears (a P-row for the product owner if it does). C is rejected: it is the one query whose
cost is O(store) regardless of budget, so it can only ever return `OutOfBudget`.

> **Alignment with the live product (revises 1.1 and 1.2).** Arkiv's deployed DSL has `!=` and `!`
> as first-class operators — `status != "open"` is a valid query today — so "Arkiv does not need it"
> was wrong. That is the product need the paragraph above asked for; it is raised as **P08**. If the
> product owner confirms standalone negation must survive, the recommendation becomes **1.1-B and
> 1.2-B**: one engine-maintained live-set index term (`#live`, a `BitmapTrie` over all live IDs,
> +1 container write per create and per delete), giving a universe for `NOT`, `!=`, `EXISTS`, a
> match-all, and a free `count(*)`. The `[API]` change "require ≥ 1 positive literal" is then
> dropped. Glob `~` (P09) compiles to the §5 prefix scan for a literal with one trailing wildcard
> only; anything else is `InvalidQuery`.

### 1.3 Caps: where they live, and "nesting"

| | A — `#params` chain parameters: `#maxFilterGroups`, `#maxPredicatesPerGroup` | B — code constants versioned by the `Superblock` `format` row |
| --- | --- | --- |
| Consensus-visible | yes, and provable: a genesis mismatch is a root mismatch | yes, but only by "same code" |
| Discoverable by clients | ordinary `get` on record 0 | out of band |
| Consistent with | `#maxStrLen`, `#maxCellNameLen` — the same kind of limit | the physical ceilings §4 keeps in code |
| Cost | two cells at genesis | none |

**Recommendation: A.** These are validation limits of exactly the kind §4 already puts in `#params`
("policy within physics"), and a client building queries should be able to read them. Suggested
genesis values for Arkiv: 8 groups × 16 predicates — enough for any query a UI generates, small
enough that the worst-case term count is 128 equality seeks. **"Nesting"** in the API's
`LimitExceeded` text has no meaning for a flat DNF — OR of ANDs is depth 2 by construction. Either the
API's query structure admits nested groups (then the design needs a normalisation step and a depth
cap) or the word should go. Recommendation: **flat DNF only**, no nesting, strike the word from the
API (`[API]` change) — normalising arbitrary boolean trees to DNF is exponential in the worst case,
which is precisely the adversarial input invariant 6 warns of.

### 1.4 Canonical evaluation and cost shape

| | A — canonical work = full posting list per literal (every container of every term visited); region-pruning is an implementation optimisation whose saving goes to the operator | B — canonical work includes region-pruning: for the 2nd and later positives, and for negatives, only containers whose 48-bit region is present in the running intermediate are loaded |
| --- | --- | --- |
| Determinism | pure function of (state, filter) | pure function of (state, filter) — the intermediate is deterministic |
| Charged work | overstates: a selective first predicate does not reduce what later predicates cost | matches the work a correct implementation must do |
| AND-order as "the caller's main optimization lever" (API) | true only for the early-stop-on-empty case | true in general: a selective first predicate cuts every later predicate's cost |
| Consistency with §13's "cost is the canonical execution, warm or cold" | consistent | consistent — the rule fixes *which* execution is canonical; this defines it |
| Metering complexity | count terms and containers | count terms and containers; the container set is the pruned one |

**Recommendation: B.** The two-tier index exists so that work scales with containers touched
([§5](golem-db-design.md#why-the-posting-list-gets-a-second-tier)); charging for containers a correct evaluator never
needs to open would contradict that rationale and make the API's "main optimization lever" claim
false. The rule is one sentence: *canonical evaluation descends a term's `BitmapTrie` only into
regions the running intermediate occupies; the first positive literal of each group has no
intermediate and is descended fully.* The cost shape, for D10 to price:

```
per literal      1 Index seek  (+ k cursor steps for a range/prefix over k terms)
per term visited 1 BitmapTrie root read + interior-node reads along descended paths
per container    1 BitmapContainer read + bytes decoded
per group        Roaring ∧ / ∧¬ per container pair; per query, ∨ per group
```

Everything is a count of reads and bytes; nothing depends on statistics, wall-clock or node state.

### 1.5 Negation over absent and differently-typed cells

Added 2026-10-01. 1.1 answers *what negation subtracts from*; this answers *which records it keeps*.
Take `status="active" AND NOT amount=100`. The API makes a cell of another type "treated as absent"
(API *Filtering*) but never says whether a negated literal matches a record whose cell is absent.
Mainstream databases disagree on exactly this.

| | A — **MongoDB-style**: a negated literal matches every record not in the literal's posting list, including records without the cell and records whose cell has another type | B — **SQL-style**: a negated literal matches only records that *have* the cell, with the literal's type, and a different value (`<>` excludes NULL) |
| --- | --- | --- |
| `NOT amount=100` keeps a record with no `amount` | yes | no |
| … with `amount` stored as `str` | yes | no |
| Mechanism | the ANDNOT of the mechanism above, as is | ANDNOT, then ∧ "records having an `i32` `amount`": either the union of every `amount` term (1.1-C's cost, ∝ distinct values, unbounded) or a new per-name existence term maintained on every write |
| Cost | none beyond 1.1 | unbounded (union), or +1 container write per attribute write, for every name (existence terms) |
| Caller expressing the other meaning | "has `amount`, and it is not 100" needs an `EXISTS amount`, with B's cost problem per attribute | "absent or not 100" is `NOT amount=100 OR NOT EXISTS amount`, same problem |

**Recommendation: A.** It falls out of the posting-list mechanism for free, fixture Q3 below already
assumes it, and it agrees with the API's rule that another type is "treated as absent". B needs
either an unbounded union or an existence index per attribute name, a larger design change than the
whole of 1.1-B.

**Condition:** if Arkiv's live DSL treats `!=` SQL-style *and* P08 requires keeping the live
behaviour exactly, choose between a documented semantic change for Arkiv users (A) and paying for
per-name existence terms (B). The P08 answer therefore records which semantics the live DSL has.

### What would change the recommendations

- 1.1/1.2: a product requirement for "all records" or "records lacking an attribute" queries → adopt
  the live-set term (B) for both at once.
- 1.5: the live DSL being SQL-style and P08 requiring exact compatibility (see 1.5's condition).
- 1.3: a decision that `#params` is to stay minimal and every limit beyond string/name caps is code
  → B; the cost is client discoverability.
- 1.4: a metering model that prices only `Index` seeks and treats the second tier as free — then A
  and B charge the same and the distinction is moot. D10 should say which.

### Fixtures that close the decision

Dataset: records 100–105 with `status:str`, `amount:i32`, some lacking `amount`, one with
`amount:str`.

```
Q1  status="active" AND amount>=100                         AND-order, range as union, region pruning: count containers loaded
Q2  amount>=100 AND status="active"                         same result set as Q1, different receipt (order-dependent cost)
Q3  status="active" AND NOT amount=100                      includes records without amount and the amount:str record (1.1-A, 1.5-A)
Q4  NOT amount=100                                          InvalidQuery (1.1-A, 1.2-A)
Q5  {}                                                      InvalidQuery (1.2-A)
Q6  (status="active") OR (status="closed" AND amount<70)    OR-order; a record matching both groups appears once
Q7  9 groups                                                LimitExceeded with #maxFilterGroups = 8
Q8  status="active" AND status="closed"                     empty after 2nd literal; early stop; no further work charged
Q9  amount=100 with amount stored as str in one record      that record absent (typed predicate)
```

Each fixture records the result set, `total_matched`, and the canonical read counts per the 1.4-B
shape.

### Text to change on acceptance

- §5: new closing subsection *Combining Predicates: Filter Evaluation* — the mechanism block above,
  the four rules, the cost shape; anchor referenced from §12's first sentence and from the §5
  Notation callout (replacing "specified in `CHANGES.md` D01").
- §4 `#params` table: `#maxFilterGroups`, `#maxPredicatesPerGroup` (`u32` BE), with the genesis
  values as deployment choices.
- API *Filtering*: `[API]` — strike "nesting" from `LimitExceeded`; state that a filter needs ≥ 1
  positive literal per group and ≥ 1 group; cross-reference the design section.
- `CHANGES.md`: D01 → `done`; S07 → `done`; D10 gains the cost-shape input; D09 gains the two new
  `#params` codecs.

### Outcome

_(architect fills in per sub-question 1.1–1.5)_

---

## 4–6 · D04 · D03 · D02 — Concurrency contract, crash recovery, `rewind`

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D04, D03, D02 — all P1, track
L. Produces the material for S08 (recovery and failure document) and the host box (S09).
**Bears on:** §10 *Branches over the Head*, *Committing a Branch*; §11 *Writing Against a Sealed
Commit*, *Operations*; §13 cache-validity rule; API *Commits*, *Branches*, `rewind`, error table.
**Depends on:** D13 for the MDBX sync mode (assumed here: sync-on-commit); P05 for whether the
segment halves apply. Written as one brief because the linearization point chosen in D04 is what
makes D03's crash windows enumerable and D02's ordering rule follow.

### Question

The document has the ingredients — a head guard, MDBX's single writer, fsync-segments-before-MDBX,
"MDBX first, then truncate" for rewind — but not the contract (Astra D06, Fable A4):

| # | Gap | Where |
| --- | --- | --- |
| 4.1 | A branch read passes the guard, then its fall-through to committed state overlaps another branch's commit. Which state does it read? | §10 read algorithm step 3 "committed `Cell` at the origin commit" — how is that guaranteed? |
| 4.2 | Two sealed candidates over the same head both pass step 1's guard. Where is the race decided, and what has each written by then? | §10 step 8 re-checks the guard *after* step 7 has written rows |
| 4.3 | With segments: both candidates may append and fsync rows before either reaches the MDBX transaction; the loser leaves durable rows, contrary to "a losing branch leaves no trace in the segment file" | §11 *Appends are staged* |
| 3.1 | Restart procedure: what is read, what is truncated, what is verified | §11 gives the segment half; nothing gives the MDBX half or says what is *not* checked |
| 3.2 | Failure during commit: segment fsync fails, MDBX write fails, disk full. Can another commit proceed? Is retry safe? What about receipts already returned? | absent |
| 2.1 | Does `rewind(to)` exist? The API lists it; §10 says the lineage "never forks"; §11 uses it four times; no chapter defines it | absent |
| 2.2 | What does it undo — cells, index, tries, history rows, `#roots`, `#alloc`, segments — and in what order? | absent |
| 2.3 | After `rewind(99)` and a new commit 100, everything that named "commit 100" — branch handles, cursors, warm caches, `at = 100` — names a different state under the same number | §13 cache rule "commit it was built at equals the commit the request resolves to" |

### D04 — Concurrency contract

#### 4.1 Read isolation for branch and query reads

| | A — one MDBX read transaction **per operation**, with the head row checked inside it | B — one MDBX read transaction **per branch**, opened at `begin()` and held until the branch ends |
| --- | --- | --- |
| How "reads see the origin" is guaranteed | The `Superblock` head row is in the same snapshot as the cells. Each op opens a read txn, reads `head`; if `head.commitNr ≠ handle.commitNr` → `HandleInvalid`; else every read in that txn is of the origin state by MVCC | The snapshot *is* the origin state; no check needed |
| Mixed-state risk | none: check and reads share one snapshot | none |
| MDBX cost | a read-txn slot per op (lock-free in MDBX; microseconds) | one slot per branch |
| Page reclamation | pinned only for the op's duration | pinned for the branch's whole life: a long-lived branch stalls freelist reuse and grows the map — the "one operational caveat" §4 already names |
| Queries | one read txn per `query` across index evaluation and record fetch (Epic 8's "one snapshot per query"); the commit reported is the head read in that snapshot | same |

**Recommendation: A.** Same guarantee, no long-lived readers, and the head check costs one row read
in a snapshot the op opens anyway. B's simplicity is real but its failure mode — a forgotten branch
pinning pages for hours — is silent and operational. What would change it: MDBX read-txn open/close
showing up in profiles at Arkiv's per-block op counts (thousands, not millions) — unlikely.

#### 4.2 Commit linearization

| | A — the MDBX write transaction is the critical section; guard checked as its first action | B — an engine-level **commit mutex**, taken before any durable side effect; the MDBX write transaction is opened inside it |
| --- | --- | --- |
| Where the race is decided | acquisition of the MDBX writer | acquisition of the mutex |
| Loser's durable footprint | none in MDBX (txn aborts at the guard) — but anything done *before* the txn (segment appends) is already durable | none anywhere: the loser never reaches the append step |
| Write-lock hold time | includes nothing but MDBX writes | mutex covers segment fsync + MDBX txn; the MDBX writer itself is held only for the txn |
| Without segments | A ≡ B | |

**Recommendation: B, with A as its degenerate form when segments are absent.** The contract, in
one sentence: *`commit(b)` acquires the commit mutex; under it, in order: guard (`head ==
b.commitNr`, else `Conflict`), segment append + fsync, MDBX write transaction (rows of steps 3–6,
head advance), release.* The linearization point is the MDBX transaction commit; the *decision*
point is the guard under the mutex, and nothing durable happens between the two on the loser's
side because the loser never passes the guard. Step 8's "re-checked here" moves to the top of the
critical section and becomes the only check that matters; step 1's check is an early, advisory copy.

#### 4.3 Losing candidate and segments

Follows from 4.2-B: the loser leaves no durable row because it never appends. The staged-append
guarantee (§11) becomes a consequence of the commit mutex rather than a separate promise. If P05
drops segments, 4.3 is void.

### D03 — Crash recovery and failure during commit

#### 3.1 Restart procedure

```
1. open MDBX; read Superblock: format, hash_fn, roaring, head = (N, SR_N, IR_N)
   - format / hash_fn / roaring not implemented by this code  →  refuse to open ("upgrade required")
   - head missing                                              →  fresh store: run genesis, or refuse if genesis file absent
2. MDBX guarantees: every row of commits 0..N is present, no row of N+1 is  (single-txn commit)
3. segments (if P05):  truncate system segment to N+1 rows; read row N; truncate each segment to mark(N)[seg]
4. branches: none survive (volatile); the host re-opens and re-executes anything it had in flight
5. optionally, on operator request: recompute SR_N and IR_N from Cell and Index and compare to head
```

Step 5 is not routine: it is O(state). It is offered because MDBX protects against torn writes, not
against a bug that wrote consistent-but-wrong rows; a recompute is the only check that catches that.

#### 3.2 Failure during `commit`, by point of failure

| Fails at | Durable state after | Head | Sealed branch `b` | Other branches over the same head | Retry `commit(b)` |
| --- | --- | --- | --- | --- | --- |
| guard | unchanged | unchanged | invalid (`Conflict`) | unchanged | no — re-open over new head |
| segment append / fsync (I/O) | possibly a partial tail beyond `mark(N)` | unchanged | still valid, still sealed | still valid | **yes** — after truncating the tail to `mark(N)`; nothing else changed |
| MDBX write txn (I/O, `MAP_FULL`, key/value size) | segments ahead of MDBX by `b`'s rows | unchanged | still valid | still valid | **yes** for I/O after truncating segments; **no** for size limits (deterministic: the same rows fail again) → `discard(b)` |
| MDBX txn commit, crash before fsync completes | MDBX has N; segments have N+1's rows | N | lost (volatile) | lost | n/a — restart truncates segments to `mark(N)` |
| after MDBX commit | complete | N+1 | consumed | invalid by guard | n/a |

Two classes of failure, which the API's error set should keep apart:

- **Deterministic** (`Conflict`, `LimitExceeded`, MDBX key/value size at commit): every node
  running the same code over the same state gets the same verdict; the host treats it as a rejected
  block.
- **Environmental** (`Io`, `MapFull`): node-local. The engine's promise is *nothing durable changed
  and the head is unchanged*; the host may retry the same sealed branch. A new error class, `Io`, is
  needed in the API's table (`[API]`).

**Receipts.** A receipt is a return value, never state (§10). Operations' receipts issued before a
failed commit describe work that was done and paid for in the branch; the host's re-execution over
the new head yields byte-identical receipts if the state is identical (determinism), and different
ones if it is not — which is correct, because the operations then did different work.

**Durability assumption (for D13):** MDBX is opened in a sync-on-commit mode (not `NOSYNC` /
`SAFE_NOSYNC`), so "commit returned" means "head is durable". Segment `fsync` precedes the MDBX
transaction so that MDBX is never ahead of segments (§11's argument, now with 4.2-B guaranteeing
the loser never fsyncs).

### D02 — `rewind(to)`

#### 2.1 Existence and what it means for "never forks"

**Recommendation (revised, see the alignment note below): semantics specified now; the feature is
deployment-optional and not in v1.** The original recommendation was "exists, host-restricted, as
the API says". Either way, reconcile with §10: the lineage is
linear *at every moment*; `rewind` shortens it, and commits then continue from `to+1`. Commit
numbers above `to` are **reassigned**. That is the property everything in 2.3 has to survive.

> **Alignment with the requirements.** Open question 4 of `golem-db.md` says: "remove, or make
> deployment-optional with those semantics specified … leaning towards having this feature at a
> later stage", and names two conditions — every history row of the undone commits is deleted, and
> as-of reads stay unambiguous across nodes — which are 2.2 and 2.3 below. Revised recommendation:
> **the semantics are specified now (this brief); the feature is deployment-optional and not in the
> v1 build.** §10 states both. Under CometBFT nothing rewinds; the Engine-API host keeps the option.
> §11's recovery keeps only the crash-truncation half in v1.

#### 2.2 What `rewind(to)` undoes, and how

Everything a commit writes is either an ordinary cell (`#roots`, `#alloc`, `#recordKeys` bindings,
user cells), an index term, or a derived structure. So undoing a commit is **change-set replay**, the
same mechanism §7 uses to read the past:

```
rewind(to), with head = N > to:
  roots_to = #roots cell (to)                      // written by commit to+1; read it BEFORE undoing that commit
  for c = N down to to+1, each in its own MDBX write transaction, under the commit mutex:
      for every CellChangeSet(c, …) row:   restore the pre-image into Cell (or delete the cell if ∅)
      for every IndexChangeSet(c, …) row:  restore the pre-image bitmapHash into Index (or delete the term if ∅)
      remove c from every CellHistory / IndexHistory entry it appears in
      delete the CellChangeSet(c, …) and IndexChangeSet(c, …) rows
      head ← (c−1, roots from #roots cell (c−1), or roots_to when c−1 = to)
  segments (if P05): truncate the system segment to to+1 rows; truncate each segment to mark(to)[seg]
```

- **`#roots`, `#alloc`, `#recordKeys` need no special handling**: their cells for the undone commits
  have change-set pre-images like any cell, so the replay removes the `#roots` cells commits
  `to+1 … N` wrote and **rewinds the allocator** — which is required, not incidental: a node that
  rewinds must mint the same IDs afterwards as a node that never saw the abandoned commits.
- **Trie nodes** of the abandoned commits become orphans (unreachable from the new head) and go to
  the same GC as any superseded node. Nothing is deleted eagerly.
- **History of the abandoned commits is gone.** A reorg means they did not happen; keeping them
  would make `commitNr` ambiguous in the history tables. Hosts that want to inspect abandoned
  blocks keep them outside the engine.
- **One transaction per undone commit**, not one for the whole rewind, so the transaction size is
  bounded by one commit's change-set and a crash mid-rewind leaves the head at some `c ≥ to` with a
  consistent state; the host re-issues `rewind(to)`. The alternative — one transaction for all — is
  atomic but unbounded and would need chunking anyway.
- **Order: MDBX first, then segments**, as §11 says: in the window between them segments are ahead,
  which recovery truncates; the reverse would make MDBX ahead, which nothing repairs.
- **Verification**: the head's roots after rewind are taken from `#roots`, not recomputed. An
  operator-requested recompute (as in 3.1 step 5) is the check.
- **Cost**: O(Σ change-set rows of the undone commits) — proportional to what was written, bounded by
  the retention window, since `rewind` past pruned history is impossible (`Pruned`).

#### 2.3 Identity after rewind

The problem: after `rewind(99)` and a new commit, "100" names a different state. Anything holding a
`commitNr` from before the rewind — a warm node's cached sequence, a client's cursor, a pinned
`at = 100`, a branch handle `(100, n)` — is silently wrong under the §13 rule.

| | A — root-based identity: every external reference to a commit carries `(commitNr, GlobalRoot)`; the engine verifies against `#roots` / head | B — rewind generation: a `Superblock` counter incremented per rewind, carried in handles, cursors and cache keys | C — invalidate everything in-process at rewind; external references (cursors) keep `commitNr` only |
| --- | --- | --- | --- |
| Detects stale cursor from before rewind | yes: `#roots[100] ≠ carried root` → `Stale` | yes, on this node | **no** — the cursor resumes silently in the wrong sequence |
| Works across nodes (warm node on another machine, client moving between nodes) | yes: roots are consensus state | no: generation is node-local — two nodes with identical state but different rewind histories reject each other's cursors | no |
| Survives restart | yes | only if the counter is durable | n/a |
| Size | 32 B per reference | 8 B | 0 |
| API surface | cursor is opaque — no change; `at` gains an optional expected root (`[API]`, additive) | cursor opaque; `at` gains a generation — meaningless to a client | none |

**Recommendation: A.** The root is the only identity of a state that is the same on every node and
survives every rewind and restart; `commitNr` is a position, not an identity, once `rewind` exists.
Concretely:

- **Cursors** carry the `GlobalRoot` of the commit they were built at (pinned) or resolved at (live,
  last page); a resume checks it against `#roots`/head and fails with a new `Stale` error rather
  than silently continuing. The §13 cache rule becomes: *a held sequence may serve a request iff the
  root it was built at equals the root the request resolves to.*
- **Branch handles** need nothing new: after rewind the head is `to`, so every handle with origin
  `≠ to` fails the guard. A handle with origin `= to` opened *before* the abandoned commits is valid
  again — and correctly so, since the state is identical. `branchNr` is a process-global monotonic
  counter, never reset, so no handle is ever minted twice.
- **`at = 100`** with no root reads the new commit 100. That is the host's intent in a reorg; a
  client that wants the old one supplies the root and gets `Stale`. Additive `[API]` change.

### What would change the recommendations

- 4.1: profiling evidence that per-op read transactions cost more than the page-pinning risk of
  per-branch ones — unlikely at Arkiv's op counts.
- 4.2: P05 dropping segments makes the mutex redundant with the MDBX writer; keep the sentence, note
  the equivalence.
- 2.2: a product requirement to *retain* abandoned commits for inspection → they would need their
  own namespace outside the history tables; not a change to the undo mechanism.
- 2.3: a decision that cursors are never handed to clients (host-internal only) would let B suffice
  on a single node — but the warm-node pattern §13 describes is multi-node.

### Fixtures that close the decisions

```
C1  read overlaps commit         branch b1 over N; b2 over N; b2 commits N+1 while b1's get is in flight
                                 → b1's get returns origin-N value or HandleInvalid; never an N+1 value (4.1-A)
C2  two sealed candidates        b1, b2 sealed over N with different segment rows; commit both concurrently
                                 → exactly one N+1; loser gets Conflict; segment files hold only the winner's rows (4.2-B, 4.3)
R1  crash after segment fsync    kill between segment fsync and MDBX commit → restart: head N, segments truncated to mark(N) (3.1)
R2  MDBX write fails (inject)    → head N; segments truncated; commit(b) again succeeds → N+1 (3.2)
R3  size limit at commit         oversize value → deterministic error; discard(b); other branches over N still commit (3.2)
W1  rewind and recommit          commits 98,99,100 (100 creates id 200); rewind(99); commit 100' (creates id 200 again)
                                 → #alloc rewound; CellHistory has no trace of old 100; #roots[99] present, [100] = new (2.2)
W2  stale cursor                 cursor built at old 100 (root A); resume after 100' (root B) → Stale (2.3-A)
W3  crash mid-rewind             rewind(97) from 100, crash after undoing 100 and 99 → restart head 98; rewind(97) again → 97 (2.2)
W4  rewind past retention        rewind(to) with to's change-sets pruned → Pruned, head unchanged (2.2)
```

### Text to change on acceptance

- §10 *Branches over the Head*: the read-isolation rule (4.1) — one paragraph.
- §10 *Committing a Branch*: the commit critical section (4.2) replaces the step-8 re-check
  sentence; step 7 notes the segment fsync precedes it under the mutex.
- §10, new subsection *Recovery and Failure* (3.1, 3.2 table, error classes) — or the S08 document
  in Phase 3.
- §10, new subsection *Rewind* (2.1–2.3); §11 *Operations* keeps its ordering sentence and points
  here; §13 cache rule reworded to roots; cursor field list gains `root`.
- API `[API]`: `Io` and `Stale` errors; `at` gains optional expected root; `rewind` gets a
  definition paragraph.
- `CHANGES.md`: D02, D03, D04 → `done`; D13 gains the sync-mode assumption; D11 notes the cursor
  root field; S08/S09 have their inputs.

### Outcome

_(architect fills in per sub-question 4.1, 4.2, 3.1, 3.2, 2.1–2.3)_

- **4.1 (2026-10-10), partly:** implemented as recommended for reads: every branch operation
  validates its handle against the head in the same store snapshot it reads from, and `commit`
  rechecks the head inside the store writer before writing. Both are in the API spec
  (*Branches*). The segment-fsync ordering of 4.2 waits for segment storage.
- **3.x (2026-10-10), partly:** the API spec distinguishes an environmental error, `StoreFull`
  (one node's store is full; never part of a result other nodes see), from deterministic ones.
  The `Io` error and the restart procedure are still open.
- **2.1 (2026-09-30):** `rewind` is **not in v1**, as revised above. The API marks it so and the
  design's D02 row says so. 2.2 and 2.3 are still to decide.

---

## 7 · D05 — Retention and historical discovery

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D05, P1, track H.
**Bears on:** §1 property 5, §7 retention note (F18), §9 GC, §11 pruning and shard-start marks
(F03), §13 pinned cursors, API `at` / `count` "must lie within retention", `Pruned`.
**Depends on:** P06 (the product statement on "full history"); D02 (rewind reads change-sets).

### Question

The document names pruning, garbage collection and a retention window in five chapters and defines
none of them (Fable A4/Hermes; Astra D07). Five things have to be fixed:

| # | Open point |
| --- | --- |
| 5.1 | **Service matrix.** For each read class, which structures must survive for the read to be served at commit T |
| 5.2 | **The window.** How "earliest supported commit" is defined, where it is configured, and how a caller discovers it |
| 5.3 | **GC roots.** §9 calls superseded trie nodes "orphaned — unreachable from the current root" and says GC reclaims them. F18 established that supported historical reads still need them. What is the actual collectability rule? |
| 5.4 | **`Pruned` vs `NotFound`, and "never silent empty".** When is each returned, and what guarantees a historical read is complete rather than quietly missing pruned rows? |
| 5.5 | **Discovery.** A historical range scan must find terms no longer in `Index`; a historical full-record read must find cell names deleted since T. §7's resolution rule assumes the item is already known. Where does the enumeration come from? |
| 5.6 | **Reader protection.** Does an in-flight operation, or a cursor between pages, hold anything against GC? |

### 5.1 Service matrix

Derived from the schema, not chosen; written down so 5.3 can be stated over it.

| Read class at commit T | Needs, beyond live state | Cost shape |
| --- | --- | --- |
| Point read `get(K, at=T)` | `CellHistory` + `CellChangeSet` for `#recordKeys‖K` and for each requested cell | 2 lookups per cell |
| Full-record read at T | as above **plus discovery** of cell names present at T (5.5) | prefix scan of `CellHistory` under `R` |
| Filtered query at T | `IndexHistory` + `IndexChangeSet` for each term; the `BitmapTrie` nodes and `BitmapContainer` rows reachable from each term's historical `bitmapHash`; **plus discovery** of terms present at T for range/prefix literals | per §5 D01 shape, over historical roots |
| Proof at T | `#roots[T]` (live cell); `CellTrie` / `IndexTrie` nodes from `SR_T` / `IR_T` down; for a virtual leaf, the leaf's tagged value at T (D06) | O(depth) reads |
| `rewind(to)` | `CellChangeSet` / `IndexChangeSet` rows for every commit in `(to, head]` | O(rows) |
| Segment read at T (P05) | the shard covering T, and the system-segment shard covering T (F03: `mark(S−1)` from the previous shard) | 1–2 reads |

### 5.2 The window

| | A — one window `W` (commits), node configuration | B — two windows: `W_hist` for history rows and bitmap nodes, `W_proof ≤ W_hist` for `CellTrie`/`IndexTrie` nodes | C — window as a `#params` chain parameter |
| --- | --- | --- | --- |
| Meaning | every structure in the matrix is retained for commits `≥ head − W`; `earliest = max(0, head − W)` | queries reach further back than O(depth) proofs; a proof in `(head−W_hist, head−W_proof]` is refused, not rebuilt | consensus-visible retention |
| Operator freedom | archival node: `W = ∞`; query-serving replica: large; validator: small | same, with the asymmetry Elena asked for | none — every node retains alike |
| Correctness risk | one number; one GC pass | two numbers; two GC roots; the proof/no-proof boundary must be reported | retention does not affect roots, so putting it in `#params` buys provability of a property nobody proves |

**Recommendation: A for v1**, expressed in commits, configured per node, with **`W = ∞` as the
archival profile**; B recorded as the refinement if operators find proofs' node retention the
dominant cost. C is rejected: retention is a node's service level, not a consensus fact — two nodes
with different windows agree on every root.

> **Alignment with the requirements (revises the recommendation).** CS-5 makes the *minimum*
> retention window an instance parameter and DI-7 says that beyond it "the consensus-path API
> refuses on every node, whatever a node happens to retain; longer retention is served by a separate
> archival surface". So the window has two layers, and the recommendation becomes **C for the
> minimum, A above it**: `#minRetention` (`u64`, commits) in `#params`; on the consensus-path API,
> `Pruned` iff `at < head − #minRetention`, identically on every node; a node may retain more (up to
> `∞`), and that surplus is readable only through an **archival surface** — a separate, unmetered,
> non-consensus read API the requirements list as "not yet designed" (`[API]`, additive). The GC
> rule in 5.3 uses the node's actual retention, which is `≥ #minRetention`. The rationale I gave for
> rejecting C ("retention does not affect roots") was correct but beside the point: the requirement
> wants the *refusal* to be uniform, not the retention.

**Discovery of the window:** an unmetered introspection call `retention() → { earliest: CommitId,
head: CommitId }` (`[API]`, additive). `at < earliest` on any call ⇒ `Pruned` before any lookup.

### 5.3 GC roots

**Recommendation — one sentence replacing §9's:** *A stored trie node, bitmap node or container is
collectable iff it is unreachable from every root in `#roots` for commits in `[earliest, head]` and
from the live head.* History and change-set rows for commit `c` are collectable iff `c < earliest`;
a history bitmap entry for such a `c` is removed from its `Roaring64`, and the row is deleted when
the set becomes empty. "Orphaned from the current root" is thereby demoted from the collectability
criterion to a description of what *becomes* collectable when the window advances. GC runs as an
ordinary write transaction under the commit mutex (D04), so it never races a commit and never
changes any root.

### 5.4 `Pruned`, `NotFound`, and completeness

- `Pruned` ⇔ `at < earliest`. Decided from the head and `W` alone, before any table is touched.
- `NotFound` ⇔ `at ≥ earliest` and the item does not exist at `at`.
- **Completeness follows from 5.2 + 5.3:** retention is by commit window over *all* structures, so a
  read at a supported commit finds every row it needs. There is no per-structure GC that could remove
  a bitmap node while its `IndexChangeSet` row survives — which is the only way a "silent empty"
  could arise. That invariant should be stated as such.

### 5.5 Discovery: scan the history table, not the live table

The history tables are keyed exactly like their live counterparts (`CellHistory` by
`recordID ‖ cellKey`; `IndexHistory` by the index term). So:

| Historical enumeration | Live path | Historical path |
| --- | --- | --- |
| cell names of record `R` at T | prefix scan of `Cell` under `R` | prefix scan of **`CellHistory`** under `R` → every cell `R` ever had within the window → resolve each at T (§7 rule) → keep those present |
| terms in `[lo, hi)` at T | cursor over `Index` | cursor over **`IndexHistory`** in the same key range → every term that ever existed in range within the window → resolve each at T → keep those present |

Deduplication is inherent (one history row per item). Cost is proportional to items that *ever*
existed in the range within the window, not to items present at T — a qualification §13's pinned
cost table should carry. A cell or term whose every commit has left the window has no history row
and is correctly absent at every supported T. No new structure is needed.

### 5.6 Reader protection

- **Within an operation:** D04's per-operation MDBX read transaction pins the pages GC would free;
  MVCC serves the reader its snapshot. Nothing more is needed.
- **Between pages of a cursor:** no lease. A pinned cursor at commit `C` resumes iff `C ≥ earliest`;
  otherwise `Pruned`. Leasing retention to cursors would let an unbounded number of clients pin an
  unbounded window — an adversarial-input problem (invariant 6). The rule is stated in D12.

### 5.7 History after snapshot sync (requirements open question 9)

A node that joins from a snapshot at commit `c` (NF-2) holds no history before `c`, yet DI-7
requires as-of reads inside the minimum window to succeed identically on every node.

| | A — the snapshot carries the window's history rows and historical trie/bitmap nodes | B — a freshly synced node is exempt: it answers `Pruned` for `at < c` until it has been live for one window, and is not used for consensus-path historical reads meanwhile | C — define the window per node from its sync point |
| --- | --- | --- | --- |
| DI-7 holds | yes | not for that node, for one window | no — weakens the guarantee for everyone |
| Snapshot size | + change-set and history rows for `#minRetention` commits + retained nodes: several times the live state | live state only | live state only |
| Complexity | export/import of four more tables and orphaned nodes | a per-node `earliest = max(head − #minRetention, c)` and a flag consensus can see | none |

**Recommendation: B**, made honest by exposing it: `retention()` reports the node's real
`earliest`, and a validator's consensus-path historical read is not a thing — DI-7's rationale
("proofs for light clients, RPC reads at a past block, audit") is entirely off the consensus path,
as the requirements' own open question 2 notes. So the exemption costs nothing consensus-critical.
A is the option if the product answer to open question 2 is "every node must serve the full window
from the moment it joins"; it is a snapshot-format decision (NF-2), not a schema change.

### What would change the recommendations

- 5.2: operator data showing `CellTrie`/`IndexTrie` node retention dominating disk at Arkiv's
  window → B.
- 5.5: a requirement for historical discovery *beyond* the history window (true archival
  enumeration) → would need `W = ∞` on that node; no mechanism change.

### Fixtures

```
H1  term deleted since T       commit 10: price=5 on R; commit 20: R.price ← 6 (term price=5 now empty, removed from Index)
                               query price in [5,5] at T=15 → [R]   (discovery via IndexHistory)
H2  cell deleted since T       commit 10: R{a,b}; commit 20: patch removes b
                               get R at 15 → {a,b}; at 25 → {a}     (discovery via CellHistory)
H3  window boundary            W=100, head=250 → earliest=150. get(K, at=149) → Pruned before any lookup; at=150 → served
H4  GC does not break a read   head=250, W=100; node X reachable only from root_140 → collectable; from root_160 → retained
                               get at 160 whose proof passes X → succeeds after GC
H5  cursor past the window     pinned cursor at 149 after head advances so earliest=150 → Pruned
H6  rewind across window       rewind(to) with to < earliest → Pruned, head unchanged
```

### Text to change on acceptance

- §7 retention note → replaced by a subsection *Retention* with the matrix, window, GC rule,
  `Pruned`/`NotFound`, and discovery.
- §9 "orphaned … GC reclaims them" → the 5.3 sentence.
- §1 property 5 → qualified per P06 ("every past state within the node's retention window;
  archival nodes retain all").
- §11 *Operations* `Pruned` bullet → points to §7; the F03 open sentence on `mark(S−1)` survival
  → answered: the system-segment shard is retained while any commit it covers is `≥ earliest`, and
  shard `S` is never retained without shard `S−1`'s final row (kept as a one-row boundary file or
  copied into shard `S`'s header — implementation choice).
- API: `retention()` introspection; `Pruned` definition tightened.
- `CHANGES.md`: D05 → `done`; D12 and D06 have their inputs.

### Outcome

_(architect fills in per sub-question 5.2, 5.3, 5.5, 5.6)_

- **5.2, minimum window (2026-10-01, with P06):** `#minRetention` (`u64`, commits) in `#params`; the
  consensus-path API refuses reads before `head − #minRetention` on every node. Landed in design §1
  property 5 and the §4 `#params` table. Still to decide: the archival surface above the minimum,
  `retention()`, and 5.3–5.7.
- **New dependency (2026-10-10):** metering calibrates its write byte weights and the history part
  of `w_cell[op]` for residency of `#minRetention` commits (metering D4, D2). The value chosen here
  therefore also sets prices.

---

## 8 · D06 — Proof scope and the non-inclusion witness

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D06, P1, track H.
**Bears on:** §8 *The Two Tries*, *Domain Separation*, *Bare-Leaf Roots and Virtual Leaves*,
`BranchNodeCompact` (§2); §1 property 3; API *Proofs*. **Depends on:** D05 (which historical nodes
exist); D09 (preimage bytes). **Changes a hash preimage** — pre-launch only.

### Question

§1 property 3 promises that "any cell or index term … can be proved against that root". §8 gives
inclusion proofs by top-down descent. Two things are missing (Astra D04):

- **8.1 Scope.** Which statements are provable, and — as an explicit non-goal — which are not.
- **8.2 The non-inclusion witness for a virtual leaf.** To prove `X` absent, descent may land on an
  occupied leaf slot whose stored `leaf_paths[j] = Y ≠ X`. `leaf_paths` is not hashed, so the
  verifier cannot trust it; it must recompute the leaf hash `Hash(0x00 ‖ Y ‖ typeTag ‖ value)` — which
  needs `Y`'s **tagged value**. The server holds `Y = Hash(recordID ‖ cellKey)`, which is not
  invertible, so it cannot find the `Cell` row that holds the value. The same holds for `IndexTrie`
  leaves (`bitmapHash` lives in `Index`, keyed by term, not by `trieKey`). `BitmapTrie` is fine: its
  leaves are materialised under their hash and carry `hi48`.

### 8.1 Scope

**Recommendation — provable, against a `GlobalRoot`:**

| Statement | Witness |
| --- | --- |
| cell `(R, c)` has tagged value `v` at T | `#roots[T]` proof against head (if T ≠ head), then `CellTrie` path from `SR_T`, then `v` |
| cell `(R, c)` is absent at T | path to the deciding node + the 8.2 witness |
| record with key `K` exists / does not exist at T | inclusion / non-inclusion of `#recordKeys ‖ K` (D08-A) |
| term `t` has posting-list root `h` at T | `IndexTrie` path + `h` |
| `recordID r` is / is not in term `t` at T | the above, then `BitmapTrie` path from `h` to the container for `r`'s region (or the empty slot), then the container bytes |
| the roots at T | inclusion of `#roots[T]` in the head's `CellTrie` |

**Explicit non-goals:** range completeness ("every term in `[lo, hi)` is listed") and query-result
completeness. `IndexTrie` is keyed by `Hash(term)`, so adjacency in the trie says nothing about
adjacency in value order; a completeness proof would need a value-ordered authenticated structure the
design does not have. §1 property 3's wording should say "any cell or index term" is provable and
add that result sets are not.

### 8.2 The witness for a mismatching virtual leaf

| | A — reverse map `trieKey → (recordID, cellKey)` as a new table | B — nested leaf preimage `Hash(0x00 ‖ trieKey ‖ Hash(typeTag ‖ value))` and store the inner hash beside `leaf_paths` | C — scan `Cell` for the row hashing to `Y` | D — non-inclusion proofs are a non-goal |
| --- | --- | --- | --- | --- |
| Witness for mismatch | `Y` + `Y`'s tagged value (fetched via the map) | `Y` + `vh = Hash(typeTag ‖ value)` (read from the parent node) | as A | — |
| Verifier work | recompute leaf hash from full value | recompute `Hash(0x00 ‖ Y ‖ vh)` | as A | — |
| Historical case (Y's cell deleted since T) | the map must itself be historised (change-set rows) — another table under §7 rules | the **historical parent node** already carries `vh`; nothing else needed | scan history — O(state) | — |
| Storage | +1 table: 32 B key + ~40 B value per live cell, plus history of it | +32 B per leaf child in every interior node, CoW-duplicated across retained versions | none | none |
| Write cost | +1 row per create / new cell / delete, + history | none: `vh` is computed for the leaf hash anyway | none | none |
| Inclusion proofs | unchanged | one extra hash: verifier computes `vh` then the leaf | unchanged | unchanged |
| Preimage change | no | **yes** (`0x00` and `0x02` leaves) | no | no |
| Reveals to verifier | full value of a *different* record's cell | only `vh` — no value leakage | full value | — |

**Recommendation: B.** It is the only option whose historical case is free — the node retained for
the proof at T already holds what the witness needs — and it leaks no unrelated value to the
verifier. The preimage change is acceptable now because nothing is deployed; it would be a state fork
later, which is why D06 is P1. For `IndexTrie` the inner hash is `bitmapHash` itself (already a
digest), so the leaf formula becomes `Hash(0x02 ‖ trieKey ‖ bitmapHash)` — unchanged — and the
stored companion is `bitmapHash`. `BranchNodeCompact` gains `leaf_value_hashes: Vec<B256>`
(stored, never hashed, same order as `leaf_paths`). **A** is the conventional answer and is rejected
because historising the map doubles its cost and adds a fourth history pair. **C** is O(state).
**D** is honest but gives up record non-existence proofs, which §4 promises and D08-A relies on.

**Steelman for A:** it also gives `trieKey → identity` for debugging and for the reth-style
leaf-hash stripping upgrade (§8 path 1). Both are served as well by an *uncommitted, node-local,
rebuildable* map that is not part of the contract — which any implementation may keep without the
design saying so.

### 8.3 The four traced non-inclusion cases (verifier's view)

```
missing slot        descend to node N whose state_mask bit for X's next nibble is 0
                    witness: path nodes to N.  verifier: recompute N's hash from its fields; bit is 0 → X absent
prefix mismatch     descend to node N whose prefix diverges from X within prefix_len
                    witness: path nodes to N.  verifier: N's prefix ≠ X's nibbles at that depth → X absent
mismatching leaf    descend to node N; slot j is a leaf; server supplies (Y, vh) from leaf_paths[j], leaf_value_hashes[j]
                    verifier: Hash(0x00 ‖ Y ‖ vh) = child_hashes[j] and Y ≠ X → X absent
singleton root      root is a bare leaf: witness (Y, vh); verifier: Hash(0x00 ‖ Y ‖ vh) = StateRoot and Y ≠ X
empty trie          StateRoot = EMPTY_ROOT (D09) → everything absent
historical          same four, descending from SR_T taken from #roots[T]; nodes retained per D05
```

### Cost

Proof size: depth × (2 + ⌈prefix_len/2⌉ + 4 + 32·popcnt(state_mask)) bytes + witness (≤ 64 B). At
a billion cells, depth ≈ 8, typical popcnt ≈ 16 → ~4.3 KB. Generation: depth reads. Storage for B:
+32 B per leaf child; a node with 16 children of which 14 are leaves grows from ~520 B (with
`leaf_paths`) to ~970 B — proportionally the same +32 B per leaf the existing `leaf_paths` already
costs.

### What would change the recommendation

A decision (P07) that a second engine must reproduce the root *without* adopting Golem DB's node
format would make `leaf_value_hashes` an engine-private optimisation — B still works because the
witness is `(Y, vh)`, however the server obtains `vh`; only the *cost* claim would change.

### Fixtures

The four traces above as vectors over the §8 worked example (records 100, 101), plus: prove
`(101, Status)` absent (mismatching-leaf case at slot `A`); prove `(200, Price)` absent (missing
slot); after deleting record 101 at commit 3, prove `(101, Price)` absent at head and present at
commit 2.

### Text to change on acceptance

- §8 *The Two Tries* leaf formula for `CellTrie`; domain table row `0x00`; §2 `BranchNodeCompact`
  gains `leaf_value_hashes`; §8 *Bare-Leaf Roots* gains *Proving absence* with the four cases.
- §1 property 3 wording; new sentence: result completeness is not proven.
- API *Proofs*: the provable-statement table; non-goal sentence. `[API]`.
- `CHANGES.md`: D06 → `done`; D09 gains the new preimage; S05 lists `leaf_value_hashes` as free
  (stored, not hashed).

### Outcome

_(architect fills in: 8.1 scope; 8.2 A/B/C/D)_

- **New input (2026-10-10):** range completeness stays a non-goal, but **whole-record
  completeness** now follows indirectly: `#meta` is committed state, so a client holding an
  inclusion proof of `#meta` can check that a full read withholds no user cell (the proven cells
  must match its `cells` count). Design §3 *Record Shape: the `#meta` Cell*; the design's D06 row
  notes it. 8.1's provable-statement table should list it.

---

## 9 · D09 — Normative encoding profile

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D09, P1, track E. Closes only
by vectors in `conformance/vectors/` (Epic 6, issue 45). Produces S05 with F-B10.
**Bears on:** §2 *Bitmap Encoding*, §3 type grid, §4 reserved layouts, §8 *Domain Separation*, D01
(caps codecs), D06 (leaf preimage).

### Question

Several encodings are declared normative without the bytes being given (Astra D02). Each row below
is a value an independent encoder must produce identically, or the roots diverge.

### Recommendations, one per item

| # | Item | Recommendation | Decoder rule |
| --- | --- | --- | --- |
| 9.1 | Roaring format for `BitmapContainer.roaring` | **Roaring portable format, 32-bit**, as specified by the Roaring format spec (the format CRoaring and the Java/Go/Rust ports interoperate on), *with* the run-container cookie | reject any other cookie or version |
| 9.2 | Container-type selection | **Canonical form = `runOptimize` applied, always.** For one 16-bit chunk: a run container iff it is strictly smaller than the alternative; else an array container iff cardinality ≤ 4096; else a bitmap container. This is CRoaring's `runOptimize` decision rule, stated as a rule rather than a library call | reject a serialization whose container types differ from the canonical choice for its contents |
| 9.3 | `Roaring64` for `CellHistory` / `IndexHistory` | **Not normative.** Both tables are out of commitment (§8). Their serialization is a storage choice; recommend the same portable 64-bit format for uniformity but state that a node may use any | — |
| 9.4 | Odd `prefix_len` padding | low nibble of the final `prefix` byte is `0x0` | reject a non-zero pad nibble |
| 9.5 | `EMPTY_ROOT` | `Hash(0x07)` — a new domain byte for "empty trie", no payload. Not all-zero: zero is also the tombstone tag and an "absent" convention elsewhere, and a fixed constant should be domain-separated like every other digest | — |
| 9.6 | `typeTag` for reserved-record cells | **Reserved cells use grid types**: caps `u32` (id 12), allocator and model values `u64` (13), `#key` `bytes32` (11), `#roots` value = 64-byte `bytes` (id 3, var-length, field-only), `@modelWeight` `u64`. Kind bit 0 for all. Strike §3's "reserved layouts … rather than drawing on this grid" | as for user cells |
| 9.7 | Absent pre-image in `IndexChangeSet` (value `B256`) | **zero-length value** ⇔ absent; 32 bytes ⇔ a root | reject any other length |
| 9.8 | Absent pre-image in `CellChangeSet` | `typeTag = 0x00`, empty payload — the same bytes as an overlay tombstone (§10) | reject `0x00` with a non-empty payload |
| 9.9 | `bool` | exactly one byte, `0x00` or `0x01` | reject other values or lengths |
| 9.10 | Type-id map | the §3 grid **as shipped** is the map; frozen by the `Superblock` `format` value at genesis; a later `format` may add ids, never reassign | unknown id ⇒ `InvalidArgument` on write, corrupt-state error on read |
| 9.11 | Cross-table key caps | genesis validation checks the **longest derived key**: `IndexChangeSet` = `8 + L` where `L` = `len(cellKey) + 1 + 1 + maxValueLen`; must be ≤ MDBX's max key size for the page size (4 KiB pages: 2022 B in libmdbx). State the formula, not the number | refuse genesis |
| 9.12 | Leaf preimages (D06) | `CellTrie` leaf `Hash(0x00 ‖ trieKey ‖ Hash(typeTag ‖ value))`; `IndexTrie` leaf `Hash(0x02 ‖ trieKey ‖ bitmapHash)`; `BitmapTrie` leaf `Hash(0x04 ‖ hi48 ‖ roaring)` | — |
| 9.13 | New `#params` caps (D01) | `#maxFilterGroups`, `#maxPredicatesPerGroup`: `u32` BE | as caps |
| 9.14 | `#meta` value (added 2026-10-10) | `bytes32` field: `cells ‖ cellBytes ‖ indexedCells ‖ indexBytes`, each `u64` BE, sizes per metering D4 (design §3) | reject another type, kind or length |
| 9.15 | Generated record key (added 2026-10-10) | `H("golemdb/record-key/v1" ‖ #keySeed ‖ recordID)`, `recordID` as `u64` BE (design §4 *Record-key modes*) | — |
| 9.16 | `#keyMode`, `#keySeed`, new caps and counters (added 2026-10-10) | `#keyMode` `u32` (0 caller-assigned, 1 generated); `#keySeed` `bytes32`, generated mode only; `#maxRecordCells`, `#maxRecordIndexedCells`, `#minActivationDelay` `u32`; `#alloc` `#liveCells`, `#indexTerms` `u64` | as the other `#params` cells |
| 9.17 | Genesis identity (added 2026-10-10) | `H("golemdb/genesis/v1" ‖ 0x00 ‖ format u32 ‖ hash_id u16 ‖ roaring u16 ‖ count u32 ‖ for each genesis cell in key order: len u32 ‖ key ‖ len u32 ‖ value)` (API *What genesis writes*) | mismatch on reopen is `GenesisMismatch` |

**Non-canonical input is rejected, not normalised.** A decoder that silently re-encodes hides a
divergent peer; rejection surfaces it at the boundary. The error is `InvalidArgument` for
caller-supplied bytes and a corrupt-state error for stored bytes.

**Steelman for library defaults instead of 9.2's rule:** "whatever CRoaring emits" is easier to
implement. It loses because a second implementation in another language cannot reproduce "whatever
CRoaring emits" without reading CRoaring's source, and the rule is three lines.

### Vectors (the acceptance check)

```
V1  empty trie root; singleton CellTrie root; singleton IndexTrie root; singleton BitmapTrie root (one container)
V2  prefix_len 1, 3, 11 (odd) and 2, 10 (even) — node bytes and hash
V3  bool true/false as attribute and as field; a 0x02 bool rejected
V4  CellChangeSet absent pre-image; IndexChangeSet absent pre-image
V5  every reserved cell: #params caps, #alloc, #roots (one entry), #recordKeys (one binding), @meteringModel, @modelWeight — tagged bytes and leaf hash
V6  the same Roaring set reached by (a) inserting ascending, (b) inserting descending, (c) insert-then-remove — identical bytes
V7  a 16-bit chunk at cardinality 4096 and 4097 (array/bitmap boundary); a chunk that is a single run of 5000 (run container)
V8  the §6 worked example (records 100, 70 000, 1 179 700) — full node and container bytes and the bitmapHash
V9  the §8 worked example — CellTrie node bytes, leaf hashes with the D06 nested preimage, StateRoot
V10 genesis validation: a #maxStrLen that fits the Index key but not the IndexChangeSet key → refused
V11 #meta of an empty record and of the metering worked example's record; a generated key for a fixed seed and IDs 64, 65; the genesis identity of Genesis::DEV-equivalent settings
```

### Text to change on acceptance

- §2 *Bitmap Encoding*: 9.1–9.3. §8 *Domain Separation*: 9.4, 9.5 (domain table row `0x07`), 9.12.
  §3: strike the reserved-layout exception; 9.9. §4: reserved cells' types (9.6); genesis validation
  formula (9.11). §7: 9.7, 9.8. §4 `#params`: 9.13.
- New appendix *Normative Surface* (S05): every item above, plus the list of what is **free**:
  physical trie layout (§9), `leaf_paths` and `leaf_value_hashes` (stored, not hashed), history
  serialization (9.3), NippyJar/segment format (§11), warm caches, MDBX page size (given 9.11 holds).
- `conformance/vectors/` gains V1–V10; `CHANGES.md` D09 → `done` when an independent encoder
  reproduces them.

### Outcome

_(architect fills in per item; 9.2, 9.5, 9.6 are the ones with real alternatives)_

---

## 10 · D13 — Environment assumptions

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D13, P2, track M. Produces the
*Assumptions* section of the overview (S06). **Bears on:** every "bounded by RAM" and "MDBX
guarantees" sentence (§4, §10, §11, §13); D04 (sync mode); D03 (failure classes).

### Question

The document leans on MDBX's durability and on RAM in a dozen places without saying what it assumes
of either, or who enforces the memory bounds (Astra D11/Elena; rubric R3). Two things to fix:

- **13.1** What the engine assumes of MDBX, the filesystem and the process.
- **13.2** Who owns the memory caps for overlays, undo logs, sealed candidates, staged segment rows
  and warm sequences — and, critically, whether any of them can *reject an operation*.

### 13.1 Assumptions to state (recommendation: state exactly these)

| Assumption | Value | Why it matters |
| --- | --- | --- |
| MDBX durability mode | `SYNC_DURABLE` (the default): a committed write transaction is on stable storage before `commit` returns. **Not** `NOSYNC`, `SAFE_NOSYNC`, `UTTERLY_NOSYNC`, and not `WRITEMAP` | D03's failure table and §11's "MDBX is never ahead of segments" are false under any relaxed mode |
| MDBX concurrency | one write transaction at a time (MDBX's own rule); readers are lock-free MVCC snapshots; `MDBX_NOTLS` so read transactions may move between threads | D04's per-operation read transaction |
| Page size | 4 KiB unless configured; the max key size follows from it (D09 9.11) | key caps |
| Filesystem | `fsync` / `fdatasync` are honoured (no lying volatile write cache); MDBX file and segment files may be on different filesystems | recovery guarantees |
| Process | single process holds the environment read-write; other processes read-only (§4 *Conventions*) | writer exclusivity |
| Clock, randomness | the engine reads neither. `machineId` (D11) is not engine-generated | invariant 1 |
| Address space | the memory map must fit: the environment's `geometry` upper bound is an operator setting; hitting it is `MapFull` (environmental, D03) | failure class |

### 13.2 Memory bounds: who rejects

The trap: a **node-local** cap that rejects an operation on the consensus path makes block validity
depend on which machine validates — a fork.

| | A — engine enforces per-branch caps (overlay entries, log entries, staged bytes) and rejects with an error | B — engine enforces caps only if they are `#params` chain parameters | C — engine enforces **no** memory caps; it publishes its memory formula; the host bounds work through its own consensus-visible limit (block gas limit / budget); exhaustion is an environmental `Io`-class failure |
| --- | --- | --- | --- |
| Determinism | **broken** unless every node configures identical caps | kept | kept — budget already bounds every operation's work, so a block's total is bounded by the host's gas limit |
| Who sizes RAM | engine operator, per node | genesis author | host operator, from the formula and the gas limit |
| New mechanism | admission counters + error | `#params` cells + counters | none |
| Failure when exceeded | deterministic rejection (if caps agree) | deterministic rejection | node-local OOM — the same class as disk full: retry elsewhere, nothing durable changed |

**Recommendation: C**, with the formula stated in the design so the host *can* size:

```
per branch      overlay ≈ Σ touched cells (key + tag + value)     + Σ touched terms (key + 32 + touched containers)
                log     ≈ Σ operations (Σ logged pre-images)        — grows with ops, not with distinct state
                staged  ≈ Σ appended segment bytes                  (P05)
sealed          the same, frozen; × the number of simultaneously sealed candidates
warm sequence   8 B × N, plus sort keys if retained (D14)          — node-local cache, operator-capped, evicted freely
```

Budget-bounded operations make every term above a function of paid-for work, so the host's gas
limit is a RAM bound in disguise; the engine documenting the constant factors is what turns it into
a number. **A** is rejected on invariant 1. **B** is coherent but adds consensus parameters for a
property the gas limit already provides.

**Warm sequences** are exempt from all of this: they are a node-local cache whose eviction is
invisible to correctness and to cost (canonical execution), so an operator cap is fine there.

> **Alignment with the requirements.** DI-4: "A node may cap open branches as a physical limit."
> Refinement of C: the one permitted node-local cap is the **number of open branches** — `begin()`
> is host-timed and off the consensus path, so refusing it forks nothing. Data-plane outcomes
> (`create`/`get`/`patch`/`delete`/`query`/`count`) still never depend on node configuration.

### Fixtures

Not vectors — a table in the design: for a block of 1 000 operations touching 5 000 cells averaging
64 B, one candidate: overlay ≈ 0.5 MB, log ≈ 0.4 MB, sealed × 3 ≈ 2.7 MB. And a statement test: no
engine error other than `Io`/`MapFull` depends on machine configuration.

### Text to change on acceptance

- New §1 subsection *Environment Assumptions* (13.1), moved to `00-overview.md` in Phase 3.
- §10 *Sealing*: "bounded by memory" → "bounded by the host's gas limit through the formula in §1";
  same for §11 staged rows and §13 warm node.
- API *Common conventions*: `Io` / `MapFull` as environmental, non-deterministic errors (with D03).
- `CHANGES.md` D13 → `done`; D04 confirmed on sync mode.

### Outcome

_(architect fills in: 13.1 confirm/adjust; 13.2 A/B/C)_

---

## 11 · D10 — Assumed metering shape and activation semantics

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D10, P2, track M.
**Bears on:** §1 scope note, §4 `@meteringModel` / `@modelWeight` lifecycle rules, §5/§10/§11/§13
cost sentences; API *Cost and Budget*, *Administration*. **Depends on:** D01 (cost shape), D13.

### Question

- **10.1** Cost arguments in §5, §10, §11 and §13 rest on a metering model the design says is "not
  part of this document". The API's *Cost and Budget* already fixes the shape callers see. The design
  needs the same shape as a stated premise, or its cost arguments are unanchored (F-B9).
- **10.2** "Activation at `A`": the model active *for* commit `c` is the greatest version with
  `activation ≤ c`, but operations are "priced at branch base" — so a branch based at `A−1` producing
  commit `A` prices with the old model while state `A` names the new one. Which is meant? (Astra
  D09)
- **10.3** Install requires `activation > head`. Installing at head 99 with `A = 100` satisfies it and
  leaves no upgrade window. Is a minimum window required, and where does it live?
- **10.4** An incomplete weight set at `A` "fails the activation" — halt, reject, stay on the old
  model, or repair?

### 10.1 The assumed shape (recommendation: import the API's contract as a premise)

Add to §1 an *Assumed metering interface* block, quoting the API rather than restating it:

> cost = Σ n_c × w_c + bytes_written × w_b, over op classes `c` of the active model; deterministic;
> operation-local and additive; nothing physical priced; cost is the canonical execution; monotone.
> `OutOfBudget{spent}` with no partial results. `commit` unmetered because pre-paid; `rollback` and
> `checkpoint` unmetered (§10).

and a table of **what each chapter contributes as countable inputs**: D01's counts (Index seeks,
cursor steps, trie reads, containers, bitmap ops), §12's sort fetches per level, §13's page
materialisation, §10's per-cell write and per-container write, §11's append bytes. The op classes
and weights themselves stay in the metering chapter. Nothing else is needed for the design's cost
sentences to be well-founded.

### 10.2 Which model prices a commit

| | A — the model active at the **branch base** (`activation ≤ base`) | B — the model active at the **commit being produced** (`activation ≤ base + 1`) |
| --- | --- | --- |
| "Activation at A" means | first observed in state A; first *used* for commit A+1 | commit A and later are priced by it |
| Analogy | — | Ethereum fork rules: block N uses the rules of block N |
| Receipt `priced_at` | base | target = base + 1 (known at `begin()`: a branch's target is always base + 1) |
| Weights readable | at base — in both cases, since the admin commit installing them is ≤ head | same |

**Recommendation: B.** It makes "activation at A" mean what everyone will read it to mean, matches
fork semantics hosts already reason in, and costs nothing: a branch's target commit is known at
`begin()`. §4 rule 3 changes from "at its branch's base commit" to "for the commit its branch
produces".

### 10.3 Minimum window

**Recommendation:** a chain parameter `#minActivationDelay` (`u32`, commits) in `#params`; install
requires `A ≥ head + 1 + #minActivationDelay`. The host chooses the value at genesis; for Arkiv a
value in the order of a day of blocks. Alternative — leave it to the admin — is rejected because
the text currently *implies* the inequality provides a window, and an operator will believe it.

### 10.4 Incomplete weights

The current rule ("check completeness at A; nodes without `v+1` code halt at A regardless") has a
bad failure mode: `v+1` nodes see an incomplete set and — under "stay on old" — keep running on
`v`, while nodes without `v+1` code have halted; under "halt" the chain stops with no admin able to
commit a repair.

| | A — check at **install**, by the installing node's code; activation at A unconditional | B — check at A; on failure stay on the old model | C — check at A; on failure halt |
| --- | --- | --- | --- |
| Who can check | the installing node must run `v+1` code — which it must, since it is performing the upgrade | every `v+1` node | every `v+1` node |
| Other nodes at install | accept the admin commit without checking completeness (they cannot); they check what any node can: version > current, `A` in range, cells parseable | same | same |
| Failure surface | `InvalidArgument` on the install call — before anything is committed | split: `v+1` nodes on `v`, old nodes halted | chain halted; no repair path |
| Defensive re-check at A | yes — if it ever fails, that is a code bug, and halting is correct | — | — |

**Recommendation: A**, with the defensive re-check. It moves the failure to the one moment when a
human is present and nothing has been committed. §4 rule 1's sentence "Deferring the check is what
preserves the upgrade window" is replaced by 10.3, which preserves it explicitly.

### Fixture (timeline)

As decided (10.2-A, 10.3, 10.4 unpriced), with `#minActivationDelay = 3`:

```
head 99   install v2: @meteringModel[2] = A, @modelWeight[2‖*]; A ≥ 99 + 1 + 3 = 103; choose A = 103
          the admin tool, running v2 code, rejects a missing or misspelled weight name before sending
          nodes replaying the install check only: version 2 > 1, A within bounds, cells well-formed
100–102   calls priced by v1; priced_at = the branch base (99..101) or the head at admission
103       head reaches 103: v2 is active. Branch work based on 102 that produces commit 103 was
          still priced by v1 (priced_at = 102); calls admitted at head 103 use v2 (priced_at = 103)
          a v2 weight with no value is unpriced: calls needing it fail with OutOfBudget{required: None},
          and a warn log names the weight; an admin weight patch repairs it from the next head
          a node without v2 code halts when its head reaches 103 ("upgrade required")
```

### Text to change on acceptance

- §1: *Assumed metering interface* block and input table. §4 lifecycle rules 1 and 3 rewritten;
  `#params` gains `#minActivationDelay` (D09 codec). API *Administration*: install-time completeness
  check, `priced_at` = target. `[API]`.
- `CHANGES.md` D10 → `done`; §4/§13 status lines drop `depends-on-metering` where the premise now
  exists in-file.

### Outcome

_(architect fills in per 10.2, 10.3, 10.4)_

- **10.1 (2026-10-10):** largely overtaken: the metering spec now exists and the design links to
  it; the *Assumed metering interface* block is no longer needed as a premise.
- **10.2 (2026-10-10): A**, the model active at the **branch base**. Metering D9 and the API spec
  specify it: branch work based on head 99 that produces commit 100 uses the old schedule;
  `priced_at` = base, and for calls without a branch the head at admission. Reason: the schedule
  is captured once, before any work, from committed state, so a call never needs a schedule that
  is not yet committed.
- **10.3 (2026-10-10): adopted.** `#minActivationDelay` (`u32`, commits) in `#params`; install
  requires `A ≥ head + 1 + #minActivationDelay`. Landed in design §4, metering D9 and the API's
  *Administration*.
- **10.4 (2026-10-10): neither A nor B nor C — missing weights are unpriced.** At activation, a
  weight the model's code expects but that has no value makes every call needing it fail with
  `OutOfBudget{required: None}`, also under `Unlimited`, like an arithmetic overflow. Nothing halts
  or splits, the affected operations are unavailable rather than free (fails safe, R4), and the
  admin repairs it with an unmetered weight patch while the chain runs. A default of 0 was
  rejected: it makes the operation free and lets a block's budget no longer bound its work.
  - Weight names are a fixed list coded with each model version. Patches to the active model
    naming an undeclared weight are reverted (`InvalidArgument`). At install, nodes not yet
    running the new code cannot check names, so the admin tool, which runs it, must reject
    unknown or missing names before sending the install. Undeclared names are ignored at
    activation.
  - Golem DB writes warn logs for operators: at activation per missing or ignored name, and per
    call failing on an unpriced weight.
  - Landed in metering D8/D9, design §4 and the API's *Administration*.

---

## 12 · D14 — Cost qualifications

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D14, P2, track M.
**Bears on:** §12 *Fetching and Sorting*, §13 *What a Page Costs*, *Serving a page from a warm node*,
§7 history bitmaps, §10 memory sentences. **Depends on:** D10 (what is charged), D13 (memory
formula).

### Question

Several cost statements are true as counts of lookups but are read as totals (Astra D11):

| Claim | What is true | What is unstated |
| --- | --- | --- |
| "a sorted query costs O(N)" (§12, §13) | N sort-value **fetches** at level 1 | ordering is O(N log N) comparisons; comparing long `str` values costs bytes |
| warm node holds "an explicit `Vec<recordID>` — 8 B per matched record"; "binary search on the key" | 8 B per ID | a binary search on the *key* needs the keys: either retained (+ key bytes per record; 40 MB per million with 32 B keys) or re-fetched (log N `Cell` reads per page) |
| "one history lookup plus at most one change-set lookup" (§7) | true as a lookup count | a `Roaring64` grows with the item's modification count; decode cost ∝ containers; every modification **rewrites** the bitmap value |
| "every term while fewer than 65 536 records" | fixed by F15 to allocation extent | — |
| "bounded by memory" (§10, §11) | — | fixed by D13's formula |

### Recommendations

- **14.1 Sort cost.** State: *N fetches at level 1 (+ tied groups at deeper levels), then an in-memory
  sort of O(N log N) comparisons, each comparison bounded by the shorter value's length.* For metering
  (D10): charge the sort as a per-record op class `sort_record` (N × w) — the comparisons are
  memory-only and their count is algorithm-dependent, so folding them into a per-record weight is
  the deterministic choice; a canonical `⌈N log₂ N⌉` count is the alternative and is rejected as
  false precision.
- **14.2 Warm sequence contents.** State both forms and their memory: **(a)** IDs only, 8 B/record,
  page k found by fetching sort keys at the log₂ N probe positions (one `Cell` read each); **(b)**
  IDs + sort keys, `8 + Σ key bytes` per record, no fetches. Operator's choice; canonical cost is
  identical (cold execution) either way, per §13's rule. Fix the §13 table's "8 MB per million" to
  "8 MB per million for IDs; plus sort keys if retained".
- **14.3 History bitmap growth.** Add to §7: *a hot cell modified once per commit accumulates one
  bit per commit in its `CellHistory` value; Roaring run-compresses consecutive commits so the
  serialized size stays small while modifications are dense, and grows toward 2 bytes per
  modification when they are sparse. Every modification rewrites the value.* Point to the metering
  chapter's existing acknowledgement. Note the mitigation available if it ever matters: chunk
  history by commit range (a `commitNr / 2¹⁶` prefix in the key), turning the rewrite into an
  append to a bounded container — not adopted now.
- **14.4 Table of quantities** (Astra's) placed in §13 with the "stated / qualified" columns filled
  from 14.1–14.3 and D13.

### Fixtures

Arithmetic, not vectors: N = 10⁶, S = 1, 32 B `str` sort key → 10⁶ fetches, ≈ 2 × 10⁷ comparisons;
warm (a) 8 MB + 20 fetches per page, (b) 40 MB + 0 fetches. Hot cell modified every commit for 10⁶
commits → `CellHistory` value ≈ 1 run container ≈ tens of bytes; modified every 3rd commit → ≈ 2
B/modification ≈ 0.7 MB.

### Text to change on acceptance

§12 *Fetching and Sorting* (14.1), §13 *What a Page Costs* and warm-node table (14.2, 14.4), §7
(14.3). `CHANGES.md` D14 → `done`.

### Outcome

_(architect fills in: 14.1 charge model; 14.2 which form the reference implementation uses)_

- **14.1 (2026-10-10): the canonical count `⌈N log₂ N⌉ × S`**, as in metering D6, not a
  per-record weight. Measuring actual comparisons would be non-deterministic, but a modeled count
  is a fixed formula of N and so deterministic; unlike a flat per-record weight it scales with the
  log N growth of work per record, so it neither overcharges small sorts nor undercharges large
  ones (R4). A byte term for long sort keys can be added later.
- **14.3, partly covered:** metering D2 now states that a cell's `CellHistory` bitmap grows with its
  retained modifications and is rewritten on each, and covers it with a fixed part of `w_cell[op]`
  calibrated for `#minRetention`. The §7 sentence is still to add.

---

## 13 · D11 — Cursor contract

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D11, P2, track M.
**Bears on:** §13 *The Cursor and the Warm Node*, *Walking and Jumping*; API *Paging*, cursor table,
*Open items*. **Depends on:** D02 (root-based identity). `[API]`.

### Question

- **11.1** Is the cursor part of the metered, deterministic output? The API returns "a cursor for the
  next page, and a cost receipt" as siblings; §13 says `machineId` may be "a per-result random token".
  If anything in the returned structure is non-deterministic, the determinism invariant needs a
  stated boundary (Fable Argus/Hermes).
- **11.2** Fingerprint scope: *same-sequence* (covers what fixes membership and order; `projection`
  and `limit` may vary) or *same-query* (nothing may vary). Expressed as an exclusion list, per the
  §13 Open box (now in Open Questions).
- **11.3** Precedence when a request carries a cursor **and** `offset` **and/or** `at`.

### 11.1 Determinism boundary

| | A — cursor is engine output and **deterministic**: a pure function of (query, state, position); routing is not the engine's business | B — cursor carries an engine-generated random routing token, documented as the one non-deterministic field |
| --- | --- | --- |
| Invariant 1 | holds for every byte the engine returns | holds for the receipt only; the cursor is exempted by rule |
| Multi-node deployments | a proxy wraps the engine cursor in its own envelope `{ engineCursor, route }`; the engine never sees `route` | the engine emits the token; the proxy interprets it |
| Verifiability | two nodes evaluating the same page return identical cursors — testable in conformance | not testable |

**Recommendation: A.** The engine has no clock and no randomness (D13); the cursor should not be
the one exception. `machineId` leaves the engine's cursor; §13's warm-node section describes the
proxy envelope instead. The cursor's fields become `{ root, commit?, sortKey, recordId,
fingerprint }` — `root` from D02 — serialized canonically (D09 codecs), opaque to callers by
convention, deterministic by construction.

### 11.2 Fingerprint scope

| | A — same-sequence: covers everything **except** `{ projection, page.limit, page.offset, cursor, budget, debug }` | B — same-query: covers everything except `{ page.offset, cursor, budget, debug }` |
| --- | --- | --- |
| May vary between pages | projection and page size | page size only if `limit` is excluded too — otherwise nothing |
| Catches | a changed filter, sort or `at`/pinned-ness — the cases that make the key meaningless | additionally a changed projection — harmless, but a likely caller mistake |
| Useful | fetch summary columns first, full rows on later pages; adaptive page size | strictness |

**Recommendation: A**, as an exclusion list, which is what the Open box argued: an enumeration of
covered inputs under-covers silently when the query surface grows; an exclusion of the provably
harmless set stays correct. The fingerprint is `Hash(0x08 ‖ canonical(query minus excluded fields)
‖ root)` — a new domain byte, so it is also the `Stale` check of D02.

### 11.3 Precedence

**Recommendation — strict, no compound positions:**

| Request carries | Rule |
| --- | --- |
| `cursor` + `offset` | `InvalidQuery`. A cursor *is* a position; an offset relative to it would be a second position mechanism to specify and test |
| `cursor` (pinned) + `at` | `at` must equal the cursor's commit; else `InvalidQuery` |
| `cursor` (live) + `at` | `InvalidQuery` — an iteration cannot become pinned midway; start a new one |
| `cursor` whose `root ≠ #roots[commit]` (pinned) or `≠ head root` (live, and the held sequence rule) | `Stale` (D02) |

The alternative — "cursor wins, offset skips ahead from it" — is rejected because it makes a
page's position depend on two inputs with different failure modes under drift.

### Fixtures

```
K1  same query, two nodes, same state → byte-identical cursor (11.1)
K2  page 2 with a different projection → served (11.2-A); with a different sort → InvalidQuery
K3  cursor + offset → InvalidQuery; pinned cursor + matching at → served; + other at → InvalidQuery (11.3)
K4  cursor from before a rewind → Stale (D02)
```

### Text to change on acceptance

§13 cursor field list and warm-node section; API cursor table (`machineId` removed; `root` added;
fingerprint rule; precedence table; `Stale`). `CHANGES.md` D11 → `done`; Open Questions D11 row
closed.

### Outcome

_(architect fills in: 11.1 A/B; 11.2 A/B; 11.3 confirm)_

---

## 14 · D12 — Live-paging guarantee

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D12, P2, track H.
**Bears on:** §13 *What drift actually does*; API *Pinned and live*. **Depends on:** D05 (5.6).

### Question

§13 says "sorting on an immutable cell makes live paging **anomaly-free** … no pinning is needed at
all". A cursor prevents position-shift duplicates and skips; it does not freeze filter membership
or projected values (Astra D10):

```
order: rank ASC (immutable), then id.   filter: active = true
64: rank 10, active=false     65: rank 20, active=true     66: rank 30, active=true
page 1 → [65]; cursor (20, 65).   Then 64 becomes active.   page 2 → [66].   64 is never returned.
```

Also open: does a pinned cursor lease retention?

### Recommendation — narrow the claim, keep the mechanism

- **Statement for §13:** *A cursor makes live paging free of **position-shift anomalies** — no record
  is returned twice or skipped because of inserts or deletes before the position, and, with an
  immutable sort key, none because of re-ordering either. It does not make the iteration a
  snapshot: a record may enter or leave the filter between pages, and projected values are those
  at the head each page saw. Each page is a correct page of some state; the sequence is not a page
  of any one state. For snapshot semantics, pin.*
- **Three added cases** in the drift table: enters the filter before the position (never returned) ·
  leaves the filter before the position (already returned; now stale) · projected value changes
  (later pages show later values).
- **No retention lease** (D05 5.6): a pinned cursor at `C` resumes iff `C ≥ earliest`, else
  `Pruned`. Pinning selects a snapshot; it does not extend the node's obligation to keep it.

No alternative is recommended: the mechanism is right; only the adjective was too strong.

### Fixture

Astra's three-record trace above, plus: pinned at `C`; advance `head` until `earliest > C`; resume →
`Pruned`.

### Text to change

§13 *What drift actually does* (statement + three rows), *Pinned and Live* (lease sentence); API
*Pinned and live*. `CHANGES.md` D12 → `done`.

### Outcome

_(architect: confirm)_

---

## 15 · D18 — Is the reserved-record layout part of the commitment contract?

**Status:** done. **Register:** `CHANGES.md` D18, P2, track H. **Depends
on:** P07 (is a second conformant engine a goal?). Feeds S05.

### Question

`#alloc`, `#roots`, `#recordKeys`, `#params`, `@meteringModel`, `@modelWeight` are committed cells,
so `StateRoot` is a function of Golem DB's bookkeeping layout, not only of the logical database. A
second engine holding the same records would not agree on a root unless it reproduced §4 byte for
byte (Fable Athena). Is that the contract, or a Golem DB choice?

### Alternatives

| | A — the §4 layout **is** normative: "conformant engine" = reproduces the `GlobalRoot`, bookkeeping included | B — partition the commitment: `GlobalRoot = Hash(0x06 ‖ UserStateRoot ‖ IndexRoot ‖ SystemRoot)` where `UserStateRoot` covers `recordID ≥ 64` only; a second engine must match `UserStateRoot` and `IndexRoot`, may vary `SystemRoot` |
| --- | --- | --- |
| What a second engine reproduces | §2–§4, §8 exactly | §2–§3, §8 for user cells and the index; its own bookkeeping |
| Cost now | none | a third trie (or a partition of `CellTrie` by ID range: system records route under one fixed prefix), one more root in `#roots`/head (96 B), a preimage change |
| Proofs | unchanged | a proof of a user cell no longer transits system cells; a proof of the allocator or a key binding goes through `SystemRoot` |
| Honest description | "Golem DB's root" | "the database's root, plus this engine's root" |
| P07 default | matches ("not a goal for v1; §4 layout is Golem DB's") | prepares for a goal nobody has set |

**Recommendation: A for v1**, stated plainly in the normative-surface appendix (S05): *the
commitment binds the engine's bookkeeping; an engine that computes the same `GlobalRoot` is a
re-implementation of §2–§4 and §8, and that is the conformance target.* Record **B** as the path if
P07 flips — it is a contained change (one extra root, a fixed routing prefix for system records) and
is cheapest before launch, so P07 should be decided before the preimages freeze (D09).

> **Alignment with the requirements (settles the question).** DI-2 requires the commitment to cover
> "every piece of engine state that affects results (allocator, parameters, cost schedule)", and NF-8
> requires a second engine to pass "commitment vectors" to be swappable. So the bookkeeping is under
> the root *by requirement*, and root equality *is* the swappability contract. **A is the
> requirement; B is withdrawn** — it would move engine state out of the root DI-2 says it must be in.
> P07 is thereby decided (see the K4 table above).

### Text to change

S05 appendix: the statement; §1 or §8: one sentence. `CHANGES.md` D18 → `done` once P07 is
recorded.

### Outcome

**A, by requirement** (DI-2, NF-8; P07 decided 2026-09-25). B is withdrawn, not deferred: it would
move engine state out of the root DI-2 requires it in. Recorded 2026-10-01. Text landed 2026-10-01:
the statement opens design Appendix A (S05, being assembled), with a pointer sentence in §8 *Which
Tables Are Under Commitment*. `CHANGES.md` D18 and P07 are closed.

---

## 16 · D15 — `Conflict` vs `HandleInvalid` for a stale handle

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D15, P3, track L. `[API]`.

**Question.** A branch whose origin is no longer the head fails with `Conflict` on `commit` but
`HandleInvalid` on every other call. Same condition, two errors (Fable Hephaestus).

| | A — keep both: `Conflict` = "you lost the commit race", `HandleInvalid` = "you called a dead handle" | B — one error, `HandleInvalid`, for every call including `commit`; `Conflict` reserved for `expected_version` mismatch |
| --- | --- | --- |
| Caller's action | identical in both cases: re-open over the new head, re-execute | same |
| Information | tells the caller *which call* discovered it — which the caller already knows | one condition, one name |
| API error table | two rows describing one state | `Conflict` gains a single meaning |

**Recommendation: B.** The distinction carries no actionable information and costs a reader a
question. `Conflict` then means exactly one thing, the provisional optimistic-concurrency guard,
which is a different condition. `[API]`: error table and the `commit` row.

**Outcome (2026-10-10): B.** A stale handle is `HandleInvalid` on every call, `commit` included; `Conflict` is reserved for an optimistic-concurrency mismatch. Deciding factor beyond the recommendation: with A, the error depended on which call happened to detect the stale branch first, so a host had to match both for one situation. Landed in the API spec, design §10 and metering D2/D8. Implementation: `commit` passes `HandleInvalid` instead of `Conflict` as its stale-branch error.

---

## 17 · D16 — Refuse a second consecutive `rollback()`?

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D16, P3, track L. **Depends
on:** D07. `[API]`.

**Question.** §10 makes `rollback()` repeatable — each call steps back one more checkpoint — and
then warns that a defensive double call "will reach past the batch it meant to abandon". The API
leaves refusing it open.

| | A — keep repeatable (status quo) | B — `rollback()` on an **empty open frame** (no operation since the last checkpoint) is an error `NothingToRollBack` and pops nothing | C — refuse only two *consecutive* rollbacks (a counter) |
| --- | --- | --- | --- |
| Multi-frame undo | yes | no — after one rollback the open frame is empty, so a second is refused | after an intervening operation, yes |
| The footgun (`rollback` in `catch` and on the success path) | fires silently | caught | caught for the exact double-call; not for rollback-after-checkpoint-with-no-ops |
| Who needs multi-frame undo | no host in view: a block builder reverts one transaction; a client batch is one frame | — | — |
| Rule | "steps back one checkpoint, repeatable" | "undoes the open frame; refuses if it is empty" | stateful, harder to explain |

**Recommendation: B.** Multi-frame undo is a capability nobody has asked for and the design itself
flags as the way to get it wrong; refusing an empty-frame rollback removes the footgun with a
one-line rule and no state. If deep undo is ever wanted, `rollbackTo(checkpointNr)` is the honest
API for it, not repeated blind pops. `[API]`: `rollback` row and a new error.

**Outcome (2026-10-10): A**, keep multi-frame rollback, as implemented on `feature/golem-db-api`, with one refinement: once every frame has been rolled back, a further `rollback()` returns `NoFrameToRollback` instead of silently doing nothing. Documented in the API spec (*Branches*). `CHANGES.md` D16 can close.

---

## 18 · D17 — Typed segment columns?

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D17, P3, track E. **Depends
on:** P05 (segments adopted at all). Void if P05 drops §11.

**Question.** Should a segment declare a §3 type per column, so rows are self-describing, or hold raw
bytes the host interprets? (§11's former Open box; full argument in the design's Open Questions.)

| | A — untyped: a column is raw bytes | B — typed: each column declares a grid type; rows carry `typeTag`s |
| --- | --- | --- |
| Engine ignorance (invariant 3) | kept: the engine knows nothing about a receipt | the engine validates receipt fields against a type it did not need |
| Tooling | needs host code to render | renders without host code |
| Versioning | host versions its formats freely | encoding pinned at genesis (`#params` is immutable, P02) — a host cannot evolve a segment's schema without a new genesis |
| Compression | dictionary compression over opaque bytes works as well | same |

**Recommendation: A.** The genesis-pinning cost is the decisive one: §11 already worries that the
segment *set* is fixed at genesis; pinning each column's *type* as well turns every format change in
the host into a chain restart. Typing can be layered by the host (a type byte inside the row) at no
cost to the engine.

**Outcome:** _(A/B)_

---

## 19 · D19 — Per-commit log digest: what it is and where it is committed

**Status:** waiting_on_additional_input. **Register:** `CHANGES.md` D19 (new), P2, track H.
**Source:** requirement SE-1 — "for each commit the engine returns a deterministic digest of the rows
appended to each log, so the host can commit the log without re-reading it" — and the requirements'
open question 8. Surfaced by resolving P05. **Depends on:** P05 (adopted), D09 (digest bytes).
`[API]`.

### Question

§11 has no digest. SE-1 requires one per segment per commit. Two sub-questions:

- **19.1** Its definition: over which bytes, in what order, under which domain.
- **19.2** Where it is committed — by the host into its own header, or by the engine into the
  world-state root.

### 19.1 Definition (recommendation)

`logDigest(seg, c) = Hash(0x09 ‖ seg-name-length ‖ seg-name ‖ commitNr ‖ mark(c−1)[seg] ‖ n ‖ Hash(row₀) ‖ … ‖ Hash(rowₙ₋₁))`
where `Hash(row) = Hash(0x0A ‖ col-count ‖ (len ‖ bytes)*)`. Returned by `seal` (it is known then —
the rows are staged) and repeated by `commit`. A segment with no rows in `c` has a digest too (`n =
0`), so the host never has to special-case. A domain byte per level; lengths before variable
fields; the starting ordinal bound in, so the same rows at a different position hash differently.

### 19.2 Where it is committed

| | A — **host** composes `AppHash = H(GlobalRoot ‖ logDigests)` and maintains that | B — **engine** writes commit `n`'s digests as cells at commit `n+1` (lag-one, like `#roots`), so the world-state root transitively commits the log |
| --- | --- | --- |
| Who builds | Arkiv | Golem DB |
| Engine commits to log content | no — §11's stance today | yes, one commit late |
| Cells per commit | none | one per segment (32 B), under a new system record `#logDigests` (id 5), same lag-one write as `#roots` |
| Provable | against the host's `AppHash`, by host rules | against `GlobalRoot`, by the engine's ordinary cell proof; a light client with only the root can verify a receipt digest |
| Under CometBFT | the header has only `AppHash`; the host must compose | `AppHash = GlobalRoot`, unchanged |
| Engine-Arkiv-ignorance | neutral | neutral — "digest of appended rows" is not a chain concept |

**Recommendation: B.** It is less for the host to build, it keeps `AppHash` a single root, and it
makes segment content provable against the root every other proof already uses — the same argument
§4 makes for `#roots` over a side table. The cost is one 32-byte cell per segment per commit,
lag-one, and it changes the design's current "segments are outside the commitment" to "a segment's
*content* is outside; its *digest* is inside, one commit late". The requirements' open question 8
lists the same two options and notes B "is less for Arkiv to build".

### Fixture

Commit `c` appends 3 rows to `bodies`, none to `receipts`: digests for both; commit `c+1` writes
`#logDigests[c] = { bodies: d₁, receipts: d₂ }`; proof of `d₁` against `root_{c+1}`; the same three
rows appended at a different starting ordinal yield a different `d₁`.

### Text to change

§11: digest definition; *Operations* table (`seal`/`commit` return digests); catalogue row `#logDigests`
(id 5); §8 *Which Tables Are Under Commitment* gains the sentence. API `[API]`: `SealedCommit` and
`CommitId` outputs gain `logDigests`. `CHANGES.md`: D19 added.

**Outcome:** _(19.1 confirm; 19.2 A/B)_

---

## Product decisions

One table for every product-level question the programme has met: the register's K4 rows (P01–P07),
the questions the briefs surfaced (P08, P09), and the requirements document's own open questions
where this work can answer them (OQ2, OQ3, OQ5). Two sources of authority are used, and each row
names its own:

- **Requirements** — `arkiv-source-of-truth/golem-db.md`, the source of truth this design
  implements. A row answered by a cited requirement is **decided by requirement** and needs no
  product meeting.
- **Live product** — `arkiv-source-of-truth/background-information/arkiv-live-product.md`, what
  Arkiv's users have on the Braga testnet today. A capability users already have is a product fact:
  dropping it is a regression that needs a decision; keeping it needs a mechanism.

Status: **decided** (by requirement or live-product fact; consequence scheduled) · **waiting**
(recommendation made; product owner must confirm) · **open** (cannot be answered from the
documents).

| Row | Status | Question | Resolution | Authority | Consequence |
| --- | --- | --- | --- | --- | --- |
| P01 | decided | Segment set fixed at genesis vs. admin-activatable | **Fixed at genesis** | Requirements CS-5: "declared log segments" are instance parameters, "set once at initialisation, immutable thereafter" | §11 *Genesis Declaration* stands; its "graduate to the admin class" sentence becomes "would need a requirement change" |
| P02 | decided | `#params` immutable with no admin path | **Immutable** | Requirements CS-5; glossary *Instance parameters*: "writable through neither plane" | §4 stands; the "graduates to the admin class … not designed now" hedge is struck |
| P03 | decided | Launch type subset | **The §3 bold set** — `bool str bytes bytes32 u256 i32 dec256` + `bytes20` — is the live product's target list (`Int32 / UInt256 / String / Bool / Decimal / Bytes / Bytes32`) plus the address width | Live product §5 ("a superset; document the mapping"); live types today are `string \| numeric` | §3 confirmed; SDK owes the mapping `numeric` → `i32` or `dec256` by range, `string` → `str` |
| P04 | decided | Arkiv vocabulary inside a generic engine | **Reword the two definitional uses (§8 "block headers", §11 ¶1); keep examples** | Requirements: "Genericity … is the enforcement mechanism: an engine that can support a cycling-event tracker cannot have smuggled in a blockchain concept" | K1-sized edit, scheduled with the Phase 3 moves |
| P05 | decided | §11 segments: adopted or still a proposal | **Adopted** as the design of a stated requirement, with two additions owed | Requirements SE-1 *Immutable log*: append-only, `(segment, ordinal)`, outside the root, pruned with retention, may contain its own commit's root, **and "for each commit the engine returns a deterministic digest of the rows appended to each log"** | §11 status `proposed` → `recorded, open`; catalogue rows lose *proposed*; D17 live; **D19** added for the digest; shard-mark survival stays in D05 |
| P06 | done (2026-10-01) | "Full history" (§1 property 5) vs. a retention window | **A minimum retention window as an instance parameter; beyond it the consensus-path API refuses on every node; deeper history is a separate archival surface** | Requirements DI-7 *History*, CS-5 ("minimum retention window"), glossary *Retention window* | Property 5 qualified; D05 5.2 revised; history's *tier* stays open (OQ2 below) |
| P07 | done (2026-10-01) | Is a second conformant engine a goal? | **Yes, and conformance includes the root** | Requirements: "Golem-DB is swappable … the first implementation of that contract"; NF-8: a second engine must pass "encodings, **commitment vectors**, query results, cost receipts"; DI-2: the commitment covers "every piece of engine state that affects results" | D18 = A; its partitioned-root alternative withdrawn — the reserved layout is normative *by requirement* |
| **P08** | waiting | Standalone negation and existence terms: must the live DSL's `!=` / `!` on their own survive into v0.1? | **Recommend yes → adopt the live-set index term** (D01 1.1-B / 1.2-B): one engine-maintained term over all live `recordID`s, +1 container write per create and per delete; gives a universe for `NOT`, `!=`, `EXISTS`, match-all, and a free `count(*)`. D01 1.1-A would reject `status != "open"` as `InvalidQuery` | Live product: `!=` and `!` are first-class operators today; requirements OQ6 asks the same question | Product owner confirms. If yes: D01 1.1 → B, 1.2 → B, and the API's "≥ 1 positive literal" `[API]` change is dropped. Also record which `!=` semantics the live DSL has, SQL-style (excludes entities without the attribute) or MongoDB-style (includes them): the input to D01 1.5 |
| **P09** | waiting | Glob `~`: general, prefix-only, or dropped? | **Recommend prefix-only**: a literal followed by one trailing wildcard compiles to the §5 prefix scan; any other pattern ⇒ `InvalidQuery` at the SDK. Interior or leading wildcards are unbounded scans with no deterministic cost shape | Live product has general glob; the target draft says "no glob … glob pricing is nontrivial" | Product owner accepts the regression; one sentence in D01; an SDK rule |
| OQ3 | decided | Queries inside a branch (requirements open question 3) | **Keep the API rule — queries read committed state, `N−1` — and accept a one-block lag.** Arkiv's expiry must find entities with `expiresAtBlock ≤ N` while executing block `N`; an entity created and already-expired in the same block is collected at `N+1`. Nothing else in the live product queries during execution; extending `query` through the overlay would cost an index overlay read path and a second cost model for one edge case | Live product: "issues deterministic Golem-DB deletes during block processing" | No engine change; answer to be reflected into the requirements' OQ3 |
| OQ2 | open | History's tier: who reads past state, how far back, how often | Today's demand is **zero** — the live product exposes no as-of read, only events — so the minimum window can be small (a day of blocks) and D05 5.7-B (synced-node exemption) is safe. "How far back will RPC and light clients want" is a forward-looking product call | Live product (absence of the feature) | Product owner; sets `#minRetention` |
| OQ5 | open | Namespacing / multi-tenancy: engine mechanism or business logic | Not a live feature; not blocking. If the engine route is taken it becomes a requirement and a new D-row (a scope parameter on every operation, partitioning keys and index terms) | — | Product owner; no deadline |

### Conflicts between the briefs and the requirements, found while resolving these

Flagged per the programme's working rules (`AGENTS.md`, outside this repository), not silently
reconciled; each affected brief carries an *Alignment* note.

| Brief | Brief said | Requirements say | Resolution |
| --- | --- | --- | --- |
| D05 5.2 | one node-configured window, `∞` = archival | CS-5 / DI-7: the **minimum** window is an instance parameter; beyond it the consensus path refuses everywhere, whatever a node retains; longer retention is an archival surface ("not yet designed") | Revised: `#minRetention` in `#params`; `Pruned` iff `at < head − #minRetention` on every node; nodes may retain more, served only through the archival surface (new, non-consensus `[API]`); the GC rule uses the node's actual retention ≥ minimum |
| D05 (new 5.7) | — | open question 9: a node synced from a snapshot at `c` has no history before `c`, yet DI-7 requires as-of reads inside the window on every node | Added as 5.7 with a recommendation (synced node exempt for one window, exposed via `retention()`); needs the architect |
| D02 2.1 | `rewind` exists, host-restricted | open question 4: "remove, or make deployment-optional with those semantics specified … leaning towards having this feature at a later stage"; its stated conditions are exactly 2.2 and 2.3 | Revised: **semantics specified now**, **feature deployment-optional and not in the v1 build** |
| D13 13.2 | engine imposes no node-local caps | DI-4: "A node may cap open branches as a physical limit" | Refined: the one permitted node-local cap is the **count of open branches** — host-timed, off the consensus path; data-plane outcomes never depend on node configuration |
| D18 | A recommended, B recorded as the path if P07 flips | DI-2 + NF-8 | B withdrawn; A is the requirement, not a choice |
| D01 1.1/1.2 | negation only beside a positive literal; no match-all | live product has standalone `!=` / `!` (P08) | Revised to the live-set term if P08 confirms |
| D01 1.1/1.2, D06 8.1 | — | open questions 6 and 10 ask exactly these; 10's option (a) is D06's non-goal | Consistent; the requirements' open questions should cross-reference the briefs |
| D09 9.3 | history bitmaps not normative | decision log *Encodings*: "one pinned Roaring serialisation" | Consistent for `BitmapContainer`; the requirements do not distinguish the two Roaring uses — 9.3 makes the distinction explicit and should be reflected back |