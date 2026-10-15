# Golem DB — API

The interface Golem DB presents to the layer above it: **what a caller can say and what comes back.**
No rationale, no mechanics, no storage layout — every such question is answered by reference:

- **[golem-db-design.md](golem-db-design.md)** — data model, schema, reserved records, history,
  commitment, branches, commit immutable data, sorting and paging (chapters 1–13). Cited as
  _design §n_.
- **[golem-db-metering.md](golem-db-metering.md)** — authoritative cost formulas, budgets,
  receipts, calibration requirements and pricing-schedule lifecycle. Cited as _metering Dn_.

Writes and branch reads take a branch handle; queries and historical reads target a `CommitId`.
Data-plane calls (`create` / `get` / `patch` / `delete`, `query`, `count`) return a metered result — a
value plus a cost receipt — or an error. Branch operations, introspection and administration are
unmetered.

## Contents

- [Data Model](#data-model) — [Records](#records) · [Record keys](#record-keys) · [Cells](#cells) · [Cell names](#cell-names) · [Cell Types](#cell-types) · [Reserved records](#reserved-records)
- [Commits and Branches](#commits-and-branches) — [Commits](#commits) · [Branches](#branches) · [Frames and checkpoints](#frames-and-checkpoints) · [Operations](#operations)
- [CRUD Operations](#crud-operations) — [`create`](#create--insert-a-new-record) · [`get`](#get--point-read-by-key) · [`patch`](#patch--partial-mutation-of-one-record) · [`delete`](#delete--remove-a-record)
- [Immutable data](#immutable-data)
- [Query](#query) — [`query`](#query--filtered-sorted-paged-read) · [Filtering](#filtering) · [Sorting](#sorting) · [Paging](#paging) · [`count`](#count--count-matches-without-materializing-records)
- [Cost and Budget](#cost-and-budget)
- [Proofs](#proofs)
- [Introspection](#introspection)
- [Administration — the metering API](#administration--the-metering-api)
- [Common conventions](#common-conventions)
- [Open items](#open-items)

---

## Data Model

### Records

```
record = (key, cells)
```

- **`key`** — the record's unique 32-byte identifier.
- **`cells`** — named, individually typed values in one flat namespace per record. Sparse: two records
  sharing a cell name need not agree on that cell's type.

Records are internally addressed by a dense, monotonically allocated id that is never reused, and the
key is stored as a reserved `#key` cell readable by `get`. One consequence reaches the API:
**creation order** is a well-defined deterministic order ([Sorting](#sorting)).
→ _design [§3](golem-db-design.md#record-identity-the-key-cell)_

> **Provisional — record versioning.** Whether records carry a `u64` version usable as an
> optimistic-concurrency guard is **not decided**. `expected_version?` appears below marked
> provisional. → [Open items](#open-items)

### Record keys

**32 bytes** (`bytes32`). Assignment mode is fixed per branch lineage; the `key` argument of `create`
must agree with it (`KeyModeMismatch` otherwise):

| mode                           | behaviour                                                              |
| ------------------------------ | ---------------------------------------------------------------------- |
| **`CallerAssigned`** (default) | the caller supplies every key on `create`; collision ⇒ `AlreadyExists` |
| **`EngineAssigned`**           | the store mints keys deterministically; `AlreadyExists` unreachable    |

A `create` whose key equals one of the engine's [reserved records](#reserved-records) ⇒ `Reserved`.
That set is a handful of exact constants, not a prefix range.

### Cells

| kind          | wire name   | indexed                    | filterable | sortable |
| ------------- | ----------- | -------------------------- | ---------- | -------- |
| **attribute** | `attribute` | yes — indexed on first use | yes        | yes      |
| **field**     | `field`     | never                      | no         | yes      |

- **Typing is per write.** Each write declares kind and type; a later write to the same name may
  change both. A value not matching its declared type's codec ⇒ `InvalidArgument`. There is no global
  name→type registry.
- **Wire shape** is `(kind, typeCode, value)`, both on input and on return.
- **Kind and type travel with the value and under the commitment**, so a stored cell is decodable
  from its own bytes and a proof attests a _typed_ value.
- There are no caller-domain fields (`owner`, `payload`, …); identity and content concepts are
  ordinary cells written by the layer above.

→ _design [§3](golem-db-design.md#cell-kinds-and-types)_

### Cell names

```
name   = ["$"] first *rest          ; 1 .. #maxCellNameLen bytes, including "$"
first  = ALPHA
rest   = ALPHA / DIGIT / "_" / "-" / "." / ":"
ALPHA  = %x41-5A / %x61-7A          ; A–Z a–z   (ABNF byte ranges, RFC 5234)
DIGIT  = %x30-39                    ; 0–9
```

- ASCII only, case-sensitive, no normalization — `Price` and `price` are different names.
- `#` (system) and `@` (admin) prefixes are excluded by the grammar. Reserved record identity and
  API capabilities, not a prefix, enforce record-class protection.
- One leading `$` is allowed and has no special engine meaning. Host protocols may reserve it
  for their own cells and enforce their own write policy. `$` alone or elsewhere is invalid.
- Violating the grammar or the `#maxCellNameLen` cap ⇒ `InvalidArgument`.

→ _design [§3](golem-db-design.md#cell-names)_

### Cell Types

| range      | who defines             | change process                                                             |
| ---------- | ----------------------- | -------------------------------------------------------------------------- |
| **1–63**   | this spec               | append-only; a new core type is a spec release                             |
| **64–127** | deployment registration | fixed per deployment — instance or compile-time config, not a runtime call |

The core type ids, their widths, order-encodings and index classes are the **type grid** of
_design [§3](golem-db-design.md#the-type-grid)_. A launch subset implements only some of them, and the
assignment may still change before launch; once shipped it is frozen.

Caller-visible rules:

- Each type has an **index class** — `none` · `eq` · `eq+range` · `eq+prefix`. A predicate the class
  does not support ⇒ `InvalidQuery`.
- `bytes` is **field-only**; an attribute of type `bytes` ⇒ `InvalidArgument`.
- **NaN** ⇒ `InvalidArgument`; `-0.0` normalizes to `+0.0`.
- **API scalars** (`CommitId`, `limit`, costs and finite budget amounts) are `u64`.
  **Budgets** are `Limited(u64)` or explicit `Unlimited` ([Cost and Budget](#cost-and-budget)).

**Custom types** are registered as deployment configuration — there is no runtime registration call. A
registration specifies `id` (64–127), `name`, `width`, `index class`, `codec` and `enc`, plus
conformance vectors an implementation must reproduce byte-identically.

### Reserved records

The engine owns a handful of records — system `#params`, `#alloc`, `#roots`, `#rootIndex`, `#recordKeys` and admin
`@meteringModel`, `@modelWeight`. Their keys are their own names, zero-padded to 32 bytes; their cells
are all `field`s, so they never appear in a filter result.
→ _design [§4](golem-db-design.md#record-classes-and-the-reserved-catalogue)_

| operation         | system                    | admin                          | user    |
| ----------------- | ------------------------- | ------------------------------ | ------- |
| `get`             | allowed                   | allowed                        | allowed |
| `query` / `count` | n/a — never indexed       | n/a                            | allowed |
| `create`          | `Reserved` (genesis only) | `Reserved` (genesis only)      | allowed |
| `patch`           | `Reserved`                | `Reserved` — metering API only | allowed |
| `delete`          | `Reserved` (nobody, ever) | `Reserved` (nobody, ever)      | allowed |
| metering API      | n/a                       | allowed, validated             | n/a     |

Reads being unrestricted is how a client discovers its deployment: `get` on `#params` returns the
caps (`#maxStrLen`, `#maxBytesLen`, `#maxCellNameLen`) and the declared immutable-data segments, on
`#roots` the committed roots of any past commit, on `#rootIndex` which commit produced a given root, and on the admin records the active metering model and its weights.

**Not engine concerns:** identity, authentication, record ownership, confidentiality, per-caller rate
limiting. → _design [§4](golem-db-design.md#what-the-engine-enforces)_

---

## Commits and Branches

→ _design [§10](golem-db-design.md#10-write-branches-and-checkpoint-frames)_

### Commits

- `CommitId: u64` — 0 = genesis, +1 per commit, gapless, immutable, durable.
- The lineage never forks; competing candidates are branches until one commits. There is no `rewind`
  in v1, so a commit cannot be undone: a host commits only blocks it will not reorg. Fork _choice_ is
  the host's job.
- Every commit's roots stay readable from `#roots`, which is what makes proofs against past commits
  possible ([Proofs](#proofs)).
- **Retention.** Every node keeps at least the `#minRetention` most recent commits readable (see
  `params`) and never prunes inside them; an operator may keep more, up to full history, and serves
  it through this same API. A read at a commit before the node's earliest retained commit raises
  `Pruned`; `retention()` reports the boundary ([Introspection](#introspection)). Block execution
  reads only the head, so nodes may differ above the floor without affecting consensus.
  → _design [§7](golem-db-design.md#retention)_

### Branches

A branch is an in-memory diff overlay over the head: volatile, lost on restart, outside the
commitment.

```
BranchHandle = (commitNr: u64, branchNr: u64)
```

- **Several branches may be open over the same head**, each seeing its own work in progress.
- **First-committer-wins.** The commit guard is `handle.commitNr == head`; the losers' `commit` ⇒
  `Conflict`.
- **A losing branch is invalidated, not rebased:** _every_ subsequent call on it, reads included, ⇒
  `HandleInvalid`. Re-open over the new head and re-execute.
- **Reads on a branch are point reads only.** `query` and `count` target committed state; a branch
  cannot query its own uncommitted writes.
- **A commit is homogeneous** — either data operations or admin operations, never both.

### Frames and checkpoints

Operations inside a branch are partitioned into **frames** delimited by **checkpoints**. Frames are
not handle-addressed: `begin` opens the branch and its first frame, and the only thing a caller states
is where a frame ends. `rollback` steps back one boundary and is repeatable, so a client builds an
all-or-nothing **batch** — a concept the engine does not have — by ending each batch with
`checkpoint()` on success or `rollback()` on failure, for any reason including reasons the engine
cannot see. Rollback cost is proportional to the operations undone, never to the size of the branch.
→ _design [§10](golem-db-design.md#frames-and-checkpoints)_

> **`rollback()` is not idempotent.** Calling it twice undoes two frames. Pair each batch with exactly
> one terminal call, `checkpoint()` or `rollback()`, never both. Whether the API should _refuse_ a
> second consecutive rollback is [open](#open-items).

### Operations

| op            | signature                        | semantics                                                                                                                                 |
| ------------- | -------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `head`        | `() → CommitId`                  | current canonical head                                                                                                                    |
| `begin`       | `(at?: CommitId) → BranchHandle` | opens a branch **and its first frame**; `at` defaults to head, must lie within retention                                                  |
| `checkpoint`  | `(b)`                            | seals the open frame and opens the next; O(1)                                                                                             |
| `rollback`    | `(b)`                            | steps back one checkpoint boundary; repeatable — see above                                                                                |
| `seal`        | `(b) → SealedCommit`             | freezes the overlay and computes the roots, persisting nothing; optional — see below                                                      |
| `commit`      | `(b) → CommitId`                 | implies a final checkpoint, and a `seal` if none was taken; origin must be the head, else `Conflict`; assigns head+1 atomically; consumed |
| `discard`     | `(b)`                            | drops the branch wholesale; receipts already returned stay valid                                                                          |
| `rewind`      | `(to: CommitId)`                 | **not in v1.** Reserved for a host-restricted reorg mechanism within the retention window (design D02)                                   |
| `branch_hash` | `(b) → B256`                     | digest of the branch's current state, computed on demand from the overlay                                                                 |
| `branch_info` | `(b) → {origin, frame_depth}`    | introspection                                                                                                                             |

**All branch operations are unmetered**, and `rollback` additionally **issues no refund**. `commit` is
unmetered at the data plane: record-induced work is prepaid and block/commit overhead is
host-funded ([Cost and Budget](#cost-and-budget)). Operation charges also cover undo work.

#### `seal` — compute the roots without persisting

`seal(b) → SealedCommit { commitNr, stateRoot, indexRoot }` splits `commit` in two: it performs
everything a commit computes — merkleization, history, change-sets — and stops short of writing
anything. The caller-visible contract:

- **`head()` does not move.** `commit` remains the single durability point.
- **A sealed branch is still `get`-readable** through its overlay; **writes are rejected**
  (`HandleInvalid`), since the diff and the roots are fixed.
- **Sealing does not reserve the head.** A sealed branch still loses the first-committer-wins race
  like any other, and takes everything staged on it — including
  [immutable-data rows](#immutable-data) — with it.
- **`seal` is optional.** `commit` on an unsealed branch does both halves, so `begin … commit`
  remains the whole lifecycle for a caller that has no use for the window.

It exists for two cases: writing data that must contain the root of its own commit
([Immutable data](#immutable-data)), and consensus protocols that separate _proposing_ a state from
_accepting_ it — a sealed branch is a computed-but-not-adopted state, and a validating node can
answer _valid_ from `seal` alone, with no durable write in the path. Several branches may be sealed
over one head at once; whichever is adopted commits and the rest leave no trace. Sealed branches are
in-memory and lost on restart.
→ _design [§10](golem-db-design.md#sealing-a-branch)_

**`branch_hash`** answers the block-validation case: a node re-executing a proposed block needs the
resulting root before it will accept the header. It is **not incremental and not O(1)** — the engine
merkleizes once at commit rather than per operation, so the call runs one pass over the branch's net
touched set, which is exactly what the overlay already holds. `commit` may reuse the result. Whether
repeated calls should be metered is [open](#open-items).
→ _design [§10](golem-db-design.md#the-in-memory-overlay)_

A worked block-building sequence using these operations is in
_arch [§9](../golem-db-architecture.md#end-to-end-example-one-block)_.

---

## CRUD Operations

Writes use a budgeted, read-only planning phase followed by atomic application. Admission
and planning are charged even if the call is rejected; no mutation is applied unless the
complete plan fits the budget. The [metering specification](golem-db-metering.md#d2-write-cost-model)
is authoritative for cost structure and counting rules.

Successful creates and patches must respect genesis-fixed limits on user-cell counts,
indexed-cell counts, names and values. System cells are excluded from user-cell counts.
Exact parameter names and metadata layout are defined with the schema; the metering
requirements are in [D5](golem-db-metering.md#d5-record-shape-and-deletion-bound).

### `create` — insert a new record

| Input     | Meaning                                                                       |
| --------- | ----------------------------------------------------------------------------- |
| `branch`  | handle the write is applied to                                                |
| `key?`    | new record's key; required in `CallerAssigned`, forbidden in `EngineAssigned` |
| `cells`   | map name → `(kind, typeCode, value)`; may be empty (`{}`) |
| `budget` | `Limited(u64)` or host-authorized `Unlimited` |
| `debug?` | bool, default false; requests the mandatory-to-support detailed cost ledger |

**Output:** the record key + a cost receipt.

The engine additionally creates the record's `#key` and `#meta` cells and its binding in
`#recordKeys`. A create of _k_ user cells therefore incurs _k_ + 3 cell-create charges,
plus admission, record-base and applicable index charges. Receipt user-cell counts exclude
these three system operations. `#key` is readable by `get`; neither `#key` nor `#meta` is
writable through the data plane. The `#meta` encoding and read visibility remain to be
specified alongside the schema.

`create` with `cells: {}` creates an existing record with no user-defined cells. Its
binding, `#key` and `#meta` are still created and charged; it performs no index joins.

**Errors:** `AlreadyExists`, `KeyModeMismatch`, `Reserved`, `InvalidArgument`, `OutOfBudget`.

### `get` — point read by key

| Input         | Meaning                                                                                    |
| ------------- | ------------------------------------------------------------------------------------------ |
| `target`      | a branch handle (reads through the overlay) or a `CommitId` (historical, within retention) |
| `key`         | record key to look up                                                                      |
| `projection?` | cell names to return, either kind; absent = full record                                    |
| `budget`     | `Limited(u64)` or host-authorized `Unlimited` |

**Output:** the record, full or projected, each cell as `(name, kind, typeCode, value)` + a cost
receipt.

An existing record with zero user-defined cells is not `NotFound`: a full `get` succeeds
and includes its readable system cells, including `#key`.

A commit-targeted `get` resolves each requested cell independently, so a projection costs only the
cells asked for, and **time travel is a flat surcharge per cell** — independent of how far back the
commit lies. → _design [§7](golem-db-design.md#resolving-a-value-as-of-a-commit)_

**Errors:** `NotFound`, `OutOfBudget`, `HandleInvalid`.

### `patch` — partial mutation of one record

| Input               | Meaning                                                                                                   |
| ------------------- | --------------------------------------------------------------------------------------------------------- |
| `branch`            | handle the write is applied to                                                                            |
| `key`               | record key to mutate                                                                                      |
| `expected_version?` | _provisional_ optimistic-concurrency guard; `Conflict` on mismatch                                        |
| `cell_changes`      | map name → `set(kind, typeCode, value)` \| `remove`; absent names untouched; may remove the last user cell |
| `budget`            | `Limited(u64)` or host-authorized `Unlimited` |
| `debug?`            | bool, default false; requests the mandatory-to-support detailed cost ledger |

**Output:** a cost receipt.

A `set` may change kind and/or type. **Changing an indexed value flips two terms**, not one — the
record leaves its old term and joins the new one. Untouched user cells contribute nothing.
The call also pays admission, record-base and the system-cell work defined by the metering
model. Assigning the current value is charged as an update and, for an indexed cell, as
leave plus join, even if an implementation elides the writes.

Removing the last user-defined cell leaves the record alive, with its binding and
system cells intact. Its user-cell counts become zero. Only `delete` removes the record;
an empty record can be populated again with `patch` without changing its identity.

**Errors:** `NotFound`, `Reserved`, `InvalidArgument`, `Conflict`, `OutOfBudget`.

### `delete` — remove a record

| Input               | Meaning                                    |
| ------------------- | ------------------------------------------ |
| `branch`            | handle the write is applied to             |
| `key`               | record key to delete                       |
| `expected_version?` | _provisional_ optimistic-concurrency guard |
| `budget`            | `Limited(u64)` or host-authorized `Unlimited` |
| `debug?`            | bool, default false; requests the mandatory-to-support detailed cost ledger |

**Output:** a cost receipt.

The record leaves live state; earlier commits stay readable while retention holds. Re-creating the
same key later is an ordinary `create`.

> **Budget note.** Deletion pays admission, record-base, cell-delete and index-leave costs,
> including deletion of the binding, `#key` and `#meta`. Its storage-write byte charge is
> zero; admission byte validation is separate. A maximum deletion cost is computable from
> record shape and the current schedule, per [metering D5](golem-db-metering.md#d5-record-shape-and-deletion-bound).
> → [Cost and Budget](#cost-and-budget)

**Errors:** `NotFound`, `Reserved`, `Conflict`, `OutOfBudget`.

---

## Immutable data

Append-only rows attached to a commit, for data that is not state: large, never queried by content,
outside the commitment, retention-bound, and possibly containing the root of its own commit. Rows
live in named **segments** declared at genesis, and are addressed by a dense per-segment `ordinal`.
→ _design [§11](golem-db-design.md#11-commit-immutable-data-segments)_

| op                        | signature                      | notes                                       |
| ------------------------- | ------------------------------ | ------------------------------------------- |
| `immutable_data_append`   | `(b, seg, row) → ordinal`      | **sealed branch only**; staged, provisional |
| `immutable_data_get`      | `(seg, ordinal) → row`         |                                             |
| `immutable_data_range_of` | `(seg, commitNr) → [from, to)` | which ordinals that commit appended         |
| `immutable_data_rows_of`  | `(seg, commitNr) → [row]`      | the whole run, one contiguous read          |

A `row` is one opaque byte array per column, the column arity being fixed per segment at genesis. The
engine assigns them no meaning and no type.

**Appends are only legal on a sealed branch** ([`seal`](#seal--compute-the-roots-without-persisting)),
which is what lets a row reference the commit's own `stateRoot`. Three consequences for a caller:

- **The returned ordinal is provisional** — valid only if this branch commits. A branch that loses
  the race leaves no trace in the segment, so ordinals are never half-assigned.
- **Rows are staged, not written**, until `commit`; a commit's rows land as one contiguous run.
- **A commit's staged rows are held in memory**, so how much one commit can carry is bounded by RAM.

`immutable_data_range_of` answers "which rows did commit _n_ append" for every cardinality — a
segment receiving one row per commit returns a range of length 1, one that received nothing returns
an empty range — so no segment declares how many rows it writes.

Segments are declared in `#params.#immutableDataSegments` as `(name, columns, compression)`, with a
global `#params.#shardSpan`. Since `#params` is immutable, **the segment set is fixed at genesis** —
a deployment should declare generously.

---

## Query

Queries run against **committed state only** — never against a branch.

### `query` — filtered, sorted, paged read

| Input     | Meaning                                                                                       |
| --------- | --------------------------------------------------------------------------------------------- |
| `at?`     | `CommitId`; absent = read the head — the choice also fixes page stability ([Paging](#paging)) |
| `query`   | the query structure — below                                                                   |
| `cursor?` | an opaque cursor from a previous page                                                         |
| `debug?`  | bool, default false; adds the per-op-class cost ledger — results unchanged                    |
| `budget` | `Limited(u64)` or host-authorized `Unlimited` |

```
query = {
    projection? : [cell name]
    filter      : ordered DNF — OR of AND-groups, negated literals allowed
    sort?       : [ (name, type, direction) ]      ordered list of terms
    page        : { limit, offset? }
}
```

**Output:** record list (full or projected), `total_matched`, the commit the page was evaluated at, a
cursor for the next page, and a cost receipt.

### Filtering

The `filter` is an **ordered** DNF and the store never reorders it:

- **AND-order** — predicates intersect in submitted order, carrying a running intermediate; a group
  stops early when that intermediate is empty. This is the caller's main optimization lever.
- **OR-order** — groups evaluate in submitted order and union. The order is pinned so the abort point,
  and thus `OutOfBudget{spent}`, is deterministic.
- **Early exit applies to unsorted queries only.** A sorted query costs O(N) whatever page was asked
  for. An unsorted query may stop once `offset + limit` records are collected, unless `total_matched`
  is required.
- A record matching several groups is returned **once**.
- **Predicates are typed:** a cell of another type under the same name is treated as absent.
- `LimitExceeded` caps group count, predicates per group and nesting.

**The store performs no statistics-based planning** — work is a pure function of
`(state, ordered filter, page)`, and query optimization is the caller's job. Consequently the
_outcome_ (results vs. `OutOfBudget`) is order-dependent while the result set is not: ordering affects
_whether_ you get results, never _which_.

### Sorting

Sorting orders the set the filter selected; it never changes which records are returned. Multi-key
ordering is native — `sort` is an ordered list of `(name, type, direction)` terms.

→ _design [§12](golem-db-design.md#12-sorting)_

### Paging

**`total_matched` is always available and always free.** (On an unsorted query, requesting it forfeits
early exit.)

A page begins where the caller says: a **cursor** walks — it carries the position of the record last
emitted; an **offset** jumps — it indexes the sorted sequence directly.

- **A cursor's position is a value, not a count**: the last emitted record's sort values plus its
  record id. Resuming takes the first element strictly greater than that key. The key is compared,
  never dereferenced, so a resume is well defined even if that record was since deleted or changed.
- **Exhaustion:** a cursor is exhausted when nothing is greater than its key — exact. An offset is
  exhausted when a page returns fewer than `limit`, which against a live query can fire early.

#### Pinned and live

Chosen when the first page is requested; the cursor carries it through the iteration.

| mode       | request         | behaviour                                                            |
| ---------- | --------------- | -------------------------------------------------------------------- |
| **pinned** | names `at`      | every page evaluated at that commit; one unchanging state            |
| **live**   | names no commit | each page resolves against the head at the time; the sequence drifts |

**Either way the response reports the commit it evaluated at.**

| mutation between pages                                | offset                                             | cursor                                           |
| ----------------------------------------------------- | -------------------------------------------------- | ------------------------------------------------ |
| insert **before** the position                        | everything shifts right; one record returned twice | **clean** — the position is a value, not a count |
| delete **before** the position                        | everything shifts left; one record skipped         | **clean**                                        |
| a record's **sort value changes** across the boundary | duplicate or skip                                  | duplicate or skip                                |
| any mutation **after** the position                   | none yet                                           | none yet                                         |

No page is ever internally wrong under either mechanism — only the sequence can be inconsistent.
**Sorting on an immutable cell makes live paging anomaly-free.** Pinning adds one read per resolved
value, roughly doubling a query's read count, and does not grow with age; range and prefix
predicates at a pinned commit also visit the terms that changed in their range during the retention
window. **A pinned iteration can be served by any node** while its commit stays within the
`#minRetention` window; the cursor holds no lease, and once its commit falls before the serving
node's earliest retained commit the next page raises `Pruned`.
→ _design [§13](golem-db-design.md#pinned-and-live)_

#### The cursor

Opaque; the query is resubmitted alongside it.

```
cursor = { commit?, sortKey, recordId, fingerprint, machineId? }
```

| field         | contract                                                                                                                                                                                            |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `commit`      | present exactly when the iteration is pinned                                                                                                                                                        |
| `sortKey`     | the last emitted record's sort values, one per term, with **absent** represented explicitly                                                                                                         |
| `recordId`    | the tie-break component of the position                                                                                                                                                             |
| `fingerprint` | attributes the cursor to a sequence; a mismatch ⇒ `InvalidQuery`, never a degraded mode. Covers at least everything determining membership and order; whether it covers more is [open](#open-items) |
| `machineId`   | opaque engine-assigned routing hint; carries no durable machine identity                                                                                                                            |

Two normative rules for implementations that keep a warm sequence:

- **Cache validity.** A held sequence may serve a request **iff the commit it was built at equals the
  commit the request resolves to.**
- **Cost is the canonical execution, warm or cold.** The receipt reports filter evaluation, sorting and
  materialisation regardless of what was short-circuited.

→ _design [§13](golem-db-design.md#the-cursor-and-the-warm-node)_

### `count` — count matches without materializing records

| Input     | Meaning                                                      |
| --------- | ------------------------------------------------------------ |
| `at?`     | `CommitId`; absent = current head; must lie within retention |
| `filter`  | ordered DNF, as in `query`                                   |
| `budget` | `Limited(u64)` or host-authorized `Unlimited` |

**Output:** a `u64` count + a cost receipt.

---

## Cost and Budget

Cost is **consensus-visible**: it decides `OutOfBudget`, which decides whether a call returns results.
The [metering specification](golem-db-metering.md) defines the cost model; this API describes
its caller-visible contract. Read counting remains subject to that document's open findings.

Guarantees a caller may rely on:

1. **Deterministic** — identical on every implementation, machine and version for the same operation
   against the same state.
2. **Operation-local and additive** — attributable to one call, summable across calls.
3. **Nothing physical is priced** — not pages, not disk bytes, not wall-clock time.
4. **Cost is the canonical execution** — caching, warm nodes and held sequences never reduce a charge.
5. **Monotone** — there are no refunds anywhere in the model.

### The formula

```
record op cost = admission cost + w_rec[op]
               + sum(cell operation costs)
               + sum(index operation costs)
```

Admission counts request validation, not database reads. Cell and index costs include
state reads, mutations, separate write-byte weights and modeled trie-path charges.
The model derives cell/index trie depths from population counters at branch base;
depth is an explicit multiplier, not solely folded into a weight. See
[metering D2](golem-db-metering.md#d2-write-cost-model) and
[D3](golem-db-metering.md#d3-write-metering-and-modeled-trie-depth) for the authoritative formulas.

### Budget

- **`budget`** is explicit: `Limited(u64)` or `Unlimited`. `Limited(u64::MAX)` is not an
  unlimited sentinel. A host controls authorization for unlimited work; Arkiv uses limited
  budgets for consensus execution and may allow unlimited planning for `eth_estimateGas`.
- A write checks each planning charge before performing the work. If it cannot fit, return
  **`OutOfBudget{spent, required: None}`** with no writes or partial results. `spent` includes
  completed admission and state-read charges, never more than the limited budget.
- If planning completes but the full cost exceeds the budget, return
  **`OutOfBudget{spent, required: Some(total)}`**. An exact-budget total succeeds. Rejected
  input still pays for validation performed; no write charge is collected for failed planning.
- Arithmetic overflow returns `OutOfBudget` with `required: None`, even under `Unlimited`.
- **Rollback and branch discard refund nothing**, including charges for purely in-memory
  work. Operation charges cover undo; rollback carries no additional charge.
- **`commit` carries no additional data-plane charge:** record-induced work is prepaid,
  while root-history, committed allocator and other block/commit overhead are host-funded.
- **Deletion is not pre-paid at creation.** A host whose entities are life-limited — where expiry, not
  a caller, triggers the delete — should pre-pay in its own pricing layer above these receipts.

### Cost estimation

The same read-only `plan_write(operation, branch_state, budget)` planner supports execution
and estimation. Estimation returns the full planned cost without applying the mutations;
it does not consume IDs or change counters. A host-authorized unlimited estimate still
performs validation and checked arithmetic. The estimate is valid only for the inspected
state and schedule. A multi-operation simulation must use a disposable branch if later
operations need to observe earlier writes. Host resource controls are described in
[metering D7](golem-db-metering.md#shared-planner-and-arkiv-usage-contexts).

---

## Proofs

Every committed state has a single 32-byte root; any cell or index term can be proved against it to a
party holding nothing but the root. Two caller-relevant properties:

- **Any past commit is provable, not just the head** — a light client holding the current root can
  verify the root at a past commit and descend from it.
- **Proofs attest typed values**, since the tag byte is inside the hashed value.

→ _design [§8](golem-db-design.md#8-state-commitment-and-global-root)_

> _Provisional._ The call's signature is not pinned. It needs a target commit and the item to prove
> (record key + cell name, or an index term), and returns the node path plus the root it verifies
> against. Non-inclusion proofs follow the same shape.

---

## Introspection

`head` and `branch_info` are in [Operations](#operations). All introspection is unmetered.

| op       | signature                                    | meaning                                                             |
| -------- | -------------------------------------------- | ------------------------------------------------------------------- |
| `roots`  | `(at?: CommitId) → {state_root, index_root}` | the committed roots as of a commit; absent = head                   |
| `params` | `() → map<name, value>`                      | the deployment's chain parameters — equivalently `get` on `#params` |
| `retention` | `() → {earliest: CommitId, head: CommitId}` | this node's earliest retained commit and its head. `earliest ≤ head − #minRetention` (or 0); above that floor it differs between nodes, and an archival node reports 0. Any read at `at < earliest` raises `Pruned` |

`branch_hash` is a branch operation and lives in [Operations](#operations) with the rest.

---

## Administration — the metering API

The **model** is code, identified by a `modelVersion`: the op-class taxonomy, counting rules, byte-term
definition and expected weight names. It changes by upgrading the engine. The **weights** are data —
one `u64` per named weight per model version — and are what this API writes.

| op               | signature                                            | meaning                                                                          |
| ---------------- | ---------------------------------------------------- | -------------------------------------------------------------------------------- |
| `install_model`  | `(version, activation: CommitId, weights: map) → ()` | installs a future model version and its complete weight set, in one admin commit |
| `set_weight`     | `(version, name, weight) → ()`                       | patches one weight                                                               |
| `metering_model` | `(at?: CommitId) → {active_version, pending?}`       | the active model and any pending activation                                      |
| `model_weights`  | `(version?) → map<name, u64>`                        | the weights of a model version; absent = active                                  |

Lifecycle rules — violations ⇒ `InvalidArgument`:

1. **Install complete, with a minimum window.** `install_model` requires `version` > current,
   `activation ≥ head + 1 + #minActivationDelay` (a `#params` value from the genesis file), parseable
   cells, and a **complete** weight set for that version's code. An incomplete set fails the install
   with `InvalidArgument`, and nothing is committed. `#minActivationDelay` is the upgrade window.
2. **The active model takes immediate patches only.** `set_weight` on it takes effect at the next
  committed head, after the admin commit succeeds, never before persistence or mid-branch.
  There is no scheduling for the current model. Future work is staged under the pending version.
3. **Capture pricing once, never re-price in flight.** A branch captures its schedule at `begin`:
  the model active at the commit the branch produces, with the weights committed at its base. A
  model activating at commit A therefore prices the branch that produces A, and weight patches apply
  from the following commit. An invalid or incomplete active model makes `begin` fail. Calls without
  a branch capture the current head's schedule at admission, even when reading historical data.
  `priced_at` identifies the pricing commit (for a branch, the commit it produces), not the data
  commit. Each query page captures pricing anew; pinning data does not pin prices.
4. **At most one pending model** at a time.

Golem DB keeps the active model and complete weights in memory, reconstructing them from
committed head and metering records when unavailable. It publishes head and the matching
pricing snapshot together at commit boundaries; uncommitted changes do not affect prices.
Activation at commit 100 applies to the branch based on head 99 that produces commit 100,
and to calls without a branch admitted at head 100 or later. See [metering D8](golem-db-metering.md#d8-cost-schedules).

**Surface separation.** `open()` returns a **data handle** and an **admin handle**. Admin operations
take no branch — each forms its own single-purpose commit — which makes commit homogeneity structural.

**Authorization is the host's.** The engine validates the lifecycle rules; it does not decide who may
call the API.

> **Consensus note.** A weight change moves the success/abort boundary and is therefore
> outcome-affecting in blockchain mode. Model activations must be chainspec-coordinated.

→ _design [§4](golem-db-design.md#meteringmodel-recordid-32)_

---

## Common conventions

- **Return shape.** A metered result is a value plus a cost receipt. Errors still report cost spent.
- **Cost receipt.** `{ cost, priced_at, ledger? }` — the cost charged; the commit that priced the
  call (for a branch call, the commit the branch produces; otherwise the head at admission); and, under `debug`, the per-op-class counts. For writes, the ledger
  includes the cell/index operation and byte counts specified by
  [metering D4](golem-db-metering.md#d4-storage-and-size-counting), excluding system cells
  from user-cell totals. Every implementation must support these opt-in write details and
  provide them when requested. A receipt is a return value, never state, and is never
  revoked by rollback or branch discard.
- **`budget`** — see [Cost and Budget](#cost-and-budget).
- **`expected_version?`** — _provisional_; see [Records](#records).
- **Determinism.** Every accept/reject verdict is a pure function of the operation and of state, and is
  decided at operation admission rather than deferred to commit.
- **One class of failure surfaces late.** Anything decidable from logical state or a static limit is
  checked at the operation. What depends on the storage engine's runtime accounting — key and value
  size limits, transaction capacity, storage exhaustion, I/O failure — can only surface at `commit`.
  Budget is deliberately _not_ in that category.

**Errors** (shared set):

| error             | raised when                                                                                          |
| ----------------- | ---------------------------------------------------------------------------------------------------- |
| `NotFound`        | the addressed record or cell does not exist at the target                                            |
| `AlreadyExists`   | `create` on an existing key in `CallerAssigned` mode                                                 |
| `KeyModeMismatch` | the `key` argument disagrees with the lineage's assignment mode                                      |
| `Reserved`        | the operation addresses a reserved record through a surface that may not modify it                   |
| `InvalidQuery`    | a predicate the target type's index class does not support, or a cursor whose fingerprint mismatches |
| `InvalidArgument` | malformed input: name grammar, cap, codec, or a metering lifecycle violation                         |
| `LimitExceeded`   | DNF caps — group count, predicates per group, nesting                                                |
| `OutOfBudget`     | the next charge or full planned cost cannot fit, or cost arithmetic overflows; carries `spent` and optional write `required`; no partial results |
| `Pruned`          | the target commit, or the commit of an immutable-data ordinal, is before this node's earliest retained commit (`retention()`); never inside the `#minRetention` window; an archival node never raises it |
| `HandleInvalid`   | a consumed branch handle, or one whose origin is no longer the head                                  |
| `Conflict`        | a commit guard failed, or an `expected_version` mismatch                                             |
| `Internal`        | engine fault                                                                                         |

For writes, `Reserved` is raised during admission and reports the cost of validation
already performed, including the check detecting the reserved target. It is distinct
from `InvalidArgument` so a host can distinguish malformed input from a reserved-structure
violation. If the budget cannot fund the check, `OutOfBudget` occurs first.

---

## Open items

| #   | Item                                                                                   | Status     |
| --- | -------------------------------------------------------------------------------------- | ---------- |
| 1   | **Record versioning / OCC** — whether `expected_version` exists at all                 | undecided  |
| 2   | **Proof call signature** — target, item addressing, returned path shape, non-inclusion | not pinned |
