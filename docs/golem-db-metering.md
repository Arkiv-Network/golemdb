# Golem DB Metering

Status: draft for discussion, 2026-10-09. How Arkiv turns these costs into fees is in a separate document.

## Contents

- **Context:** [Why](#why) · [Principle](#principle) · [Scope](#scope) · [Review Findings](#todo-review-findings) · [Goals](#goals) · [Requirements](#requirements) · [Record Model](#record-model)
- **Foundation:** [D1. Where Metering Happens](#d1-where-metering-happens)
- **Cell store**
  - Writes: [D2. Write Cost Model](#d2-write-cost-model) · [D3. Modeled Trie Depth](#d3-write-metering-and-modeled-trie-depth) · [D4. Storage and Size Counting](#d4-storage-and-size-counting) · [D5. Record Shape and Deletion Bound](#d5-record-shape-and-deletion-bound)
  - Reads: [D6. Read Metering](#d6-read-metering)
- **Immutable log:** [D7. Immutable-Log Metering](#d7-immutable-log-metering)
- **Cross-cutting:** [D8. Budget, Rollback and Commit](#d8-budget-rollback-and-commit) · [D9. Cost Schedules](#d9-cost-schedules) · [D10. Golem DB Weights](#d10-golem-db-weights)
- **Illustration and open points:** [Worked Example](#worked-example-create-patch-delete) · [Open Questions](#open-questions)

## Why

Golem DB is a shared resource: anyone who pays can make every node compute and store. Metering sets that price, with four aims:

- **Decentralization:** full-node requirements stay bounded, so community members can run one on normal hardware. Archival nodes are out of scope.
- **Security:** no cheap attacks; no caller obtains more work or storage than they pay for.
- **Adoption:** no needless overcharging.
- **Predictability:** costs can be estimated before submitting.

Metering is therefore deterministic and accurate enough, not measured. Where it approximates, it rounds up: undercharging is an attack surface, overcharging only a cost.

Example attacks prevented:

- Cheap writes that trigger expensive trie updates at commit.
- Long cell names that inflate index keys.
- Value rewrites that shrink live data but grow history.
- Range queries over attributes with many distinct values, which step through many index terms; each term stepped over must be charged.

## Principle

- **Golem DB** assesses what each call costs in compute and storage, with no notion of time. Like Ethereum's `SSTORE`, storage is a one-off cost. Full nodes keep a fixed history window, so that cost covers the write and its bounded history.
- **A host** such as Arkiv builds its own pricing on top, for example adding duration to storage.

## Scope

Metering of Golem DB's read and write calls: the cost model, receipts, budgets and cost schedules. How a host turns costs into fees is out of scope.

## TODO: Review Findings

Review date: 2026-09-27. F1 covers the open accounting and contract issue.

### F1. High: Read metering lacks a complete reference execution

D6 gives principles, while D10 leaves the read-weight list open-ended. The available docs
do not fully pin bitmap traversal and intersection, materialization, byte counting,
unsuccessful lookups or charge checkpoints. Implementations could disagree on costs and
abort points even when they return the same successful results.

D6 also promises a budgeted index-ordered bulk-deletion scan that has no API definition.
The [design rejects index-ordered emission](golem-db-design.md#the-alternative-index-ordered-emission)
for general query sorting; a specialized purge scan needs a separate contract, not an
implicit change to `query`.

**Resolution needed:** specify the complete read operation taxonomy and deterministic
counting order. Define exact integer evaluation of the modeled sort cost, including
the `N = 0` case, and add conformance examples for successful and budget-aborted reads.
As part of completing read metering, define the purge scan's signature, ordering,
snapshot/cursor semantics and charges, or mark it as a proposed extension rather than
an available operation.

## Goals

1. **Fair cost:** every call pays, accurately enough, for the compute and storage it causes.
2. **Deterministic:** the same call against the same state costs the same on every instance and implementation.
3. **Configurable history:** an instance keeps either the full history (archival use) or the last n commits (full node).
4. **Useful to hosts:** a host like Arkiv gets what it needs to build its own pricing on top, such as time-based storage.
5. **Bounded deletion:** removing a record never costs more than a maximum computable at any time from its shape and the current schedule.
6. **Evolvable:** pricing can follow hardware, usage and database growth, with every instance applying a change at the same commit.

## Requirements

Cost properties:

- **R1 Deterministic:** identical on every implementation, machine and version, for the same call against the same state.
- **R2 Per call and additive:** a cost is attributable to exactly one call and summable across calls.
- **R3 Bounded pricing:** with a limited budget, pricing work must stay within that budget. Host-authorized unlimited estimation is outside this budget-bounded guarantee (D8). Write cost may depend on the call's arguments and on state the call reads anyway, never on a read made only for pricing (the no-probe rule).
- **R4 Rounds up:** no caller obtains unbounded work for bounded cost. Where cost is approximated, it is approximated upward: undercharging is an attack surface, overcharging only inefficiency.
- **R5 Defined, not measured:** cost counts logical work (rows, index terms, trie paths) as the reference execution performs it, never physical events (pages, cache hits, timing). A warm and a cold instance charge the same.
- **R6 No refunds:** operations are metered regardless of whether their effects reach disk, including execution that only changes in-memory branch state. Rolling back a frame or discarding a branch does not refund charges already incurred. Rollback and branch discard carry no charge: rolled-back operations have already paid for deferred work (trie paths, history, persistence) that is never performed, which exceeds the in-memory undo (D8). Commit carries no additional data-plane charge: record-induced work is prepaid by calls, while block/commit overhead is host-funded (D8).

Budget:

- **R7:** every call accepts a budget. A write checks each planning charge before doing the work; a charge that would exceed a limited budget aborts with no writes or partial results. `OutOfBudget{spent, required?}` reports work already performed. For writes, `required` is present only when planning has established the full representable cost. D8 defines explicit unlimited planning for host-authorized estimation.

Coverage and reporting:

- **R8:** every read and write call (`create`, `get`, `patch`, `delete`, `query`, `count`) is metered for compute, and writes also for storage. So are the immutable-log calls (`immutable_data_append`, `immutable_data_get`, `immutable_data_range_of`, `immutable_data_rows_of`; D7).
- **R9:** every receipt reports the call's cost and the commit that priced it (`priced_at`). Details of cell and index data added and removed by write calls (D4) are opt-in per call on a transport, which keeps receipts small; every implementation must support the option and return the details when requested. An in-process API may return them with every receipt, as the [Rust API](golem-db-api.md#receipts) does.
- **R10:** every user record has a maximum deletion cost, computable at any time from its shape and the current schedule. Actual deletion may cost less.

Cost schedules:

- **R11:** a cost schedule is a metering model plus its weights. The model (cost structure (D2) and counting rules) is code identified by a version; weights are committed data, one price per weight name.
- **R12:** weights are adjustable at runtime through admin writes, without upgrading Golem DB, taking effect only at a committed head boundary. Branch calls use the schedule captured at their base commit; calls without a branch capture the schedule at the current head on admission. This applies to reads of historical data as well as current data. Calls are never re-priced in flight (D9).

## Record Model

Golem DB holds three kinds of records ([design §4](golem-db-design.md#record-classes-and-the-reserved-catalogue)):

- **System records** (`#` record key prefix) hold Golem DB's own configuration and state, e.g. immutable deployment parameters (`#params`), and key bindings (`#recordKeys`).
System records are created at genesis and only Golem DB business logic is allowed to touch cells of these records. 
- **Admin records** (`@` record key prefix) are initially created at genesis. All changes need to go through Golem DB's admin (metering) API, which validates and stores them. 
Golem DB cost schedules and weights (`@meteringModel`, `@modelWeight`) which should be allowed to change through Golem DB's admin (metering) API.
- **User records** are managed by the application that uses Golem DB, for example Arkiv's entities and accounts. The write calls metered in D2 act on user records.

A record is implemented as a flat list of cells. Each cell is a key-value pair:

- **Key:** the record's ID prefix (`u64`, 8 bytes) followed by the cell name.
- **Value:** a type tag (1 byte) followed by the encoded value.

Every user record has two system cells:

- `#key`: maps the record to its record key.
- `#meta`: four counters describing the record's user cells ([below](#the-meta-cell)).

Both have fixed-length encodings. Empty user records are legal: `create` may supply
zero user-defined cells, and `patch` may remove the last one without deleting the
record. Such a record still has `#key` and `#meta`, with all four user counts in `#meta`
equal to zero, plus its binding in `#recordKeys`. `get` succeeds for the existing record;
only `delete` removes it and its binding from live state.

Empty records are not free: creating one pays admission, record-base and the three
system-cell creates (binding, `#key`, `#meta`), with no user-cell or index operations.
Deleting an empty record pays the corresponding admission, record-base and system-cell
deletion costs. A host may impose stricter content requirements without Golem DB doing so.

The reverse mapping, record key → record ID, lives in the system record `#recordKeys`: one binding cell per live record, keyed by the 32-byte record key, holding the record ID.

- `create` adds the binding; `delete` removes it. The removed binding stays in history for the retention window.
- A deleted key can be re-created; it gets a fresh binding and a new record ID. A host that must prevent key reuse enforces it above Golem DB, for example through derived keys.
- "No record with key K" is a non-inclusion proof of the binding.
- `#recordKeys` grows with the number of live records. Per-record caps (D5) apply to user records only.

### The `#meta` Cell

`#meta` holds four counters over a record's user cells; its layout, maintenance and the
record completeness proofs it enables are in
[design §3](golem-db-design.md#record-shape-the-meta-cell). Metering needs it because a
`patch` reads only the cells it touches, and the no-probe rule (R3) forbids reading the
rest just to price or validate the call.

| Counter | Used for |
| --- | --- |
| cells | cell cap check ([D2](#d2-write-cost-model) step 5); deletion bound ([D5](#d5-record-shape-and-deletion-bound)) |
| indexed cells | indexed-cell cap check (D2 step 5); deletion bound (D5) |
| cell bytes, index bytes | host storage accounting: Arkiv's `extend` prices the bytes an entity keeps stored times the blocks added to its lifetime (goal 4) |

Sizes follow D4. Golem DB's own pricing does not use the byte counters. Maintaining
`#meta` is charged as a system cell operation (D2); reading it is an ordinary `get` of
the cell, alone or with the full record, and is metered as a read (D6). The database-wide counters that set the modeled trie depth are
separate (D3).

## D1. Where Metering Happens

Meets R1, R5.

Cost is computed over Golem DB's logical schema.
The logical schema includes cell rows, index terms, posting-list containers, trie paths. It does not consider lower level items such as concrete MDBX pages. Physical page-level work depends on each node's current physical history: open read transactions, compaction and configuration change which pages a write touches. Two honest instances might well disagree on those.

Consequences:

- A storage layout change re-measures prices; it never changes the cost structure.
- Cost counts the reference execution, whatever an implementation short-circuits: cached trie nodes and held query results never reduce a charge.

## D2. Write Cost Model

Meets R1, R2, R3, R7, R11.

A write call (`create`, `patch`, `delete`) touches one record. It runs in two phases: first plan the call and accumulate its cost, then apply it. Immutable-log appends are metered separately (D7).

Execution and cost estimation use the same read-only planner, conceptually
`plan_write(operation, branch_state, budget) -> plan | error`. Its budget is
`Limited(u64)` or `Unlimited` (D8). A successful plan contains the operations and their
total cost, including planning work. Estimation returns that cost without applying the
plan; planning never consumes record IDs, changes counters or appends rollback entries.
The plan and estimate are valid only for the inspected branch state and pricing schedule;
execution must prevent intervening changes or replan.

**Order of checks.** Every call checks in three stages, cheapest and least state-dependent
first. The first failure is returned; no later stage runs.

| Stage | Checks | Cost | Failures |
| --- | --- | --- | --- |
| 1. Handle | writes and branch reads: the branch handle is known, not consumed, its origin is still the head, and, for writes, not sealed. Committed reads instead select their snapshot: the head, or the requested commit | 0 | `HandleInvalid`, `Sealed`, `CommitUnavailable` |
| 2. Admission | input form, including errors found while the request was built; reserved keys; key mode; cell names and value lengths against the genesis limits. Reads no record state | admission cost | `InvalidArgument`, `Reserved`, `KeyModeMismatch` |
| 3. Record state | the binding, then `#meta`, cells and index terms; the per-record caps | `w_rec[op]` plus the reads performed | `NotFound`, `AlreadyExists`, `CellNotFound`, cap violations |

Stage 1 comes first because it fixes the pricing snapshot (D9): without a valid handle or
snapshot, there is no schedule to charge with, so a call rejected there costs nothing (D8).
Validating a handle reads only the current head, a fixed-size row, not record state. The genesis
limits and key mode used in stage 2 are `#params` cells, fixed at genesis, so they count as
configuration rather than state. Within stage 3, the steps below fix the order.

Before each charged planning step, starting with admission and later the `w_rec[op]` binding lookup,
check that its charge fits the remaining limited budget. If it does not, stop without
performing the step and return `OutOfBudget{spent, required: None}`. Each step must have
a known, bounded charge before execution; the calibration requirements below cover
verification of those charges, while the reference admission procedure below defines
admission limits. `spent` includes only completed charged steps. Equality fits:
exhausting the budget does not by itself fail a call if no further charge is needed.

1. **Plan** (reads only, no writes):
  1. Admission (stage 2): check the input's form, reserved keys, the key mode, cell names and value lengths, charging incrementally as defined below. No record state is read. Rejected input still incurs the admission cost performed before rejection.
   2. Read the key binding (`w_rec[op]`). `create` fails with `AlreadyExists` if it exists; `patch` and `delete` fail with `NotFound` if it does not. A deleted record has no binding, so its key can be re-created.
   3. `patch` and `delete` only: read [`#meta`](#the-meta-cell) for the current counts. A `create` starts from zero.
   4. `patch` and `delete`: read every touched cell. All operations: read every touched index term. The results decide each operation and its bytes.
   5. Compute the resulting counts and check them against the per-record caps (D5).
    6. Compare the total cost (admission + record base + reads performed + planned writes) with the limited budget, if any. If it exceeds the budget, fail with `OutOfBudget{spent, required: Some(total)}`: `spent` = admission plus the record base and reads performed. No additional work is done merely to obtain `required` after an earlier budget abort. A total equal to the budget succeeds; `Unlimited` skips budget comparisons, not validation or checked arithmetic.
2. **Apply:** execute the planned writes, reusing the phase-1 reads. No metering or validation failure can occur here; only local storage faults remain, which are not cost questions.

A failure in the plan phase writes nothing, so it is charged no write cost (D8). A write is never partly applied.

The plan phase yields the call's cell and index operations, including the per-record system cells listed below. Host-funded commit overhead is separate (D8). The four cost components are:

```
record op cost       = admission cost + w_rec[op]
                     + Σ cell operation costs
                     + Σ index operation costs

admission cost       = w_admission_base
                     + entries inspected × w_admission_entry
                     + bytes validated × w_admission_byte

cell operation cost  = w_cell_read                         (only when read)
                     + w_cell[create | update | delete]
                     + cell_trie_depth × w_cell_trie_update
                     + cell bytes written × w_cell_write_byte

index operation cost = w_idx_read + w_idx[join | leave]
                     + index_trie_depth × w_index_trie_update
                     + w_idx_term_create                  (only if the term is new)
                     + index bytes written × w_idx_write_byte

batch cost           = Σ record op costs
```

**Admission cost.** The base covers fixed request checks; entry and byte weights cover
the input inspected by the reference validation procedure. Admission uses the same
budget as all later planning steps. Check each charge before the corresponding work;
stop with `OutOfBudget` if it cannot be funded. If validation discovers malformed input
or a reserved target, return the corresponding error with admission cost already spent.
Rejected input is not free. If the budget cannot fund even the admission base, no
admission work is performed and `spent` is zero. No record, cell or index charge is
collected unless its step is reached. Validation order and byte-counting rules must be
deterministic; checking a declared length is distinct from inspecting payload bytes.
Transport decoding and allocation before the Golem DB call remain host responsibilities.
Input errors that a typed API detects while the request is built, such as an invalid
name passed to a builder, count as admission: they are charged as the admission work
that would detect them, though they are reported when the call is made.

**Reference admission procedure.** Define the deterministic validation order, entry
and byte counting rules, charge checkpoints, and hard request-size and change-count
limits alongside the implementation, including which checks precede traversal and how
they are charged. Conformance tests must fix the expected charges and errors for valid
input, rejected input, and budget exhaustion. These rules form part of the versioned
metering model; they must not vary between implementations. The model must also pin
planning-read order and test charges at budget-exhaustion checkpoints.

Every planned cell or index mutation pays for one leaf's trie path, priced at the modeled depth (D3). Required reads remain charged even for no-ops. `w_cell_read` is charged only where a cell is read: by `patch` and `delete`, never by `create`.

**Record base cost.** `w_rec[op]` covers the fixed per-record work that is not a cell operation:

- All operations: reading the key's binding in `#recordKeys`. Input validation is charged separately as admission.
- `create`: increment the in-memory branch record ID counter. Persisting and merkleizing the final `#alloc` value is host-funded commit overhead (D8). A key collision fails with `AlreadyExists` (D8).
- `patch`: check the caps.
- `delete`: one seek to enumerate the record's cells.

This fixed work folds into one weight per operation with no depth term. The allocator's committed cell and trie update are accounted for separately as host-funded overhead (D8).

`w_rec[op]` must be calibrated excluding the admission work now covered by the separate
weights; adding admission to the former combined base would double charge validation.

**System cell operations.** The system cells (Record Model) are charged as cell operations, like user cells:

| Record operation | System cell operations |
| --- | --- |
| `create` | create binding, create `#key`, create `#meta`; no reads, the record is new and the binding read is in `w_rec` |
| `patch` | read `#meta`, even for empty patches; update it only when a count changes |
| `delete` | delete binding (read in `w_rec`), read and delete `#key`, read and delete `#meta` |

System cells have fixed-length encodings, so their byte terms are constants. They are not included in `#meta`'s counts or the receipt's user-cell counts (D4).

**Cell operations.** On `create`, every user cell is a cell create, without a read: the record is new. On `patch` and `delete`, a read of each touched user cell decides the operation:

| Read result and request | Cell operation | Bytes written | Bytes deleted |
| --- | --- | --- | --- |
| cell missing, value assigned | create | new cell | – |
| cell missing, `set` requested (keep the stored kind) | none: the patch fails with `CellNotFound` | – | – |
| cell present, different type, kind or value assigned | update | new cell | old cell |
| cell present, its current type, kind and value assigned | no-op: read only; no index operation | – | – |
| cell present, deletion requested | delete | – | old cell |
| cell missing, deletion requested | read only (`w_cell_read`); no index operation | – | – |

Bytes written are made live and charged (D4). Bytes deleted leave live state but stay in the change-set until pruned; they are reported, not charged.

- `w_cell[op]` covers the cell row, its history entry and its change-set entry; the cell-trie path is the depth term.
- An old value causes three kinds of work:
  - **Change-set copy.** At commit, the pre-image goes into `CellChangeSet`, once per modified cell per commit, however often the branch touched it. The work grows with the old value's size, not with the number of earlier versions. The write byte weight pre-pays it: every byte is copied at most once, when it is overwritten or deleted, so it is charged once, when written (D4).
  - **History append.** At commit, the commit number is added to the cell's `CellHistory` bitmap, once per modified cell per commit. The bitmap grows with the cell's retained modifications; `w_cell[op]` covers it as a fixed charge, calibrated for `#minRetention` (D4).
  - **Branch processing.** On every touch, planning reads the old value and the operation log captures it; a rollback copies it back. This work grows with the old value's size and recurs per touch. `w_cell_read` and `w_cell[op]` cover it as fixed charges, calibrated up to `#maxBytesLen`.
- A patch's `set` takes the kind from the cell read in step 4, so for `set` only the type or value can differ. Its failure on a missing cell is decided by a read the patch owes anyway (R3).
- A record `delete` performs a cell delete for every user cell.
- Assigning a cell its current value with its current kind is a no-op: it pays only for the reads that establish that nothing changes. A kind change alone is an update, since it joins or leaves the index. No cell write, index leave or join, or history entry follows.

**Index operations.** Leave the old term if the old cell was indexed; join the new term if the resulting cell is indexed, including attribute/field transitions. Each join or leave charges `w_idx_read`, even for the same term:

| Read result and request | Index operation | Charged |
| --- | --- | --- |
| term exists, record joins | join | `w_idx_read + w_idx[join]` |
| term missing, record joins | create term, then join | `w_idx_read + w_idx_term_create + w_idx[join]` |
| record leaves, whether or not it is the last member | leave | `w_idx_read + w_idx[leave]` |
| value or type changes | leave the old term, join the new one | both |

- `w_idx[join]` and `w_idx[leave]` cover the membership change, the posting-list container and the posting-list path; the index-trie path is the depth term.
- `w_idx_term_create` covers both creating a term and its later removal: the term row and the leaf insert and removal. It has no depth term, because the join that follows and the eventual leave already pay for the leaf's path. Removing a term emptied by its last member costs nothing at that point, so every leave costs the same and deletion stays predictable.
- A join writes the term's bytes (D4); a leave deletes them.

**Common rules.**

- Every branch is decided by a read the operation owes anyway, so cost is known before any change is applied (R3).
- Compute and storage are summed, not multiplied: a trie path rewrite costs the same for a 4-byte or a 400-byte value, while writing a value is linear in its length.
- A batch, such as a host transaction, costs the sum of its record op costs. Reads are metered separately (D6).
- Repeated touches of the same record or cell are charged per touch. An implementation may merge the work, never the charge.
- These weights aggregate [architecture §10](golem-db-architecture.md#structural-op-classes)'s finer op classes, which remain the calibration basis (D10).
- On request, a receipt includes the per-part counts as a diagnostic ledger. This per-call option must be supported by every implementation (R9).

**Calibration and verification.** Deterministic operation counts alone do not establish
that weights adequately price the work. Weights must be calibrated against measured
work and verified on representative and worst-case workloads within the deployment
limits, including maximum-sized values, failed planning and repeated mutation/rollback
cycles. Planning-read charges must cover the work performed before rejection; mutation
charges must cover in-memory execution as well as their deferred work. Fixed per-cell
charges must cover old-value processing up to `#maxBytesLen`, and a rolled-back
operation's unperformed deferred work must exceed its undo, also up to `#maxBytesLen`
(D8). A deployment whose `#maxBytesLen` makes these fixed charges excessive needs a
per-byte old-value term instead, which is a model change. If verification shows inadequate coverage or excessive overcharging, revise the weights
or cost model. Measurements inform calibration, never per-call runtime charges, which
remain determined by logical counts and the captured schedule.

## D3. Write Metering and Modeled Trie Depth

Meets R3, R4.

Trie work is deferred to commit and runs once over the branch's net changes, so a write's physical trie cost depends on the rest of the batch. Cost therefore counts **one modeled path rewrite per planned cell or index mutation**, including repeated touches of the same leaf, at a modeled depth ([architecture §10](golem-db-architecture.md#deferred-work-and-modeled-paths)). Write cost is computed from the call's arguments and the reads it owes anyway (D2), so a write can be refused before any change is applied.

In D2, these modeled path rewrites are the depth terms of every cell and index operation.

**Global counters.** This model requires Golem DB to track two database-wide counters, the cells `#liveCells` and `#indexTerms` of [`#alloc`](golem-db-design.md#alloc-recordid-1):

- **Live cells:** the number of `CellTrie` leaves, across all record classes: user cells, `#key` and `#meta` cells, bindings, root history, weights, and the counters themselves.
- **Distinct index terms:** the number of `IndexTrie` leaves.

Their only purpose is to determine the modeled trie depth. They differ from the per-record counts in [`#meta`](#the-meta-cell), which cover one record's user cells and serve caps, deletion bounds and the host. The counters are committed state, so every node derives the same depth, also after a restart. Golem DB updates them once per commit from the branch's net changes, as host-funded commit overhead (D8).

The modeled depth is derived from these counters by a pinned table. Tries are 16-ary, so depth grows as ⌈log₁₆ N⌉:

| Population N | Modeled depth |
| --- | --- |
| ≤ 16⁴ (65,536) | 4 |
| ≤ 16⁵ (1,048,576) | 5 |
| ≤ 16⁶ (16,777,216) | 6 |
| > 16⁶ | 7 |

Depth is capped at 7. ⌈log₁₆ N⌉ would reach 8 only above 16⁷ (≈268M) leaves in one trie, which the current architecture is very unlikely to reach.

- The cell trie uses the live-cell count; the index trie uses the distinct-term count. The posting-list trie keeps a constant modeled depth, at the low-cardinality envelope of [architecture §10](golem-db-architecture.md#deferred-work-and-modeled-paths).
- Path-rewrite cost = `cell_trie_depth × w_cell_trie_update` or `index_trie_depth × w_index_trie_update`, using the counters at the branch base.
- The write already knows whether it adds or removes a cell or term, so counters need no extra read (R3).
- Database growth leaves the weights: they reflect cost per node only.
- The model over-charges repeated writes to one cell and writes sharing trie prefixes; deeper paths can be under-charged unless calibration provides sufficient upward margins (R4).

Modeled depths approximate typical path work, not maximum depth. Calibration must
verify sufficient pricing margins for path-depth variation, growth within a branch,
and posting-list updates. The depth cap limits charged depth, not supported database
size or actual trie depth. The [design's depth bounds](golem-db-design.md#depth-bounds-and-future-optimization-paths)
are 64 for cell/index tries and 12 for posting-list tries; these bounds and measured
workloads inform verification rather than implying that current weights already cover
every case.

## D4. Storage and Size Counting

Meets R8, R9.

**Pay once for every byte made live.** Overwritten and deleted values move into history rather than disappearing, so charging a value's bytes once, when written, pre-pays both copying them into history later and their residency there ([architecture §10](golem-db-architecture.md#the-byte-term-pay-once-for-every-byte-made-live)). Each byte is copied at most once, so the pre-payment is exact for bytes that are later overwritten or deleted, and an over-charge for bytes that never are: the safe direction (R4). Shrinking a value still costs the new value's bytes; deleting costs no bytes.

The write byte weights assume that every value is eventually copied into history. The
copy happens once, whatever the retention window; residency does not. The weights are
calibrated for residency of `#minRetention` commits, the minimum history every node of a
deployment keeps ([design §4 `#params`](golem-db-design.md#params-recordid-0)). A
deployment with a longer `#minRetention` therefore has higher weights; a node that
retains more history than `#minRetention` does so at its own cost. Either way, all
nodes of a deployment charge the same.

**Size per record, names included.** Golem DB stores cell names in full in every cell key and every index term key. Size is counted per record, never shared:

- A cell counts `8 + |name|` bytes for its key (ID prefix and name) and `1 + |value|` bytes for its value (type tag and value).
- An index entry counts `|name| + 2 + |value|` bytes: its term key, name ‖ `0x00` ‖ type tag ‖ value.
- [`#meta`](#the-meta-cell) keeps four counts per record on this basis: cells, cell bytes, indexed cells, and index bytes. They cover user cells only; system cells are fixed-size and charged separately (D2).
- When details are requested for a write call, its receipt must report cells created, updated and deleted; index joins, leaves and terms created; and cell and index bytes written and deleted, on this basis (R9). Terms created come from the index-term reads of D2 step 4, which planning performs anyway to price `w_idx_term_create`; an implementation without metering may not report them yet. Deleted counts are reported, never refunded (R6).
- Counting per record overcounts popular index terms: the safe direction (R4).

## D5. Record Shape and Deletion Bound

Meets R10.

By D2, deleting a record costs:

```
delete cost = admission cost + w_rec[delete]
            + system cell deletes                        (binding, #key, #meta; fixed size)
            + cells         × (w_cell_read + w_cell[delete] + cell_trie_depth  × w_cell_trie_update)
            + indexed cells × (w_idx_read  + w_idx[leave]   + index_trie_depth × w_index_trie_update)
```

Every leave costs the same, whether or not it empties the term (D2), and deleted bytes were pre-paid at write (D4). A valid delete request carries a fixed-size record key and no cell changes, so its admission cost is fixed by the schedule. The total therefore depends only on the record's counts and is exact at current weights and depth.

- The cell and indexed-cell counts come from [`#meta`](#the-meta-cell), updated atomically with every count change; every successful patch pays for its read, and for its update when a count changes (D2).
- Stores counts, not cost: counts are exact and independent of weights and D3 depth. The maximum deletion cost is computed from them at current weights and depth. Actual deletion may cost less, for example after the database shrinks.
- System cells are not counted in `#meta`; their deletes are fixed-size system cell operations (D2).
- Golem DB's ceilings are deployment parameters in `#params`, fixed at genesis: `#maxCellNameLen`, `#maxStrLen`, `#maxBytesLen`, and the caps on cells and indexed cells per user record, `#maxRecordCells` and `#maxRecordIndexedCells`. System and admin records are exempt from the cell caps; they are never deleted, so they need no deletion bound.
- After every `create` and `patch`, the record's resulting counts must stay within the caps. The plan phase checks this (D2); a violation fails the call and writes nothing.
- The cell caps bound a record's maximum deletion cost; the length ceilings bound the size of each write.
- Caps are on counts and lengths rather than cost, so a weight increase cannot push existing records over a limit.

## D6. Read Metering

Meets R5, R8.

Reads are counted, not modeled: nothing on the read path is deferred, so cost accumulates as the work happens and the call aborts when it crosses its budget ([architecture §10](golem-db-architecture.md#read-metering)).

- Reads follow the D2 order of checks: the handle or snapshot first, free; then admission; then counted state reads.
- The counted descent is the reference one, whether or not an implementation short-circuits it (D1).
- Sort comparisons are the exception: modeled as `⌈N log₂ N⌉ × S` from the match count N and S sort terms, so the choice of sort algorithm stays out of the receipt.
- Resolving an item at a past commit is a flat surcharge, independent of how far back.
- Range scans charge every index term stepped over, so ranges over attributes with many distinct values pay for their width.
- Reading `#meta` is a `get` of one cell; the full record includes it. The `id_of` and `key_of` accessors, to be specified with the proof API, are fixed-size point reads with a flat charge: `id_of` is one key resolution (`key_resolve`), `key_of` one cell read (`cell_read`).
- Golem DB provides a budgeted, index-ordered scan for bulk deletion, such as a host's expiry purge ([mapping §6](arkiv-golem-db-mapping.md#purge-before-transactions)).

## D7. Immutable-Log Metering

Meets R2, R5, R7, R8.

The immutable log is Golem DB's second storage structure: append-only, ordinal-addressed
segments attached to commits ([design §11](golem-db-design.md#11-commit-immutable-data-segments)).
It shares nothing with D2: an append reads no state and touches no trie, index or cell
history. Each append and read is attributable to one call, so it is metered per call,
unlike commit overhead (D8).

```
append cost     = w_append_base + row bytes × w_append_byte
read cost       = rows read × w_imm_read_base + bytes read × bytes_read
```

- Row bytes are the sum of the row's column lengths, before compression (R5).
- The design sets no row-size cap: with positive weights an append's size is bounded by
  its budget, with zero weights by the host.
- `immutable_data_append` checks its cost against the budget before staging the row; if
  it does not fit, it fails with `OutOfBudget` and stages nothing. Staged rows that never
  commit, for example because the sealed branch loses the commit race, are not refunded (R6).
- `w_append_byte` covers writing the row and its residency in the segment's shards until
  pruned. Like the write byte weights, it is calibrated for `#minRetention` (D4); a
  pruning strategy that keeps a segment longer does so at the node's own cost.
- `immutable_data_get` reads one row, `immutable_data_rows_of` the commit's whole run;
  `immutable_data_range_of` reads one system-segment row and no segment bytes.
- Every immutable-log call returns its result together with a receipt, like the record
  calls.
- Optional per-segment row keys ([design §11](golem-db-design.md#row-keys)) add one key-index
  write to each keyed append, with no uniqueness check, and one key resolution to each read by
  key. Their weights follow once the index layout is specified.

**Who pays is the host's decision.** A deployment has two options:

1. **Positive weights.** Golem DB reports the cost; the host passes it on to its clients
   or absorbs it. Even when absorbing it, the host gets a deterministic measure of the
   append work, for example to bound a block's resources.
2. **Zero weights.** The host funds the immutable log outright, for example because its
   clients already pay for the same bytes elsewhere, as Arkiv's users do through calldata
   gas. This is safe only if the host does not expose appends to its clients unpaid (R4).

## D8. Budget, Rollback and Commit

Meets R6, R7.

- Cost is charged at the call, against its budget, using its captured pricing schedule (D9).
- A call on an unknown, consumed, stale or sealed branch handle (`HandleInvalid`, `Sealed`) costs 0: it is rejected before admission, and no pricing snapshot is captured. Charging it would also break R1: handles are process-local, so whether one is valid depends on a node's in-memory state, not on the call and committed state. This assumes branch handles never come from untrusted callers: the host creates and holds them, as Arkiv does when it executes transactions in its own branches. A host that lets untrusted callers pass handles, for example over a remote API, must protect itself against free rejected calls, as for estimation below: rate limiting, authenticated access, timeouts.
- `OutOfBudget{spent, required?}` reports cost already incurred, including the reads that established the price. `spent` is what the call costs, so an API may carry it in the call's receipt, which every outcome has, and `required` in the error. Only `OutOfBudget` has a `required`: other failures, such as invalid input, are not budget questions, and their receipt alone states their cost. A refusal is not free. For a write, `required` is `Some(total)` when planning completed and the full cost is representable, otherwise `None`. A read aborts as it goes and does not report a full required cost.
- Any cost computation that overflows, or needs an unpriced weight (D9), is treated as `OutOfBudget{spent, required: None}`, including under `Unlimited`. Costs use checked `u64` arithmetic: neither wrapping nor saturation may turn an unrepresentable total into a valid cost.
- **Failed writes** are charged for the work done in the plan phase (D2), never for writes:

  | Failure | Charge |
  | --- | --- |
  | Admission (malformed input, `Reserved`, `KeyModeMismatch`) | Admission work performed, including the check that detects the error |
  | Key failure (`AlreadyExists`, `NotFound`) | Admission cost + `w_rec[op]` |
  | Later check (cap exceeded, invalid value, `CellNotFound`) | Admission cost + `w_rec[op]` + the cell and index reads performed |
  | `OutOfBudget{spent, required?}` | `spent`: completed charged planning steps, never more than a limited budget; `required` is present only after the full cost is established |

  The key-failure and later-check rows assume the preceding charged steps fit the budget;
  otherwise the call stops earlier with `OutOfBudget`. If the remaining budget after
  admission is below `w_rec[op]`, no binding lookup is performed; `spent` is the admission
  cost and `required` is `None`. `w_rec[op]` includes work a failed
  call never reaches, such as the record ID counter; that difference is the penalty for
  a failed call. Returning a receipt with an error code costs nothing extra.
- Rollback and branch discard carry no charge, and no weight covers undo. Undo replays the frame's operation log in memory; a rolled-back operation has already paid for deferred work (trie paths, history, persistence) that is never performed, which exceeds that undo. This mirrors Ethereum, where reverting a call frame's journal costs no gas and the gas spent is kept. Rollback refunds nothing, nor does discarding a branch. A receipt is a return value, not state, so a later rollback cannot revoke it.
- Commit carries no additional data-plane charge: modeled per-call charges prepay record-induced deferred work, and the host funds block/commit overhead as defined below.
- Removing an index term emptied by its last member is free for the same reason: the term's creation paid for it (D2).

### Host-Funded Block/Commit Overhead

The host funds commit-level database overhead outside record-call budgets and receipts:
the `#roots` insertion, persistence of the final `#alloc` value when changed, the
update of the global counters (D3), their
history/change-set and trie work, and Superblock head and commit-transaction maintenance
([design](golem-db-design.md#genesis-and-roots-as-cells)). This applies to every commit,
including empty and admin commits; it does not depend on collecting record-call charges.

Record-call charges still cover their own cell and index mutations, per-record system
cells, and associated deferred history and trie work. Host funding does not make that
work free merely because it is performed at commit. `w_rec[create]` pays for in-memory
ID allocation, not the final committed allocator update.

For Arkiv, this overhead belongs to the host's block-processing resource budget. The
host controls commit admission and frequency and bears the overhead even for an empty
block. How it funds or recovers that cost is host policy, not part of Golem DB's
per-record pricing schedule.

### Shared Planner and Arkiv Usage Contexts

The write planner represents budgets explicitly:

```
Budget = Limited(u64) | Unlimited
```

`Limited(u64::MAX)` is still a finite cap, not a sentinel for `Unlimited`. Unlimited
removes only the cost cap: validation, deployment limits and arithmetic-overflow checks
still apply. There is no separate estimation algorithm or force-completion flag.

Arkiv uses this planner in two contexts:

1. **Transaction execution and validation.** Arkiv supplies `Limited(remaining_budget)`
  in Golem DB cost units. Planning can abort early; only a successful plan is applied.
  The operation is charged its total cost once, with planning charges already included,
  or its `spent` cost on failure. A caller cannot select unlimited consensus execution
  by requesting an estimate.
2. **`eth_estimateGas` service.** Arkiv may supply `Unlimited` to calculate the full
  Golem DB write cost without applying the plan. This is a host-authorized estimation
  facility, not a promise of free or budget-bounded work by Golem DB. Arkiv remains
  responsible for its gas conversion and non-database costs. A multi-operation estimate
  that needs earlier writes visible to later ones must simulate them in a disposable
  branch; inspecting every operation against the unchanged initial state is insufficient.

The estimation host owns resource protection: IP rate limiting, authenticated or paid
access, or a caller-operated node, supplemented as appropriate by concurrency limits,
request-size limits and cancellation/timeouts. A host cancellation is not a deterministic
consensus `OutOfBudget` verdict. An estimate is valid for the inspected state and schedule,
not a guarantee that a later transaction will have the same cost. Even unlimited planning
may fail validation or overflow; it does not guarantee a successful estimate.

## D9. Cost Schedules

Meets R11, R12.

**Head is authoritative.** A **pricing snapshot** is the active metering model version
together with its complete weight set, as committed at a given head; every call computes
its costs from exactly one snapshot, named by `priced_at`. Golem DB always knows its current committed `commitNr` from
the [Superblock head](golem-db-design.md#the-superblock). It holds the active model and
its complete weight set in memory as a snapshot associated with that committed state.
At startup or whenever this snapshot is unavailable, reconstruct it from the committed
`@meteringModel` and `@modelWeight` records at head: select the greatest model version
whose activation commit is at or before head, validate its weight set, and load it.
Unsupported active models or malformed weights must prevent serving priced calls, not
silently fall back to an older schedule; missing weights are unpriced (below). Cache reconstruction does not change a call's
logical charge.

**Refresh at commit boundaries.** Whenever head advances, check model activation and
committed weight changes, then publish the corresponding head and pricing snapshot
together before admitting calls against that head. Never expose a new head with stale
weights or new weights with the old head. Uncommitted admin writes do not modify the
active snapshot. After recovery or rewind, rebuild or revalidate the snapshot against
the restored committed state; a commit number alone is not sufficient cache identity
if that number can be reused after a rewind.

- **Model = code.** Cost structure, counting rules, byte definitions and expected weight names, identified by `modelVersion` ([design §4](golem-db-design.md#meteringmodel-recordid-32)). D2's structure, D3's depth table and D4's counting are model changes: a new version.
- **Weights = data.** One `u64` per weight name per model version, stored in `@modelWeight` and versioned by Golem DB's own history.
- **Install, then activate.** A new model version is installed with its weights and an activation commit `A ≥ head + 1 + #minActivationDelay`. At most one model is pending. Nodes not yet running the new version's code cannot know its weight names, so install checks only what every node can: the version is newer, `A` is within bounds, the cells are well-formed. Typos are the admin tool's job: it runs the new code and must reject unknown or missing weight names before it sends the install.
- **Missing weights are unpriced.** If, at activation, the active model's code expects a weight that has no value, that weight is **unpriced**: any call whose cost needs it fails with `OutOfBudget{spent, required: None}`, also under `Unlimited`, exactly like an arithmetic overflow (D8). This fails safe (R4): the affected operations are unavailable until fixed, never free. Nothing halts and nothing splits, since every node prices from the same committed weights, and the admin repairs it with a weight patch, which is unmetered. Weight names the code does not declare are ignored at activation.
- **Unknown names are rejected where every node can check.** A weight patch on the **active** model naming a weight its code does not declare is reverted with `InvalidArgument`: every node runs that code, so the verdict is deterministic. Writes to a pending version are checked for form only.
- **Warn logs.** Golem DB writes a warn-level log entry, for operators' observability, at activation for every missing weight and every ignored name, and on every call that fails because a weight is unpriced, naming the weight. These are node logs, not consensus output.
- **Minimum activation delay.** `#minActivationDelay` (`u32`, commits) is a genesis parameter in `#params` ([design §4](golem-db-design.md#params-recordid-0)). It guarantees every node operator a window between a model's install and its activation in which to upgrade to code that implements it. Without it, `A > head` would allow installing at head 99 with activation 100, leaving no window. For Arkiv, a value of about a day of blocks.
- **Patch the active model.** A weight change staged while head is H takes effect only after the commit containing it succeeds and head becomes H+1. It cannot change pricing mid-branch or before persistence; a failed or discarded change has no effect.
- **Capture pricing once.** Branch calls retain the pricing snapshot from their base commit; stale branches remain subject to the existing invalidation rules. Calls without a branch capture the current head's snapshot at admission. `priced_at` records that pricing commit, not the data commit being read. A call already in progress keeps its captured snapshot even if head advances.
- **Historical data does not select historical prices.** A read targeting an old commit uses the same current pricing snapshot as a current-data read admitted at the same head. Each query page is a new call: pinning the data commit does not pin the pricing schedule across pages.
- **Pre-paid work is not re-priced.** Work paid in advance keeps the schedule of the call that paid it: term removal (`w_idx_term_create`), copying bytes into history (write byte weights) and record-induced deferred commit work. Host-funded block/commit overhead is separate (D8). Re-pricing prepaid work after a weight change is impractical; if weights rise, the later work is underpaid, bounded to one term removal per term and one copy per byte. Accepted. The same holds for a host that collects a record's deletion cost in advance, as Arkiv does for expiry: the D5 bound is computed at the weights and depth current when it is collected.
- **Authorization is the host's.** Golem DB validates the lifecycle; the host decides who may change weights. A host that changes weights inside its own commits needs branch-scoped admin calls ([mapping §8](arkiv-golem-db-mapping.md#branch-scoped-administration-required-api-extension)).

For example, a model with activation commit 100 becomes active when committed head
reaches 100. Branch work based on head 99 that produces commit 100 still uses the old
schedule. Calls admitted at head 100, including reads of commit 20, use the new schedule
and report `priced_at: 100`. This is a Golem DB committed-head boundary, not an implicit
rule to switch prices before executing a host block numbered 100.

## D10. Golem DB Weights

| Weight | Used for | Calibrated from [architecture §10](golem-db-architecture.md#structural-op-classes) op classes |
| --- | --- | --- |
| `w_admission_base`, `w_admission_entry`, `w_admission_byte` | Fixed request validation, entries inspected and bytes validated, including rejected input (D2) | reference admission validation |
| `w_rec[create]`, `w_rec[patch]`, `w_rec[delete]` | Record base cost excluding admission: binding read, in-memory record ID counter, cap check, cell enumeration (D2) | `key_resolve`, `record_create` (in-memory allocator part) |
| `w_cell_read` | Read of a touched cell before its operation (D2) | `cell_read` |
| `w_cell[create]`, `w_cell[update]`, `w_cell[delete]` | Cell compute (D2) | `cell_write` / `cell_remove`, `cell_history_append`, `cell_changeset_write` |
| `w_cell_trie_update` | Cell-trie path, per node × `cell_trie_depth` (D2, D3) | `celltrie_path_rewrite` |
| `w_cell_write_byte` | Cell storage: bytes written, including their later copy into history (D2, D4) | byte term, plus the change-set copy |
| `w_idx_read` | Read of an index term before join or leave (D2) | `index_seek` |
| `w_idx[join]`, `w_idx[leave]` | Index membership change (D2) | `index_term_flip`, `secidx_container_rewrite`, `secidx_path_rewrite` |
| `w_idx_term_create` | Creating a new term, including its later removal (D2) | new index row and index-trie leaf, plus their removal |
| `w_index_trie_update` | Index-trie path, per node × `index_trie_depth` (D2, D3) | `indextrie_path_rewrite` |
| `w_idx_write_byte` | Index storage: bytes written (D2, D4) | byte term (new term keys) |
| Read weights (`key_resolve`, `cell_read`, `index_seek`, `index_scan_step`, `sort_compare`, `bytes_read`, …) | Reads, including scans (D6) | read op classes |
| `w_append_base`, `w_append_byte` | Immutable-log append: fixed staging work, row bytes written and their shard residency (D7) | `immutable_data_append` plus the byte term |
| `w_imm_read_base` | Immutable-log row read (D7); bytes read use the `bytes_read` read weight (D6) | `immutable_data_read` |

Not weights: the D3 depth table (code, metering model version) and the D5 per-record caps (deployment parameters).

## Worked Example: Create, Patch, Delete

One record followed through `create`, `patch` and `delete`, priced by D2. It illustrates
the accounting for these operations; remaining specification gaps are tracked in
[Review Findings](#todo-review-findings). Receipt details are requested for each call.

**Weights.** Illustrative, not calibrated. Both tries sit at modeled depth 6 (about 10M cells and 10M terms), so every trie path costs 6 × 100 = 600.

Let `A_create`, `A_patch` and `A_delete` denote the admission costs calculated by D2
for these requests. The tables show the remaining costs; add the corresponding admission
cost to obtain the full charge. The illustrative `w_rec` values below exclude admission.
Admission weights and exact reference counts are not assigned numerical values here.

| Weight | Value | Weight | Value |
| --- | --- | --- | --- |
| `w_rec[create]` | 300 | `w_idx_read` | 100 |
| `w_rec[patch]` | 200 | `w_idx[join]`, `w_idx[leave]` | 650 |
| `w_rec[delete]` | 250 | `w_idx_term_create` | 400 |
| `w_cell_read` | 100 | `w_index_trie_update` | 100 per node |
| `w_cell[create]`, `[update]`, `[delete]` | 350 | `w_idx_write_byte` | 2 |
| `w_cell_trie_update` | 100 per node | | |
| `w_cell_write_byte` | 2 | | |

**The record.** A 32-byte record key and three user cells. Bytes follow D4.

| Cell | Kind, type | Cell bytes (key + value) | Index bytes |
| --- | --- | --- | --- |
| `owner` | attribute, `bytes20` | (8 + 5) + (1 + 20) = 34 | 5 + 2 + 20 = 27 |
| `status` = "active" | attribute, `str` | (8 + 6) + (1 + 6) = 21 | 6 + 2 + 6 = 14 |
| `payload` | field, `bytes` (256 B) | (8 + 7) + (1 + 256) = 272 | – |
| binding in `#recordKeys` | system | (8 + 32) + (1 + 8) = 49 | – |
| `#key` | system | (8 + 4) + (1 + 32) = 45 | – |
| `#meta` (four `u64` counts) | system | (8 + 5) + (1 + 32) = 46 | – |

### Create

The key has no binding. The term `owner` = 0x71C7… is new; the term `status` = "active" already has other members.

| Operation | Terms | Cost |
| --- | --- | --- |
| Record base | `w_rec[create]`: binding read (absent), record ID | 300 |
| Create binding | 350 + 600 + 49 × 2 | 1,048 |
| Create `#key` | 350 + 600 + 45 × 2 | 1,040 |
| Create `#meta` | 350 + 600 + 46 × 2 | 1,042 |
| Create `owner` | 350 + 600 + 34 × 2 | 1,018 |
| Create `status` | 350 + 600 + 21 × 2 | 992 |
| Create `payload` | 350 + 600 + 272 × 2 | 1,494 |
| `owner` joins a new term | 100 + 400 + 650 + 600 + 27 × 2 | 1,804 |
| `status` joins "active" | 100 + 650 + 600 + 14 × 2 | 1,378 |
| **Subtotal excluding admission** | | **10,116** |

Full cost: `A_create + 10,116`.

No cell reads: the record is new. Receipt: 3 cells created, 2 index joins, 1 term created, 327 cell bytes and 41 index bytes written. `#meta` afterwards: 3 cells, 327 cell bytes, 2 indexed cells, 41 index bytes.

### Patch

`status` changes from "active" to "closed" (the term "closed" already exists), and `payload` shrinks to 100 bytes: (8 + 7) + (1 + 100) = 116 cell bytes.

| Operation | Terms | Cost |
| --- | --- | --- |
| Record base | `w_rec[patch]`: binding read, cap check | 200 |
| Read and update `#meta` | 100 + 350 + 600 + 46 × 2 | 1,142 |
| Update `status` | 100 + 350 + 600 + 21 × 2 | 1,092 |
| Update `payload` | 100 + 350 + 600 + 116 × 2 | 1,282 |
| `status` leaves "active" | 100 + 650 + 600 | 1,350 |
| `status` joins "closed" | 100 + 650 + 600 + 14 × 2 | 1,378 |
| **Subtotal excluding admission** | | **6,444** |

Full cost: `A_patch + 6,444`.

Receipt: 2 cells updated, 1 index leave, 1 index join; 137 cell bytes written and 293 deleted; 14 index bytes written and 14 deleted. Deleted bytes are reported, not charged. `#meta` afterwards: 3 cells, 171 cell bytes, 2 indexed cells, 41 index bytes.

**The same patch with a budget of `A_patch + 5,000`** fails in the plan phase with
`OutOfBudget{spent: A_patch + 700, required: Some(A_patch + 6,444)}`. Beyond admission,
`spent` is `w_rec[patch]` (200) plus the reads performed: `#meta` (100), two cells (200)
and two index terms (200). Nothing is written.

**With `Limited(A_patch + 250)`**, admission completes and the binding lookup costs
200; the next `#meta` read would cost 100, so it is not performed. The result is
`OutOfBudget{spent: A_patch + 200, required: None}`. With `Limited(A_patch + 6,444)`,
the patch succeeds at the exact budget. With `Unlimited`, the same planner returns
`A_patch + 6,444` for estimation without applying the patch.

**Rejected input also costs.** If validation finds an invalid cell name after inspecting
some entries and bytes, the call returns `InvalidArgument` with
`spent = w_admission_base + entries inspected × w_admission_entry + bytes validated × w_admission_byte`.
The counts include the work that detected the invalid name, but not uninspected input.
No binding lookup or write is charged. If the next validation step cannot be funded,
the call instead stops before that step with `OutOfBudget` and no `required` total.

### Delete

`owner` is the only member of its term, so the term is removed; its removal was paid at creation. "closed" keeps other members.

| Operation | Terms | Cost |
| --- | --- | --- |
| Record base | `w_rec[delete]`: binding read, cell enumeration | 250 |
| Delete binding | 350 + 600 (read in `w_rec`) | 950 |
| Read and delete `#key` | 100 + 350 + 600 | 1,050 |
| Read and delete `#meta` | 100 + 350 + 600 | 1,050 |
| Read and delete `owner`, `status`, `payload` | 3 × (100 + 350 + 600) | 3,150 |
| `owner` leaves; term emptied and removed | 100 + 650 + 600 (removal pre-paid) | 1,350 |
| `status` leaves "closed" | 100 + 650 + 600 | 1,350 |
| **Subtotal excluding admission** | | **9,150** |

Full cost: `A_delete + 9,150`.

Receipt: 3 cells deleted, 2 index leaves; 171 cell bytes and 41 index bytes deleted, reported only.

### What the Example Shows

- **D5 holds.** From `#meta` (3 cells, 2 indexed cells) and the fixed admission cost of a valid delete, D5 gives `A_delete + 250 + 3,050 + 3 × 1,050 + 2 × 1,350 = A_delete + 9,150`. Byte counts are not needed.
- **Structure dominates the non-admission subtotal.** The payload's write-byte term contributes 544 of the create subtotal's 10,116 (about 5%). Any admission byte processing is charged separately; a host that prices storage over time adds that on top.
- **Deleting and creating have similar non-admission costs** (9,150 against 10,116): every row, trie path and index entry has to be touched either way. Their full costs also include their respective admission charges.
- **An index value change dominates a small patch's non-admission cell work.** Changing `status` costs 3,820 excluding admission and record-level overhead, of which 2,728 is the index leave and join: two terms, two trie paths.
- **A record's fixed non-admission cost** is 3,430 on `create` (base plus three system cells) and 3,300 on `delete`, before any user cell. Admission is added separately.

## Open Questions

1. **Is the D3 depth table worth it?** Realistic depths span 4–7 nodes, more likely 4–6. Is that range worth the extra code and a more complex pricing story for users, compared with one fixed depth folded into the weights?
2. **Depth table as code or weights?** [Design §4](golem-db-design.md#meteringmodel-recordid-32) expresses tunable constants such as a modeled depth as named weights. Should D3's thresholds and depths be weights, tunable without a new model version?
3. **Value of `#minRetention`.** How many commits must every node retain? It bounds full-node storage and sets the history residency that the write byte weights and the history part of `w_cell[op]` are calibrated for (D4).
