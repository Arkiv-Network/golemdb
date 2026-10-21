# GolemDB Metering

Status: draft for discussion, 2026-10-09; D6 restructured into core and query reads, with a pricing formula and the API impact, 2026-10-20. How Arkiv turns these costs into fees is in a separate document.

## Contents

- **Context:** [Why](#why) · [Principle](#principle) · [Scope](#scope) · [Review Findings](#todo-review-findings) · [Goals](#goals) · [Requirements](#requirements) · [Record Model](#record-model) · [The Index](#the-index)
- **Foundation:** [D1. Where Metering Happens](#d1-where-metering-happens)
- **Cell store**
  - Writes: [D2. Write Cost Model](#d2-write-cost-model) · [D3. Modeled Trie Depth](#d3-write-metering-and-modeled-trie-depth) · [D4. Storage and Size Counting](#d4-storage-and-size-counting) · [D5. Record Shape and Deletion Bound](#d5-record-shape-and-deletion-bound)
  - Reads: [D6. Read Cost Model](#d6-read-cost-model): [Read Call Frame](#the-read-call-frame) · [D6.1 Core Reads](#d61-core-reads) · [D6.2 Query Reads](#d62-query-reads) · [D6.3 Pricing Formula and Cost Compilation](#d63-pricing-formula-and-cost-compilation) · [D6.4 API Impact](#d64-api-impact)
- **Immutable log:** [D7. Immutable-Log Metering](#d7-immutable-log-metering)
- **Cross-cutting:** [D8. Budget, Rollback and Commit](#d8-budget-rollback-and-commit) · [D9. Cost Schedules](#d9-cost-schedules) · [D10. GolemDB Weights](#d10-golemdb-weights)
- **Illustration and open points:** [Worked Example: Writes](#worked-example-create-patch-delete) · [Worked Example: Reads](#worked-example-reads) · [Open Questions](#open-questions)

## Why

GolemDB is a shared resource: anyone who pays can make every node compute and store. Metering sets that price, with four aims:

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
- Queries that combine large posting sets: cheap per filter node, expensive in total; combining sets must be charged by their size.

## Principle

- **Calls are made of steps, and every step has a charge.** A step is one unit of logical work: a key resolution, a cell read, a cell or index write, an index term stepped over. Its charge is what the step costs in cost units, fixed by the active cost schedule. A call's cost is the sum of the charges of the steps it performs, and its budget caps that sum.
- **GolemDB** assesses what each call costs in compute and storage, with no notion of time. Like Ethereum's `SSTORE`, storage is a one-off cost. Full nodes keep a fixed history window, so that cost covers the write and its bounded history.
- **Writes are priced before they apply; reads are charged as they run.** A write is planned and its full cost established before anything changes (D2). A read is charged step by step as it executes, and stops at the first step the remaining budget cannot pay (D6).
- **A host** such as Arkiv builds its own pricing on top, for example adding duration to storage. Its weights may live in GolemDB's cost schedule as host-declared weights (D9): GolemDB stores, validates and activates them with its own, and never prices with them.

## Scope

Metering of GolemDB's read and write calls: the cost model, receipts, budgets and cost schedules. How a host turns costs into fees is out of scope.

## TODO: Review Findings

Review date: 2026-09-27. F1 covers the open accounting and contract issue.

### F1. High: Read metering lacks a complete reference execution

D6 gives principles, while D10 leaves the read-weight list open-ended. The available docs
do not fully pin bitmap traversal and intersection, materialization, byte counting,
unsuccessful lookups or charge checkpoints. Implementations could disagree on costs and
abort points even when they return the same successful results.

A host's expiry purge needs no special scan: Arkiv's purge reads one equality term of its
expiry attribute in record-ID order and deletes one record at a time under its purge budget
([tokenomics](arkiv-tokenomics.md#purge-budget)). Read metering must therefore cover equality
lookups, including lookups of absent terms, and record-ID-ordered reads with a limit.

**Resolution needed:** specify the complete read operation taxonomy and deterministic
counting order in D6, pricing the execution rules of the [query proposal](query-proposal.md#execution-rules-normative),
which serve as the reference execution. Define exact integer evaluation of the modeled sort
cost, including the `N = 0` case, and add conformance vectors (R13) for successful and
budget-aborted reads. Arkiv's purge depends on the equality and absent-term charges being
pinned first.

**Progress (2026-10-20).** D6 now separates the core reads that write metering shares (D6.1) from
the query reads (D6.2), gives every read call's cost as a formula with the charge unit of each step
(D6.3), and states the API impact (D6.4). Still open: the proposed items of the
[Read Cost Items](#read-cost-items) list, the container size (item 21), the conformance vectors, and
calibrated weight values.

## Goals

1. **Fair cost:** every call pays, accurately enough, for the compute and storage it causes.
2. **Deterministic:** the same call against the same state gives the same result, costs the same and, if it runs out of budget, stops at the same step, on every instance and implementation.
3. **Configurable history:** an instance keeps either the full history (archival use) or the last n commits (full node).
4. **Useful to hosts:** a host like Arkiv gets what it needs to build its own pricing on top, such as time-based storage.
5. **Bounded deletion:** removing a record never costs more than a maximum computable at any time from its shape and the current schedule.
6. **Evolvable:** pricing can follow hardware, usage and database growth, with every instance applying a change at the same commit.

## Requirements

Cost properties:

- **R1 Deterministic:** for the same call against the same state and cost schedule, every implementation, machine and version returns the same result, including its order, at the same cost; or stops at the same step with the same cost spent so far and the same error.
- **R2 Per call and additive:** a cost is attributable to exactly one call and summable across calls.
- **R3 Bounded pricing:** with a limited budget, pricing work must stay within that budget. Host-authorized unlimited estimation is outside this budget-bounded guarantee (D8). Write cost may depend on the call's arguments and on state the call reads anyway, never on a read made only for pricing (the no-probe rule). A read never looks anything up just to calculate its price: what each step costs is known from the request itself and from data the read has already fetched and paid for.
- **R4 Rounds up:** no caller obtains unbounded work for bounded cost, also per step: the work of each charged step is bounded by its charge, so combining sets is charged by their size and a scan by every term it steps over. Where cost is approximated, it is approximated upward: undercharging is an attack surface, overcharging only inefficiency.
- **R5 Defined, not measured:** cost counts logical work (rows, index terms, trie paths) as the reference execution performs it, never physical events (pages, cache hits, timing). A warm and a cold instance charge the same.
- **R6 No refunds:** operations are metered regardless of whether their effects reach disk, including execution that only changes in-memory branch state. Rolling back a frame or discarding a branch does not refund charges already incurred. Rollback and branch discard carry no charge: rolled-back operations have already paid for deferred work (trie paths, history, persistence) that is never performed, which exceeds the in-memory undo (D8). Commit carries no additional data-plane charge: record-induced work is prepaid by calls, while block/commit overhead is host-funded (D8).

Budget:

- **R7:** every call requires a budget, with no default. A call without one does not compile: the budget is a constructor argument of every operation (D6.4); a caller who wants no limit passes `Budget::Unlimited` explicitly, which D8 reserves for host-authorized estimation. Every step has a known charge, checked before the step runs: a write's planning steps (D2), a read's execution steps (D6). If a step's charge exceeds the remaining limited budget, that step is not performed and the call stops, with no writes and no partial results. `OutOfBudget{spent, required?}` reports work already performed. For writes, `required` is present only when planning has established the full representable cost; a read stops as it goes and reports none. D8 defines explicit unlimited planning for host-authorized estimation.

Coverage and reporting:

- **R8:** every read and write call (`create`, `get`, `patch`, `delete`, `query`, `count`) is metered for compute, and writes also for storage. So are the immutable-log calls (`immutable_data_append`, `immutable_data_get`, `immutable_data_range_of`, `immutable_data_rows_of`; D7).
- **R9:** every receipt reports the call's cost and the commit that priced it (`priced_at`). A read also reports the commit it read (`evaluated_at`). Details of cell and index data added and removed by write calls (D4) are opt-in per call on a transport, which keeps receipts small; every implementation must support the option and return the details when requested. An in-process API may return them with every receipt, as the [Rust API](golemdb-api.md#receipts) does.
- **R10:** every user record has a maximum deletion cost, computable at any time from its shape and the current schedule. Actual deletion may cost less.

Cost schedules:

- **R11:** a metering model (cost schedule) is code plus weights. The code (cost structure (D2), counting rules and expected weight names) is identified by a GolemDB code version and, if the host declares weights, a host code version; weights are committed data, one value per weight name: a price, or for host-declared weights also a host limit (D9). Several model versions can share the same code versions and differ only in weights.
- **R12:** weights are adjustable at runtime through admin writes, without upgrading GolemDB, taking effect only at a committed head boundary. Branch calls use the schedule captured at their base commit; calls without a branch capture the schedule at the current head on admission. This applies to reads of historical data as well as current data. Calls are never re-priced in flight (D9). A weight change can also be scheduled: installed as a new model version on the same code versions, it takes effect at an activation commit at least `#minActivationDelay` ahead (D9).

Conformance:

- **R13:** the reference execution is pinned by conformance vectors: a state, a call and a cost schedule, with the expected result, cost and stopping point, including calls that end in `OutOfBudget`. They cover writes and reads, and every implementation must pass them.

## Record Model

GolemDB holds three kinds of records ([design §4](golemdb-design.md#record-classes-and-the-reserved-catalogue)):

- **System records** (`#` record key prefix) hold GolemDB's own configuration and state, e.g. immutable deployment parameters (`#params`), and key bindings (`#recordKeys`).
System records are created at genesis and only GolemDB business logic is allowed to touch cells of these records. 
- **Admin records** (`@` record key prefix) are initially created at genesis. All changes need to go through GolemDB's admin (metering) API, which validates and stores them. 
GolemDB cost schedules and weights (`@meteringModel`, `@modelWeight`) which should be allowed to change through GolemDB's admin (metering) API.
- **User records** are managed by the application that uses GolemDB, for example Arkiv's entities and accounts. The write calls metered in D2 act on user records.

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
deletion costs. A host may impose stricter content requirements without GolemDB doing so.

The reverse mapping, record key → record ID, lives in the system record `#recordKeys`: one binding cell per live record, keyed by the 32-byte record key, holding the record ID.

- `create` adds the binding; `delete` removes it. The removed binding stays in history for the retention window.
- A deleted key can be re-created; it gets a fresh binding and a new record ID. A host that must prevent key reuse enforces it above GolemDB, for example through derived keys.
- "No record with key K" is a non-inclusion proof of the binding.
- `#recordKeys` grows with the number of live records. Per-record caps (D5) apply to user records only.

### The `#meta` Cell

`#meta` holds four counters over a record's user cells; its layout, maintenance and the
record completeness proofs it enables are in
[design §3](golemdb-design.md#record-shape-the-meta-cell). Metering needs it because a
`patch` reads only the cells it touches, and the no-probe rule (R3) forbids reading the
rest just to price or validate the call.

| Counter | Used for |
| --- | --- |
| cells | cell cap check ([D2](#d2-write-cost-model) step 5); deletion bound ([D5](#d5-record-shape-and-deletion-bound)) |
| indexed cells | indexed-cell cap check (D2 step 5); deletion bound (D5) |
| cell bytes, index bytes | host storage accounting: Arkiv's `extend` prices the bytes an entity keeps stored times the blocks added to its lifetime (goal 4) |

Sizes follow D4. GolemDB's own pricing does not use the byte counters. Maintaining
`#meta` is charged as a system cell operation (D2); reading it is an ordinary `get` of
the cell, alone or with the full record, and is metered as a read (D6). The database-wide counters that set the modeled trie depth are
separate (D3).

### The Index

Attribute cells are indexed; fields are not. The index is an inverted index, as in information
retrieval:

- A **term** is one cell name, type and value, such as `status = "active"`.
- A **posting** is one record ID filed under a term.
- A term's **posting list** is all record IDs under it. It is stored as a bitmap split into
  **bitmap containers**, each holding the 16-bit offsets of up to 2¹⁶ record IDs that share their
  upper 48 bits, kept together by a bitmap trie ([design glossary](golemdb-design.md#glossary)).

Writes join and leave terms (D2); queries look terms up, step through ranges of terms, and load and
combine posting lists (D6).

## D1. Where Metering Happens

Meets R1, R5.

Cost is computed over GolemDB's logical schema.
The logical schema includes cell rows, index terms, bitmap containers, trie paths. It does not consider lower level items such as concrete MDBX pages. Physical page-level work depends on each node's current physical history: open read transactions, compaction and configuration change which pages a write touches. Two honest instances might well disagree on those.

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
| 1. Handle | the branch handle is known, not consumed, its origin is still the head, and not sealed. Reads have their own order of checks ([D6](#the-read-call-frame)) | 0 | `HandleInvalid`, `Sealed` |
| 2. Admission | input form, including errors found while the request was built; reserved keys; key mode; cell names and value lengths against the genesis limits. Reads no record state | admission cost | `InvalidArgument`, `Reserved`, `KeyModeMismatch` |
| 3. Record state | the binding, then `#meta`, cells and index terms; the per-record caps | the record base (`read.key + write.record.{op}`) plus the core reads performed (D6.1) | `NotFound`, `AlreadyExists`, `CellNotFound`, cap violations |

Stage 1 comes first because it fixes the pricing snapshot (D9): without a valid handle or
snapshot, there is no schedule to charge with, so a call rejected there costs nothing (D8).
Validating a handle reads only the current head, a fixed-size row, not record state. The genesis
limits and key mode used in stage 2 are `#params` cells, fixed at genesis, so they count as
configuration rather than state. Within stage 3, the steps below fix the order.

Before each charged planning step, starting with admission and later the record base with its binding lookup,
check that its charge fits the remaining limited budget. If it does not, stop without
performing the step and return `OutOfBudget{spent, required: None}`. Each step must have
a known, bounded charge before execution; the calibration requirements below cover
verification of those charges, while the reference admission procedure below defines
admission limits. `spent` includes only completed charged steps. Equality fits:
exhausting the budget does not by itself fail a call if no further charge is needed.

1. **Plan** (reads only, no writes):
  1. Admission (stage 2): check the input's form, reserved keys, the key mode, cell names and value lengths, charging incrementally as defined below. No record state is read. Rejected input still incurs the admission cost performed before rejection.
   2. Read the key binding: the record base, `read.key + write.record.{op}`, charged together before the lookup. `create` fails with `AlreadyExists` if it exists; `patch` and `delete` fail with `NotFound` if it does not. A deleted record has no binding, so its key can be re-created.
   3. `patch` and `delete` only: read [`#meta`](#the-meta-cell) for the current counts (`read.cell`). A `create` starts from zero.
   4. `patch` and `delete`: read every touched cell (`read.cell` each). All operations: read every touched index term (`read.term` each). The results decide each operation and its bytes.
   5. Compute the resulting counts and check them against the per-record caps (D5).
    6. Compare the total cost (admission + record base + reads performed + planned writes) with the limited budget, if any. If it exceeds the budget, fail with `OutOfBudget{spent, required: Some(total)}`: `spent` = admission plus the record base and reads performed. No additional work is done merely to obtain `required` after an earlier budget abort. A total equal to the budget succeeds; `Unlimited` skips budget comparisons, not validation or checked arithmetic.
2. **Apply:** execute the planned writes, reusing the phase-1 reads. No metering or validation failure can occur here; only local storage faults remain, which are not cost questions.

A failure in the plan phase writes nothing, so it is charged no write cost (D8). A write is never partly applied.

The plan phase yields the call's cell and index operations, including the per-record system cells listed below. Host-funded commit overhead is separate (D8). The four cost components are:

```
record op cost       = admission cost + read.key + write.record.{op}
                     + Σ cell operation costs
                     + Σ index operation costs

admission cost       = write.admission.base
                     + entries inspected × write.admission.entry
                     + bytes validated × write.admission.byte

cell operation cost  = read.cell                               (only when read)
                     + write.cell.{create | update | delete}
                     + cell_trie_depth × write.cell_trie_update
                     + cell bytes written × write.cell_byte

index operation cost = read.term + write.index.{join | leave}
                     + index_trie_depth × write.index_trie_update
                     + write.index_term_create                 (only if the term is new)
                     + index bytes written × write.index_byte

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
Transport decoding and allocation before the GolemDB call remain host responsibilities.
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

Every planned cell or index mutation pays for one leaf's trie path, priced at the modeled depth (D3). Required reads remain charged even for no-ops. A touched cell's `read.cell` is charged only where a cell is read: by `patch` and `delete`, never by `create`.

**Planning reads are core reads.** Every read a write's planning performs, the key binding, `#meta`, the touched cells and the touched index terms, is charged with the core read steps of [D6.1](#d61-core-reads): `read.key`, `read.cell` and `read.term`, the same steps and weights a `get` pays. Planning returns nothing to a caller, so it charges no `read.byte`.

**Record base cost.** The record base is `read.key + write.record.{op}`, charged together before the binding lookup. `read.key` is the binding lookup in `#recordKeys`; `write.record.{op}` covers the remaining fixed per-record work that is not a cell operation. Input validation is charged separately as admission.

- `create`: increment the in-memory branch record ID counter. Persisting and merkleizing the final `#alloc` value is host-funded commit overhead (D8). A key collision fails with `AlreadyExists` (D8).
- `patch`: check the caps.
- `delete`: one seek to enumerate the record's cells.

This fixed work folds into one weight per operation with no depth term. The allocator's committed cell and trie update are accounted for separately as host-funded overhead (D8).

`write.record.{op}` must be calibrated excluding the admission work and the binding lookup, now
covered by their own weights; including either would charge it twice.

**System cell operations.** The system cells (Record Model) are charged as cell operations, like user cells:

| Record operation | System cell operations |
| --- | --- |
| `create` | create binding, create `#key`, create `#meta`; no reads, the record is new and the binding read is in the record base |
| `patch` | read `#meta` (`read.cell`), even for empty patches; update it only when a count changes |
| `delete` | delete binding (read in the record base), read and delete `#key`, read and delete `#meta` |

System cells have fixed-length encodings, so their byte terms are constants. They are not included in `#meta`'s counts or the receipt's user-cell counts (D4).

**Cell operations.** On `create`, every user cell is a cell create, without a read: the record is new. On `patch` and `delete`, a read of each touched user cell decides the operation:

| Read result and request | Cell operation | Bytes written | Bytes deleted |
| --- | --- | --- | --- |
| cell missing, value assigned | create | new cell | – |
| cell missing, `set` requested (keep the stored kind) | none: the patch fails with `CellNotFound` | – | – |
| cell present, different type, kind or value assigned | update | new cell | old cell |
| cell present, its current type, kind and value assigned | no-op: read only; no index operation | – | – |
| cell present, deletion requested | delete | – | old cell |
| cell missing, deletion requested | read only (`read.cell`); no index operation | – | – |

Bytes written are made live and charged (D4). Bytes deleted leave live state but stay in the change-set until pruned; they are reported, not charged.

- `write.cell.{op}` covers the cell row, its history entry and its change-set entry; the cell-trie path is the depth term.
- An old value causes three kinds of work:
  - **Change-set copy.** At commit, the pre-image goes into `CellChangeSet`, once per modified cell per commit, however often the branch touched it. The work grows with the old value's size, not with the number of earlier versions. The write byte weight pre-pays it: every byte is copied at most once, when it is overwritten or deleted, so it is charged once, when written (D4).
  - **History append.** At commit, the commit number is added to the cell's `CellHistory` bitmap, once per modified cell per commit. The bitmap grows with the cell's retained modifications; `write.cell.{op}` covers it as a fixed charge, calibrated for `#minRetention` (D4).
  - **Branch processing.** On every touch, planning reads the old value and the operation log captures it; a rollback copies it back. This work grows with the old value's size and recurs per touch. `read.cell` and `write.cell.{op}` cover it as fixed charges, calibrated up to `#maxBytesLen` (D6.1).
- A patch's `set` takes the kind from the cell read in step 4, so for `set` only the type or value can differ. Its failure on a missing cell is decided by a read the patch owes anyway (R3).
- A record `delete` performs a cell delete for every user cell.
- Assigning a cell its current value with its current kind is a no-op: it pays only for the reads that establish that nothing changes. A kind change alone is an update, since it joins or leaves the index. No cell write, index leave or join, or history entry follows.

**Index operations.** Leave the old term if the old cell was indexed; join the new term if the resulting cell is indexed, including attribute/field transitions. Each join or leave charges a term lookup, `read.term` (D6.1), even for the same term:

| Read result and request | Index operation | Charged |
| --- | --- | --- |
| term exists, record joins | join | `read.term + write.index.join` |
| term missing, record joins | create term, then join | `read.term + write.index_term_create + write.index.join` |
| record leaves, whether or not it is the last member | leave | `read.term + write.index.leave` |
| value or type changes | leave the old term, join the new one | both |

- `write.index.join` and `write.index.leave` cover the membership change, the bitmap container and the posting-list path; the index-trie path is the depth term.
- `write.index_term_create` covers both creating a term and its later removal: the term row and the leaf insert and removal. It has no depth term, because the join that follows and the eventual leave already pay for the leaf's path. Removing a term emptied by its last member costs nothing at that point, so every leave costs the same and deletion stays predictable.
- A join writes the term's bytes (D4); a leave deletes them.

**Common rules.**

- Every branch is decided by a read the operation owes anyway, so cost is known before any change is applied (R3).
- Compute and storage are summed, not multiplied: a trie path rewrite costs the same for a 4-byte or a 400-byte value, while writing a value is linear in its length.
- A batch, such as a host transaction, costs the sum of its record op costs. Reads are metered separately (D6).
- Repeated touches of the same record or cell are charged per touch. An implementation may merge the work, never the charge.
- These weights aggregate finer structural op classes, which remain the calibration basis (D10).
- Every receipt carries the call's ledger, its count per weight name ([D6.3](#cost-and-ledger), [D6.4](#d64-api-impact)); in process it is always present, a transport may make it opt-in per call (R9).

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

Trie work is deferred to commit and runs once over the branch's net changes, so a write's physical trie cost depends on the rest of the batch. Cost therefore counts **one modeled path rewrite per planned cell or index mutation**, including repeated touches of the same leaf, at a modeled depth. Write cost is computed from the call's arguments and the reads it owes anyway (D2), so a write can be refused before any change is applied.

In D2, these modeled path rewrites are the depth terms of every cell and index operation.

**Global counters.** This model requires GolemDB to track two database-wide counters, the cells `#liveCells` and `#indexTerms` of [`#alloc`](golemdb-design.md#alloc-recordid-1):

- **Live cells:** the number of `CellTrie` leaves, across all record classes: user cells, `#key` and `#meta` cells, bindings, root history, weights, and the counters themselves.
- **Distinct index terms:** the number of `IndexTrie` leaves.

Their only purpose is to determine the modeled trie depth. They differ from the per-record counts in [`#meta`](#the-meta-cell), which cover one record's user cells and serve caps, deletion bounds and the host. The counters are committed state, so every node derives the same depth, also after a restart. GolemDB updates them once per commit from the branch's net changes, as host-funded commit overhead (D8).

The modeled depth is derived from these counters by a pinned table. Tries are 16-ary, so depth grows as ⌈log₁₆ N⌉:

| Population N | Modeled depth |
| --- | --- |
| ≤ 16⁴ (65,536) | 4 |
| ≤ 16⁵ (1,048,576) | 5 |
| ≤ 16⁶ (16,777,216) | 6 |
| > 16⁶ | 7 |

Depth is capped at 7. ⌈log₁₆ N⌉ would reach 8 only above 16⁷ (≈268M) leaves in one trie, which the current architecture is very unlikely to reach.

- The cell trie uses the live-cell count; the index trie uses the distinct-term count. The posting-list trie keeps a constant modeled depth, at a low-cardinality envelope.
- Path-rewrite cost = `cell_trie_depth × write.cell_trie_update` or `index_trie_depth × write.index_trie_update`, using the counters at the branch base.
- The write already knows whether it adds or removes a cell or term, so counters need no extra read (R3).
- Database growth leaves the weights: they reflect cost per node only.
- The model over-charges repeated writes to one cell and writes sharing trie prefixes; deeper paths can be under-charged unless calibration provides sufficient upward margins (R4).

Modeled depths approximate typical path work, not maximum depth. Calibration must
verify sufficient pricing margins for path-depth variation, growth within a branch,
and posting-list updates. The depth cap limits charged depth, not supported database
size or actual trie depth. The [design's depth bounds](golemdb-design.md#depth-bounds-and-future-optimization-paths)
are 64 for cell/index tries and 12 for posting-list tries; these bounds and measured
workloads inform verification rather than implying that current weights already cover
every case.

## D4. Storage and Size Counting

Meets R8, R9.

**Pay once for every byte made live.** Overwritten and deleted values move into history rather than disappearing, so charging a value's bytes once, when written, pre-pays both copying them into history later and their residency there. Each byte is copied at most once, so the pre-payment is exact for bytes that are later overwritten or deleted, and an over-charge for bytes that never are: the safe direction (R4). Shrinking a value still costs the new value's bytes; deleting costs no bytes.

The write byte weights assume that every value is eventually copied into history. The
copy happens once, whatever the retention window; residency does not. The weights are
calibrated for residency of `#minRetention` commits, the minimum history every node of a
deployment keeps ([design §4 `#params`](golemdb-design.md#params-recordid-0)). A
deployment with a longer `#minRetention` therefore has higher weights; a node that
retains more history than `#minRetention` does so at its own cost. Either way, all
nodes of a deployment charge the same.

**Size per record, names included.** GolemDB stores cell names in full in every cell key and every index term key. Size is counted per record, never shared:

- A cell counts `8 + |name|` bytes for its key (ID prefix and name) and `1 + |value|` bytes for its value (type tag and value).
- An index entry counts `|name| + 2 + |value|` bytes: its term key, name ‖ `0x00` ‖ type tag ‖ value.
- [`#meta`](#the-meta-cell) keeps four counts per record on this basis: cells, cell bytes, indexed cells, and index bytes. They cover user cells only; system cells are fixed-size and charged separately (D2).
- When details are requested for a write call, its receipt must report cells created, updated and deleted; index joins, leaves and terms created; and cell and index bytes written and deleted, on this basis (R9). Terms created come from the index-term reads of D2 step 4, which planning performs anyway to price `write.index_term_create`; an implementation without metering may not report them yet. Deleted counts are reported, never refunded (R6).
- Counting per record overcounts popular index terms: the safe direction (R4).

## D5. Record Shape and Deletion Bound

Meets R10.

By D2, deleting a record costs:

```
delete cost = admission cost + read.key + write.record.delete
            + system cell deletes                        (binding, #key, #meta; fixed size)
            + cells         × (read.cell + write.cell.delete + cell_trie_depth  × write.cell_trie_update)
            + indexed cells × (read.term + write.index.leave + index_trie_depth × write.index_trie_update)
```

Every leave costs the same, whether or not it empties the term (D2), and deleted bytes were pre-paid at write (D4). A valid delete request carries a fixed-size record key and no cell changes, so its admission cost is fixed by the schedule. The total therefore depends only on the record's counts and is exact at current weights and depth.

- The cell and indexed-cell counts come from [`#meta`](#the-meta-cell), updated atomically with every count change; every successful patch pays for its read, and for its update when a count changes (D2).
- Stores counts, not cost: counts are exact and independent of weights and D3 depth. The maximum deletion cost is computed from them at current weights and depth. Actual deletion may cost less, for example after the database shrinks.
- System cells are not counted in `#meta`; their deletes are fixed-size system cell operations (D2).
- GolemDB's ceilings are deployment parameters in `#params`, fixed at genesis: `#maxCellNameLen`, `#maxStrLen`, `#maxBytesLen`, and the caps on cells and indexed cells per user record, `#maxRecordCells` and `#maxRecordIndexedCells`. System and admin records are exempt from the cell caps; they are never deleted, so they need no deletion bound.
- After every `create` and `patch`, the record's resulting counts must stay within the caps. The plan phase checks this (D2); a violation fails the call and writes nothing.
- The cell caps bound a record's maximum deletion cost; the length ceilings bound the size of each write.
- Caps are on counts and lengths rather than cost, so a weight increase cannot push existing records over a limit.

## D6. Read Cost Model

Meets R1, R3, R4, R5, R7, R8, R13.

Reads are counted, not modeled: nothing on the read path is deferred, so cost accumulates step by
step as the work happens. Every step has a known charge, checked before the step runs; the first
charge the remaining budget cannot pay stops the call without performing that step, with no
partial result and the cost spent so far (R7).

D6 has two parts with different users, plus the rules that turn both into a cost:

| Part | Covers | Used by |
| --- | --- | --- |
| [The read call frame](#the-read-call-frame) | the order of checks, the call base and admission of a read call | `get`, `query`, `count` |
| [D6.1 Core reads](#d61-core-reads) | point reads of one record's cells and of one index term | `get`, write planning (D2), query materialization, `id_of` / `key_of` |
| [D6.2 Query reads](#d62-query-reads) | query admission, term scans, posting lists, set combination, sort, paging | `query` and `count` only |
| [D6.3 Pricing formula and cost compilation](#d63-pricing-formula-and-cost-compilation) | the cost of each read call as a formula, and how an implementation accumulates it while executing | every read call |
| [D6.4 API impact](#d64-api-impact) | what read metering adds to the API: budgets, receipts, the ledger, errors | callers and hosts |

D6.1 is the part write metering depends on: its steps and weights are the ones D2 charges for
the reads a write's planning performs. D6.2 can change, or grow with new query features, without
touching the price of a write.

Rules for both parts:

- **Logical steps only.** The counted work is the reference execution, whether or not an
  implementation short-circuits it (D1, R5): a cached term, a warm node or a held result never
  reduces a charge.
- **Charges bound the work; algorithms are free.** Every read charge is defined in logical
  quantities: lookups, rows, terms, bitmap containers, bytes. Each is an upper bound on the work of
  any reasonable implementation. How an implementation performs a step (which set it walks, how it
  searches, whether it merges several sets at once) is its own choice and never changes the
  charge; weights are calibrated from the reference implementation (D10).
- **Reads touch flat tables, not trie paths.** A key resolution, a cell read and a term lookup are
  single lookups, with no depth term as in D2. The posting-list trie walk is covered by the bitmap
  container charge, at the constant posting-trie depth (D3).
- **Separate weights, equal starting values.** Steps that do similar work have separate weights
  and start with the same value; measurements may set them apart later. A key resolution and a
  term lookup are both single lookups in a flat table; a cell point read and a cell scan step both
  read one cell row.

### The Read Call Frame

What every read call, `get`, `query` and `count`, does around its reads. Write planning has its own
frame, D2's admission and record base, and charges none of this.

| Step | When | Weight |
| --- | --- | --- |
| Admission | once per call, per entry and per byte inspected | `read.admission.base`, `read.admission.entry`, `read.admission.byte` |
| Call base | once per call, when the storage snapshot is opened and its head read | `read.base` |

**Order of checks.** The first failure is returned; no later check runs.

1. **Handle**, `get` on a branch only: the handle is known, not consumed and its origin is still
   the head. A failure is `HandleInvalid` and costs 0 (D8).
2. **Not implemented**, `query` only: top-level parts not built yet (sort, limit, offset, cursor)
   are `NotImplemented` and cost 0.
3. **Admission** validates the whole request without reading state, charged incrementally, each
   part checked before its work, so an oversized request stops early with `OutOfBudget`.
4. **Snapshot**, charged `read.base`: one storage snapshot is opened, or for a branch the base
   snapshot the overlay sits on, and a requested commit is checked against its head
   (`CommitUnavailable`). Every state read uses that snapshot.
5. **Reads**: the core reads (D6.1), and for `query` and `count` the query reads (D6.2).

**Admission** has D2's three-part shape, with weights of its own that start at D2's values:
`read.admission.base` once per call, then entries and bytes:

| Request part | Entries | Bytes inspected |
| --- | --- | --- |
| a filter node: a leaf, an `And`, an `Or` or a `Not` | 1 | the leaf's field name |
| a leaf's operand | 1 per value: an `Eq` value, each `In` value, each bounded side of a `Range`, a `Prefix` string; none for `Exists` | the value's bytes |
| a projected name, a sort key | 1 each | the name |
| the record key of a `get` | none: fixed size, covered by the base | – |

Operands arrive valid: an `In`'s values are distinct and ascending, and every operand has one
indexable type, when the operand is built
([query proposal](query-proposal.md#ast-query-format)), so admission inspects them once each and
stays linear.

### D6.1 Core Reads

The point reads of one record and of one index term. They are the shared vocabulary of every
call that reads state: a `get` is made of them, a write's planning (D2 steps 2 to 4) is charged
with them, and a query materializes its records with them.

| Step | Reads | Weight |
| --- | --- | --- |
| Key resolution | the binding of one record key in `#recordKeys`, present or absent | `read.key` |
| Cell point read | one cell row by record and name, present or absent | `read.cell` |
| Cell scan step | one cell row of a full-record scan | `read.cell_scan` |
| Bytes returned | per byte of each cell returned to the caller, on the D4 basis | `read.byte` |
| Term lookup | one index term row, present or absent: whether the term exists, and its posting root | `read.term` |

- **Flat charges, calibrated for the maximum size.** A cell point read and a scan step are flat,
  calibrated for a value of the maximum size (`#maxStrLen`, `#maxBytesLen`), as D2's fixed
  per-cell charges are. A term lookup is flat, calibrated for the longest term key the genesis
  limits allow.
- **Bytes are charged after the read and before the copy.** A cell read yields the value's length;
  the cell's bytes are then charged as a step of their own, before the value is decoded and copied
  into the result. Only `str` and `bytes` vary; every other type has a fixed width.
- **The byte basis is D4's:** a returned cell counts `(8 + |name|) + (1 + |value|)`, the same
  basis as `#meta`'s `cell_bytes` and as `max_bytes` paging (D6.2). The two system cells a read
  returns have fixed sizes: `#key` 45 bytes, `#meta` 46 bytes.
- **Absent costs the lookup, not the bytes.** A key resolution, cell read or term lookup that finds
  nothing still pays its lookup; it returns no bytes.
- **Cell reads need a metering-aware method.** Reading a cell must expose its length before
  decoding or copying it, so that a large `bytes` value that would exceed the budget is detected
  without copying any of its bytes. MDBX supports this directly: a read returns a reference into
  the memory-mapped file with its length, and the value's overflow pages are only loaded when its
  bytes are touched. Today's `CellReader::get` decodes into an owned value at once and needs this
  amended method.

**Reading a record.** A record is read in one of three shapes; the same rules serve `get` and a
query's materialization (D6.2):

| Shape | Steps | Bytes returned |
| --- | --- | --- |
| projection `.only(names)` | one `read.cell` per distinct name, present or absent | the D4 size of each cell found |
| existence check `.only([])` | none beyond resolving the record | none |
| full record | one `read.cell` each of `#key` and `#meta`, which gives the record's exact counts; then one `read.cell_scan` per user cell | `cell_bytes` from `#meta`, plus 45 for `#key` and 46 for `#meta` |

A full record's charge is known from `#meta` before its user cells are scanned, so the scan is
charged as one step. A projection may name reserved cells, such as `#meta` alone
([API `#meta`](golemdb-api.md#meta)); they are point reads like any other.

**`get`** is the read call frame (admission and the call base), one key resolution, and then the
record in its requested shape (D6.3). A missing record costs admission, the call base and the key resolution, and returns
`NotFound`.

**Write planning** charges its reads with these steps (D2): the key binding is one `read.key`;
`#meta` and every touched cell of a `patch` or `delete` one `read.cell` each; every index term a
join or leave touches one `read.term`. Planning returns nothing to a caller, so it charges no
`read.byte`; processing an old value is covered by D2's fixed per-cell charges instead.

**Branch reads cost what committed reads cost.** A `get` on a branch looks each cell up in the
branch's in-memory overlay first and falls back to the branch's base snapshot; a full-record read
merges both. Where a cell comes from is a physical detail, like a warm or cold cache (R5), so a
branch read is charged the same steps and weights as the committed read, calibrated for an overlay
miss followed by a base read (R4). Its `#meta` reflects the branch's own writes, and its cost
schedule is the one captured at the branch's base commit (R12). Write planning reads the branch
the same way. Queries read committed state only, so `get` is the only branch read a caller makes.

**`id_of` and `key_of`**, to be specified with the proof API, are core reads with a flat charge:
`id_of` is one key resolution (`read.key`), `key_of` one cell read of `#key` (`read.cell`).

**Historical reads** (deferred). Resolving an item at a past commit is a flat surcharge per item,
independent of how far back.

### D6.2 Query Reads

The steps of `query` and `count` beyond the read call frame and the core reads. Their order is the
reference execution: the execution rules of the
[query proposal](query-proposal.md#execution-rules-normative).

| Step | When | Weight |
| --- | --- | --- |
| Term stepped | `Range`, `Prefix`, `Exists`: per further index entry read in a scan | `read.term_step` |
| Bitmap container loaded | per bitmap container of a term's posting list | `read.container` |
| Set combination | per bitmap container processed by a union, intersection or difference | `read.set_container` |
| Sort comparison | modeled, from the match count and the number of sort terms | `read.sort_compare` (deferred) |

- **Leaves.** An `Eq` is one `read.term`, present or absent. An `In` is one `read.term` per value,
  in the values' order. Every term found then pays for its posting list, one `read.container` per
  bitmap container loaded.
- **Scans.** `Range`, `Prefix` and `Exists` step through one `(name, type)` run in ascending key
  order:
  - Positioning the cursor reads the first entry and is one `read.term`; every further entry read
    is one `read.term_step`, including the entry that ends the scan: past the end of the range, past
    the end of the run, or at the end of the table. A scan over `k` terms therefore costs
    `read.term + k × read.term_step`, plus their containers; a scan with no term in it costs one
    `read.term`, like an absent `Eq`.
  - `Range` starts at its lower bound; an `Excluded` lower bound that matches an existing term
    costs one more step, for the entry read and skipped. Two `Unbounded` sides scan the whole run,
    as `Exists` does. Bounds that admit no value read nothing.
  - `Prefix` starts at `name ‖ 0x00 ‖ tag ‖ prefix` and ends at the first entry that does not
    start with it. The empty prefix scans the whole `str` run.

  So scans over attributes with many distinct values pay for their width.
- **Containers are charged one at a time, as they are loaded.** A posting root does not record
  how many containers it holds, so the charge-before-step rule requires loading them one at a
  time, walking the posting-list trie. Today's `bitmap_at` loads all of a term's containers at
  once and needs a container-by-container variant.
- **The containers of a set.** Every set a query holds, a posting list or the result of a
  combination, is a set of record IDs. Its **container count** `C(s)` is the number of distinct
  upper 48-bit values among its members: the bitmap containers it occupies. It is a function of
  the set alone, so every node derives the same count for the same result, whatever its
  representation.
- **Combining sets** is charged one `read.set_container` per container of every set it takes as
  input, the same rule for all three combinations:
  - **union:** an `Or`, an `In` and a scan each combine their inputs in **one** union, charged per
    container of every input; its order and algorithm are free. An `Or`'s inputs are its children's
    sets, empty ones included; an `In`'s and a scan's are the posting lists of the terms found. A
    union of fewer than two inputs combines nothing and is not charged;
  - **intersection:** each further positive child of an `And` is intersected with the running set,
    charged `C(running) + C(child)`;
  - **difference:** each `Not` child of an `And` is subtracted from the running set, charged
    `C(running) + C(child)`.

  Any merge of sets by container visits at most the containers of its inputs, so this bounds the
  work of every roaring implementation (R4). The inputs' containers were already loaded and
  charged, so a combination costs at most about as much again. Record IDs are dense, so the
  number of distinct containers is bounded by the size of the database.
- **`And` runs in the written order.** The running set starts with the first positive child; each
  further child is evaluated and charged only when its turn comes, a positive child intersected,
  a `Not` child subtracted. As soon as the running set is empty, the remaining children are
  neither evaluated nor charged. An implementation must not reorder the children, for example by
  size, nor load them in advance.
- **`Or`** evaluates every child, in the written order, with no early exit, and unions them.
- **`Not`** is allowed only inside an `And`, after a positive child. A standalone `Not` needs the
  set of all live records (P08) and returns `NotImplemented` until then.
- **No early exit on the match set.** Every query builds its whole match set, sorted or not, so
  `total_matched` is always free and the charge does not depend on where a page falls.
- **`count`** runs admission, the call base, the leaves and the combinations only: it materializes
  nothing, and the size of the match set is free.
- **Sort** (deferred). Sorting fetches sort values with core cell reads (`read.cell` plus
  `read.byte` per value fetched) and charges comparisons as a model rather than a count, so the
  choice of algorithm stays out of the receipt: `S × N × ceil_log2(N)` comparisons for `N`
  matched records and `S` sort terms, with `ceil_log2(0) = ceil_log2(1) = 0`. This overestimates
  `⌈N log₂ N⌉ × S` slightly and is exact in integers. Which records need a deeper level's value is
  fixed by the data ([design §12](golemdb-design.md#typed-sort-terms-and-multi-key-sorts)).
- **Paging has no charge of its own.** `offset`, `limit` and `max_bytes` work on the match set
  already built, and paid for, in the same call. A page ends at `limit` records or at `max_bytes`,
  whichever comes first, with at least one record
  ([query proposal](query-proposal.md#execution-rules-normative), rule 9); bytes are counted on the
  D6.1 basis. Calls are stateless, so every page builds and pays for the whole match set again, and
  with `sort` the whole sort: a deep `offset` is not cheap, which the cursor (design D11) is to
  address.
- **Materialization** reads the page's records only, in page order, as core reads (D6.1), in the
  requested shape. The result carries each record's key, so with a projection or an empty one,
  `#key` is read first, one `read.cell` with its 45 bytes, unless the projection names it; a full
  record includes it already. With no projection, a record that would
  not fit `max_bytes` is detected from its `#meta` before its cells are scanned; with a projection,
  its size is known only after the reads, which are charged, and the record is left out of the
  page.
- **Bulk deletion**, such as a host's expiry purge, uses ordinary reads: Arkiv's purge queries one
  equality term of its expiry attribute in record-ID order with an empty projection and a limit,
  and deletes record by record under its own budget
  ([tokenomics](arkiv-tokenomics.md#purge-budget)). It reads the parent block's committed state
  once per expiry block, since queries do not see a branch's deletes. No special scan is provided.

**Example.** `And[ Eq status="open", Range price [10, 20), Not Eq flagged=true ]`, where
`status="open"` has 3 containers, the price range covers two terms with 2 and 1 containers, and
`flagged=true` has 1:

| Step, in written order | Charges |
| --- | --- |
| `Eq status="open"`: lookup, load | 1 `read.term`, 3 `read.container`; running set: 3 containers |
| `Range price`: seek, two terms and the entry ending the scan, load | 1 `read.term`, 2 `read.term_step`, 3 `read.container` |
| union of the range's two lists, 2 + 1 containers | 3 `read.set_container`; range result: say 2 containers |
| intersect the running set (3) with the range result (2) | 5 `read.set_container`; running set: say 1 container |
| `Not Eq flagged=true`: lookup, load | 1 `read.term`, 1 `read.container` |
| difference: running set (1) minus child (1) | 2 `read.set_container` |

Had the intersection left the running set empty, the last two rows would not happen: the `Not`
child would be neither loaded nor charged.

**Implementation notes** (not normative). Loading a container today recomputes its hash and
re-encodes it canonically to compare, on every read, and query sets are rebuilt as a
`RoaringTreemap` record ID by record ID. Hash and canonical checks matter where bytes enter the
node (sync, import) or before they are hashed; rows a node wrote itself are canonical by
construction, so committed reads can skip both, with corruption covered by an audit. Keeping query
sets as a sorted list of containers, combined container by container with the library's
per-container operations, avoids the rebuild, gives `C(s)` as the list's length, and makes the
charging rules a literal description of the work.

### D6.3 Pricing Formula and Cost Compilation

#### Cost and ledger

A call performs steps, and every step is charged with one weight of the pricing snapshot. Counting
how often each weight was charged gives the call's **ledger**: for example 1 × `read.key`,
3 × `read.cell`, 200 × `read.byte`. The cost is then each count times its weight, added up:

```
cost = Σ over the weights the call charged:  count × weight
     = 1 × read.key + 3 × read.cell + 200 × read.byte + …
```

Modeled terms are counts like any other: a write's trie paths count `depth` per mutation under
`write.cell_trie_update`, a sort counts its modeled comparisons under `read.sort_compare`.

The ledger is what makes a cost checkable rather than just a number:

- **Receipts** carry it (D6.4), so anyone holding the pricing snapshot can recompute the cost and
  see what it was spent on, for example containers or bytes.
- **Conformance vectors** pin the ledger as `steps`, not only the total: where two implementations
  disagree, the ledger shows which step they count differently, and one vector serves any weight
  set.
- **Implementations** only count steps; the cost follows from the counts, so no step's price is
  computed in two places.

The formulas below are the same rule written out per call: each term is a count times a weight.

#### Read formulas

Notation: `A(n, b)` is admission over `n` entries and `b` bytes inspected; `KEY` and `META` are
the D4 sizes of the two system cells a read returns (D6.1),

```
A(n, b) = read.admission.base + n × read.admission.entry + b × read.admission.byte
KEY     = (8 + |"#key"|)  + (1 + 32) = 45                             record ID and name; tag and record key
META    = (8 + |"#meta"|) + (1 + 32) = 46                             record ID and name; tag and four u64 counts
```

**`get`**:

```
cost_get    = A(names, name bytes) + read.base + read.key + cost_record
cost_record = 0                                                       record not found
            | 0                                                       existence check
            | Σ over cell names n:  read.cell + size(n) × read.byte    projection; size 0 if absent
            | 2 × read.cell + cells × read.cell_scan
              + (cell_bytes + KEY + META) × read.byte                  full record: #key, #meta, then the scan
```

**`count`** and **`query`**:

```
cost_count = A(filter entries, bytes) + read.base + cost_filter(filter)
cost_query = A(filter, projection and sort entries, bytes) + read.base + cost_filter(filter)
           + sort + cost_page
cost_page  = Σ over the page's records r:
               read.cell + KEY × read.byte + cost_record(r)           existence check, or projection without #key
             | cost_record(r)                                         projection naming #key, or full record
```

A query finds records by record ID, through the index, but returns each with its record key, so
`cost_page` reads each record's `#key` cell. A full record or a projection naming `#key` reads it
within `cost_record`; `get` needs no such read, since its caller supplies the key.

`cost_filter` is the filter's cost, evaluated depth-first in the written order:

```
cost_filter(Eq t)        = read.term + C(t) × read.container          C = 0 if t is absent
cost_filter(In t₁…t_k)   = k × read.term + Σᵢ C(tᵢ) × read.container + U(the tᵢ found)
cost_filter(Range, k terms)                                           also Prefix and Exists: a scan
                         = read.term + k × read.term_step             +1 step for a matching Excluded lower bound
                           + Σᵢ C(tᵢ) × read.container + U(t₁…t_k)   every scanned term is found
cost_filter(Or c₁…c_k)   = Σᵢ cost_filter(cᵢ) + U(c₁…c_k)
cost_filter(And c₁…c_k)  = Σᵢ₌₁…ₘ cost_filter(cᵢ) + I(c₁…c_m)        m: the children evaluated, see below
U(s₁…s_k)                = Σᵢ C(sᵢ) × read.set_container if k ≥ 2, else 0
                                                                      U: the union of k sets, charged per input container
I(s₁…s_m)                = Σᵢ₌₂…ₘ (C(rᵢ₋₁) + C(sᵢ)) × read.set_container
                                                                      I: the intersection of m sets, charged per container of
                                                                      the running set and of each further set
```

`C(s)` is the container count of a set (D6.2). In an `And`, `r₁ = s₁` and `rᵢ = rᵢ₋₁ ∩ sᵢ` is the
running set after the `i`-th child; a `Not` child subtracts instead, `rᵢ = rᵢ₋₁ − sᵢ`, and is charged
the same. `m` is the first `i` whose running set `rᵢ` is empty, or `k` if none is: the children after
it are neither evaluated nor charged. The page and `sort` follow D6.2.

**Writes** keep D2's formula; their planning reads are the core reads above.

#### Compiling the cost while executing

An implementation accumulates the cost with a **meter**, one per call, created after the handle
stage and before admission:

```rust
struct Meter<'s> {
    schedule: &'s PricingSnapshot,   // captured once (D9); priced_at
    budget: Budget,                  // Limited(remaining) or Unlimited
    spent: u64,
    ledger: Ledger,                  // count per weight name
}

impl Meter<'_> {
    /// Charge `count × weight` before the step it pays for.
    fn charge(&mut self, weight: WeightName, count: u64) -> Result<(), OutOfBudget> {
        let price = self.schedule.weight(weight).ok_or(OutOfBudget::UNPRICED)?;   // D9
        let cost = price.checked_mul(count).ok_or(OutOfBudget::OVERFLOW)?;       // D8
        let spent = self.spent.checked_add(cost).ok_or(OutOfBudget::OVERFLOW)?;
        if let Budget::Limited(limit) = self.budget {
            if spent > limit { return Err(OutOfBudget { required: None }); }   // equal fits
        }
        self.spent = spent;
        self.ledger.add(weight, count);
        Ok(())
    }
}
```

Rules for using it:

1. **Charge, then perform.** Every step calls `charge` before it runs; a step whose charge fails
   is not performed. The call then returns `OutOfBudget { required: None }` with the receipt of
   what was spent, and drops every partial result.
2. **The charge unit is normative.** Where a charge is taken one unit at a time or for a batch
   decides where a budget stops, so it is part of the reference execution:

   | Step | Charged |
   | --- | --- |
   | admission | per part as inspected: the base, then each entry with its bytes |
   | `read.base` | once, before the snapshot is opened |
   | `read.key`, `read.term`, `read.cell` | once per lookup, before it |
   | `read.term_step` | once per entry, before the cursor steps |
   | `read.container` | once per container, before it is loaded |
   | `read.set_container` | once per combination, for all its input containers, before it runs |
   | `read.byte` | once per cell of a projection, for its D4 size, after the read and before the copy |
   | full-record scan | once per record, after the `#key` and `#meta` point reads and before the scan: all scan steps and the bytes of the whole record, `#key` and `#meta` included, from `#meta`'s counts |
   | `read.sort_compare` | once per sort, before it runs |

   A batch whose charge does not fit stops before any of its units.
3. **Counts come from the request or from data already paid for.** A combination's input sizes,
   a record's counts from `#meta`, a value's length from its read: never from a read made only to
   price a step (R3).
4. **`Unlimited` meters too.** It skips the comparison only: the ledger, checked arithmetic and
   unpriced weights apply as for a limited budget (D8).
5. **One meter per call, threaded through.** The meter passes through admission, planning and
   evaluation, and through the storage seams that load containers and read cells. The evaluator
   holds it, not the storage layer: storage exposes per-container loading and length-first cell
   reads, and the evaluator charges around them.
6. **The receipt is built from the meter**, on success and on every failure after the handle
   stage: `cost = spent`, the ledger, `priced_at` from the schedule, and for reads the
   `evaluated_at` of the snapshot, if one was opened.

### D6.4 API Impact

What read metering adds to the [API](golemdb-api.md). It extends the record calls' `Metered<T>`
shape, which is not on `main` yet ([api-priorities](api-priorities.md#priority-1-fix-the-shape-of-the-record-calls-and-add-meta)),
to every read.

**Every read returns a receipt.** `get`, `query` and `count`, and the immutable-data reads (D7),
return `Metered<T>`: their result together with a receipt, on success and on failure.

```rust
fn get(&self, target: ReadTarget, op: RecordOp<op::Get>) -> Metered<Record>;
fn query(&self, commit: Option<CommitId>, op: QueryOp) -> Metered<QueryPage>;
fn count(&self, commit: Option<CommitId>, op: CountOp) -> Metered<u64>;
```

**Budgets are constructor arguments,** with no default: `RecordOp::get(k, budget)`,
`QueryOp::filter(f, budget)`, `CountOp::filter(f, budget)`. An operation without one does not
compile (R7). Today's `QueryOp::from_filter(f).budget(b)` and `CountOp::filter(f).budget(b)` take
an optional step that the call ignores.

**The receipt gains the ledger and the evaluated commit:**

```rust
#[non_exhaustive]
pub struct Receipt {
    pub cost: u64,
    pub priced_at: Option<CommitId>,      // the pricing snapshot (D9)
    pub evaluated_at: Option<CommitId>,   // reads: the snapshot read; None for writes and early rejections
    pub details: Details,                 // writes' effects (D4); zero for reads
    pub ledger: Ledger,                   // count per weight name (D6.3)
}

/// Counts per weight name, in name order. `cost == Σ count × weight` under the snapshot
/// named by `priced_at`.
pub struct Ledger(BTreeMap<WeightName, u64>);
```

- **The ledger is keyed by weight name**, not a struct with a field per step: the weight list is
  model code, versioned by the GolemDB code version `G` (D9), so a new step is a new name, never
  a breaking change to the type. In process it is always present; a transport may make it opt-in
  (R9).
- **`evaluated_at` moves into the receipt.** A read can fail after its snapshot is opened, for
  example with `OutOfBudget`, and the caller still needs to know which state it was charged
  against. `QueryPage::evaluated_at` then duplicates it and can be dropped when `Metered` lands.
- **`priced_at` ≤ `evaluated_at`.** The pricing snapshot is captured at admission, from the head
  the in-memory schedule belongs to (D9); the storage snapshot is opened after admission (QR5). A
  commit landing in between makes `evaluated_at` the later head; the call is priced, consistently,
  by the earlier one. Inside a host's block execution the two are equal, since the branch base
  does not move.

**Costs and receipts by outcome:**

| Outcome | `cost` | `priced_at` | `evaluated_at` |
| --- | --- | --- | --- |
| invalid branch handle (`get` on a branch) | 0 | `None` | `None` |
| top-level part not implemented (`NotImplemented`) | 0 | `None` | `None` |
| admission error (`InvalidArgument`, `InvalidQuery`, `LimitExceeded`) | admission work done | head at admission | `None` |
| `CommitUnavailable` | admission + `read.base` | head at admission | the snapshot's head |
| `NotFound` (`get`) | admission + `read.base` + `read.key` | head at admission, or the branch base | the snapshot read |
| `OutOfBudget { required: None }` | the steps performed | as above | the snapshot read, if opened |
| success | the full cost | as above | the snapshot read |

For a `get` on a branch, "the snapshot read" is the branch's base commit: its overlay sits on that
committed state, and `priced_at` names the same commit.

**Errors.** `OutOfBudget { required: Option<u64> }` is new in `ApiError`; a read always reports
`required: None`, since it stops as it goes (D8). `LimitExceeded` is new for the query caps.
Neither exists on `main` yet.

**Host use.** Arkiv converts `cost` into gas for the owner `get` behind every patch, extend and
delete, and charges its purge queries against `purge_budget`
([tokenomics](arkiv-tokenomics.md#purge-budget)); both depend on these receipts.

### Conformance Vectors

R13's vectors pin results, costs and stopping points. Each vector contains:

| Part | Content |
| --- | --- |
| `id`, `description`, `covers` | a stable name, one line of intent, the rules it checks |
| `genesis` | the deployment parameters it depends on: length limits, per-record caps, query caps |
| `schedule` | a complete weight set, and the trie counters the modeled depth comes from |
| `setup` | ordered calls that build the state, so record IDs and index contents follow from the spec, not from one engine's storage |
| `call` | the call under test: kind, target, operation and budget |
| `expect` | `ok` with the result, cost, `priced_at` and `evaluated_at`, or `error` with the kind, the cost spent and `required` if any |
| `steps` | the expected ledger: the count of each weight name, as the receipt reports it (D6.4) |

- **Step counts say where two engines disagree;** the cost alone says only that they do. They also
  make a vector reusable under other weights.
- **A test schedule with distinct weights** makes a wrong count visible in the total cost, and
  keeps two counting errors from cancelling out.
- **The stopping point** of an `OutOfBudget` vector is given by its `steps`: only the steps
  performed. A pair of vectors pins it, one with exactly enough budget and one with one unit less.
- **Core reads first.** Vectors for D6.1 (`get` in every shape, and the planning reads of `create`,
  `patch` and `delete`) come before query vectors, since write metering depends on them.

The file format is shared with the encoding vectors (design D09) and decided with the conformance
strategy (CHANGES S13); a common choice, as in Ethereum's test suites, is hand-written YAML or code
generating JSON fixtures. The files live in `conformance/vectors/metering/`.

### Read Cost Items

Every moving part of the read cost model, decided or not, by part: the foundations, the core reads
write metering depends on, the query reads, and the API. A working list towards resolving finding
F1: once every item is settled, the rules above carry its content and the list is removed.

| # | Part | Item | Status | Current view |
| --- | --- | --- | --- | --- |
| 1 | all | Charge checked before each step; the call stops at the first that does not fit | decided | R7; no partial results |
| 2 | all | Order of checks | decided | `NotImplemented` first, then admission, then snapshot and commit check, then reads |
| 3 | all | Step list and weights | proposed | [D6.1](#d61-core-reads) and [D6.2](#d62-query-reads) tables; separate weights, similar steps start equal |
| 22 | all | Ledger and cost as ledger × weights | proposed | [D6.3](#d63-pricing-formula-and-cost-compilation); ledger keyed by weight name |
| 23 | all | Charge unit per step | proposed | [D6.3](#compiling-the-cost-while-executing) rule 2 |
| 24 | core | Core reads shared with write planning | decided (2026-10-20) | D2 charges `read.key`, `read.cell`, `read.term` for its planning reads; no `read.byte` |
| 10 | core | `get` | proposed | admission, call base, key resolution, then the record's shape |
| 8 | core | Record shapes and byte basis | proposed | projection, existence check, full record from `#meta`; D4 basis, `#key` 45 and `#meta` 46 bytes |
| 16 | core | Branch reads | proposed | the same steps and weights as a committed read, calibrated for an overlay miss (R4, R5) |
| 20 | core | Historical reads | deferred | flat surcharge per item |
| 4 | query | Unit for combining sets | decided | per bitmap container, a logical unit (D1) |
| 25 | query | Container count of a computed set | proposed | distinct upper 48-bit values among its members |
| 5 | query | Equality lookup, including an absent term | proposed | one `read.term`, present or absent, plus the containers of a term found |
| 6 | query | `In` | decided | one term read per value; values distinct and ascending by construction |
| 7 | query | Scans: `Range`, `Prefix`, `Exists` | proposed | `read.term` plus one `read.term_step` per further entry read, including the one ending the scan; two unbounded sides and `Exists` scan the whole run |
| 26 | query | Union of fewer than two inputs | proposed | not charged |
| 9 | all | Admission for reads | proposed | [read call frame](#the-read-call-frame): D2's three-part shape, own weights starting at D2's values; one entry per filter node, operand value, projected name and sort key; operands valid by construction |
| 11 | query | Purge read: equality, record-ID order, empty projection, limit | decided shape | charges follow from items 5 and 8 |
| 12 | query | `And` | decided | written order; early exit when the running set is empty, remaining children not charged; intersection and difference per container of both sets |
| 13 | query | `Or`, `Not` inside `And` | decided | `Or` evaluates every child, one union charged per container of every input; standalone `Not` waits for P08 |
| 14 | query | `count` | decided | admission, call base, leaves and combinations only; no materialization |
| 27 | query | No early exit on the match set | proposed | every query builds its whole match set; `total_matched` free |
| 15 | query | `limit`, `offset`, `max_bytes` | proposed | no charge of their own; page ends at `limit` or `max_bytes`, at least one record; caps as defaults |
| 18 | query | Caps | proposed | depth 16 (ceiling 64), records per page 1,000 (10,000), bytes per page 160 MiB (320 MiB), sort keys 4 (16); `#maxQueryBytes` at least the largest record; [query proposal](query-proposal.md#caps) |
| 21 | query | Container size | open | a container holds 1 to 65,536 record IDs, as an array or an 8 KiB bitmap; a flat per-container weight overcharges sparse posting lists. Measure loading and combining sparse against dense containers; if the gap is large, add a charge per serialized container byte, which the pinned Roaring encoding (D09) makes deterministic |
| 19 | query | Sort | deferred | `S × N × ceil_log2(N)` modeled comparisons, `ceil_log2(0) = ceil_log2(1) = 0`; fetches as core cell reads |
| 28 | API | Receipts for reads | proposed | [D6.4](#d64-api-impact): `Metered<T>` for every read, ledger and `evaluated_at` in the receipt, `priced_at ≤ evaluated_at` |
| 17 | all | Conformance vector format | proposed | [Conformance Vectors](#conformance-vectors): content fixed here, file format shared with the encoding vectors (S13) |

## D7. Immutable-Log Metering

Meets R2, R5, R7, R8.

The immutable log is GolemDB's second storage structure: append-only, ordinal-addressed
segments attached to commits ([design §11](golemdb-design.md#11-commit-immutable-data-segments)).
It shares nothing with D2: an append reads no state and touches no trie, index or cell
history. Each append and read is attributable to one call, so it is metered per call,
unlike commit overhead (D8).

```
append cost     = write.log_append.base + row bytes × write.log_append.byte
read cost       = rows read × read.log_row + bytes read × read.byte
```

- Row bytes are the sum of the row's column lengths, before compression (R5).
- The design sets no row-size cap: with positive weights an append's size is bounded by
  its budget, with zero weights by the host.
- `immutable_data_append` checks its cost against the budget before staging the row; if
  it does not fit, it fails with `OutOfBudget` and stages nothing. Staged rows that never
  commit, for example because the sealed branch loses the commit race, are not refunded (R6).
- `write.log_append.byte` covers writing the row and its residency in the segment's shards until
  pruned. Like the write byte weights, it is calibrated for `#minRetention` (D4); a
  pruning strategy that keeps a segment longer does so at the node's own cost.
- `immutable_data_get` reads one row, `immutable_data_rows_of` the commit's whole run;
  `immutable_data_range_of` reads one system-segment row and no segment bytes.
- Every immutable-log call returns its result together with a receipt, like the record
  calls.
- Optional per-segment row keys ([design §11](golemdb-design.md#row-keys)) add one key-index
  write to each keyed append, with no uniqueness check, and one key resolution to each read by
  key. Their weights follow once the index layout is specified.

**Who pays is the host's decision.** A deployment has two options:

1. **Positive weights.** GolemDB reports the cost; the host passes it on to its clients
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
  | Key failure (`AlreadyExists`, `NotFound`) | Admission cost + the record base (`read.key + write.record.{op}`) |
  | Later check (cap exceeded, invalid value, `CellNotFound`) | Admission cost + the record base + the core cell and term reads performed (D6.1) |
  | `OutOfBudget{spent, required?}` | `spent`: completed charged planning steps, never more than a limited budget; `required` is present only after the full cost is established |

  The key-failure and later-check rows assume the preceding charged steps fit the budget;
  otherwise the call stops earlier with `OutOfBudget`. If the remaining budget after
  admission is below the record base, no binding lookup is performed; `spent` is the admission
  cost and `required` is `None`. `write.record.{op}` includes work a failed
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
([design](golemdb-design.md#genesis-and-roots-as-cells)). This applies to every commit,
including empty and admin commits; it does not depend on collecting record-call charges.

Record-call charges still cover their own cell and index mutations, per-record system
cells, and associated deferred history and trie work. Host funding does not make that
work free merely because it is performed at commit. `write.record.create` pays for in-memory
ID allocation, not the final committed allocator update.

For Arkiv, this overhead belongs to the host's block-processing resource budget. The
host controls commit admission and frequency and bears the overhead even for an empty
block. How it funds or recovers that cost is host policy, not part of GolemDB's
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
  in GolemDB cost units. Planning can abort early; only a successful plan is applied.
  The operation is charged its total cost once, with planning charges already included,
  or its `spent` cost on failure. A caller cannot select unlimited consensus execution
  by requesting an estimate.
2. **`eth_estimateGas` service.** Arkiv may supply `Unlimited` to calculate the full
  GolemDB write cost without applying the plan. This is a host-authorized estimation
  facility, not a promise of free or budget-bounded work by GolemDB. Arkiv remains
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

**Head is authoritative.** A **pricing snapshot** is the active model version
together with its complete weight set, host weights included, as committed at a given head; every call computes
its costs from exactly one snapshot, named by `priced_at`. GolemDB always knows its current committed `commitNr` from
the [Superblock head](golemdb-design.md#the-superblock). It holds the active model and
its complete weight set in memory as a snapshot associated with that committed state.
At startup or whenever this snapshot is unavailable, reconstruct it from the committed
`@meteringModel` and `@modelWeight` records at head: select the greatest model version
whose activation commit is at or before head, validate its weight set, and load it.
An active model whose `G` or `H` this node does not run, or malformed weights, must prevent serving priced calls, not
silently fall back to an older schedule; missing weights are unpriced (below). Cache reconstruction does not change a call's
logical charge.

**Refresh at commit boundaries.** Whenever head advances, check model activation and
committed weight changes, then publish the corresponding head and pricing snapshot
together before admitting calls against that head. Never expose a new head with stale
weights or new weights with the old head. Uncommitted admin writes do not modify the
active snapshot. After recovery or rewind, rebuild or revalidate the snapshot against
the restored committed state; a commit number alone is not sufficient cache identity
if that number can be reused after a rewind.

- **Model = code + weights.** The code is the cost structure, counting rules, byte definitions and expected weight names, identified by a GolemDB code version `G`. D2's structure, D3's depth table and D4's counting are code changes: a new `G`. A host that declares weights adds a host code version `H`: its own list of weight names. A **model version** names its `(G, H)` and its activation commit `A` ([design §4](golemdb-design.md#meteringmodel-recordid-32)).
- **Weights = data.** One `u64` per weight name per model version, stored in `@modelWeight` and versioned by GolemDB's own history.
- **Install, then activate.** A new model version is installed with its `(G, H)`, its complete weight set and `A ≥ head + 1 + #minActivationDelay`. At most one is pending. Nodes not yet running code for its `G` and `H` cannot know its weight names, so install checks only what every node can: the model version is newer, `A` is within bounds, the cells are well-formed. Typos are the admin tool's job: it runs the new code and must reject unknown or missing weight names before it sends the install. Nodes not running code for its `G` and `H` halt at `A` ("upgrade required"). A model version with the active `(G, H)` is **weights-only**: every node already runs its code, so it activates without an upgrade. This is how a weight change is announced ahead of time.
- **Host-declared weights.** When opening the database, a host passes its host code version `H` and that model's weight names. Host weight names start with the fixed prefix `host.`, such as `host.storage_price`; a host may add its own text after it, such as `host.arkiv.storage_price`. GolemDB's own names never start with `host.`, so the two cannot collide. A host weight may be a price or a host limit: Arkiv, for example, keeps protocol limits such as its maximum entity lifetime there, to reuse the same validation, scheduling and activation. GolemDB treats host weights like its own: it validates them, stores them in `@modelWeight`, activates them and exposes them in the pricing snapshot. GolemDB never uses them in its own pricing, nor enforces host limits. The host reads them from the snapshot its calls are priced with, so GolemDB and host weights always come from the same `priced_at`.
- **Missing weights are unpriced.** If, at activation, the active model's code expects a weight that has no value, that weight is **unpriced**: any call whose cost needs it fails with `OutOfBudget{spent, required: None}`, also under `Unlimited`, exactly like an arithmetic overflow (D8). This fails safe (R4): the affected operations are unavailable until fixed, never free. Nothing halts and nothing splits, since every node prices from the same committed weights, and the admin repairs it with a weight patch, which is unmetered. Weight names the code does not declare are ignored at activation. A missing host weight is reported as absent in the snapshot; the host must treat operations that need it as unavailable, never as free.
- **Unknown names are rejected where every node can check.** A weight patch on the **active** model naming a weight its GolemDB or host code version does not declare is reverted with `InvalidArgument`: every node runs that code, so the verdict is deterministic. Writes to a pending version are checked for form only.
- **Warn logs.** GolemDB writes a warn-level log entry, for operators' observability, at activation for every missing weight and every ignored name, and on every call that fails because a weight is unpriced, naming the weight. These are node logs, not consensus output.
- **Minimum activation delay.** `#minActivationDelay` (`u32`, commits) is a genesis parameter in `#params` ([design §4](golemdb-design.md#params-recordid-0)). It guarantees every node operator a window between a model's install and its activation in which to upgrade to code that implements it. Without it, `A > head` would allow installing at head 99 with activation 100, leaving no window. For Arkiv, a value of about a day of blocks.
- **Patch the active model.** A weight change staged while head is H takes effect only after the commit containing it succeeds and head becomes H+1. It cannot change pricing mid-branch or before persistence; a failed or discarded change has no effect. An immediate patch remains possible, for example to repair a missing weight. A host that wants a weight to change only with a delay enforces that in its admin authorization: GolemDB does not.
- **Capture pricing once.** Branch calls retain the pricing snapshot from their base commit; stale branches remain subject to the existing invalidation rules. Calls without a branch capture the current head's snapshot at admission. `priced_at` records that pricing commit, not the data commit being read. A call already in progress keeps its captured snapshot even if head advances.
- **Historical data does not select historical prices.** A read targeting an old commit uses the same current pricing snapshot as a current-data read admitted at the same head. Each query page is a new call: pinning the data commit does not pin the pricing schedule across pages.
- **Pre-paid work is not re-priced.** Work paid in advance keeps the schedule of the call that paid it: term removal (`write.index_term_create`), copying bytes into history (write byte weights) and record-induced deferred commit work. Host-funded block/commit overhead is separate (D8). Re-pricing prepaid work after a weight change is impractical; if weights rise, the later work is underpaid, bounded to one term removal per term and one copy per byte. Accepted. The same holds for a host that collects a record's deletion cost in advance, as Arkiv does for expiry: the D5 bound is computed at the weights and depth current when it is collected.
- **Authorization is the host's.** GolemDB validates the lifecycle; the host decides who may change weights. A host that changes weights inside its own commits needs branch-scoped admin calls, an API extension not yet specified.

For example, a model version with activation commit 100 becomes active when committed head
reaches 100. Branch work based on head 99 that produces commit 100 still uses the old
schedule. Calls admitted at head 100, including reads of commit 20, use the new schedule
and report `priced_at: 100`. This is a GolemDB committed-head boundary, not an implicit
rule to switch prices before executing a host block numbered 100.

**Example: three weights across three model versions.** A `@meteringModel` value is
24 bytes, three big-endian `u64`s: activation commit `A`, GolemDB code version `G`, host code version `H`.
`H = 0` means the host declares no weights. `@modelWeight` holds one `u64` per model
version and weight name.

| Record | Cell key | Value |
| --- | --- | --- |
| `@meteringModel` | model version `V` | `A ‖ G ‖ H` |
| `@modelWeight` | `V` ‖ weight name | weight |

*Genesis, model 1:* GolemDB code version 1, host code version 1.

```
@meteringModel  1 → (A: 0, G: 1, H: 1)
@modelWeight    1 ‖ read.cell                 → 100
                1 ‖ write.cell_byte           → 2
                1 ‖ host.storage_price        → 5
```

*Weights-only, model 2:* at head 400 the admin announces a storage price of 8. The code
is unchanged, so `G` and `H` are too; the model carries its complete weight set.

```
@meteringModel  2 → (A: 1000, G: 1, H: 1)      A ≥ 400 + 1 + #minActivationDelay
@modelWeight    2 ‖ read.cell                 → 100
                2 ‖ write.cell_byte           → 2
                2 ‖ host.storage_price        → 8
```

From head 1000 every call is priced with model 2. No node halts: all run code for `(1, 1)`.

*Code change, model 3:* a GolemDB release adds `write.cell_trie_update`, so it is GolemDB
code version 2. The host's names are unchanged, so `H` stays 1.

```
@meteringModel  3 → (A: 5000, G: 2, H: 1)
@modelWeight    3 ‖ read.cell                 → 100
                3 ‖ write.cell_byte           → 2
                3 ‖ write.cell_trie_update    → 100
                3 ‖ host.storage_price        → 8
```

At install, every node checks only form: `V` is newer, `A` is in bounds, the cells are
well-formed. Nodes without GolemDB code version 2 halt at 5000 ("upgrade required");
the others price with model 3 from there. An immediate patch of the active model,
such as `2 ‖ host.storage_price → 9`, still takes effect at the next commit; whether
the host allows it is its admin policy.

## D10. GolemDB Weights

Weight names are dotted: `write.*` for write calls, `read.*` for read calls, `host.*` for a host's
own weights (D9); a family of weights shares a stem, such as `write.cell.create`,
`write.cell.update` and `write.cell.delete`, written `write.cell.{op}`. A weight is a price in the
cost schedule. Op classes, `op.*`, are not weights: they name kinds of measured work in the
implementation, from which weights are calibrated, and are never stored or charged.

| Weight | Used for | Calibrated from op classes (`op.*`) |
| --- | --- | --- |
| `write.admission.base`, `write.admission.entry`, `write.admission.byte` | Fixed request validation, entries inspected and bytes validated, including rejected input (D2) | reference admission validation |
| `write.record.create`, `write.record.patch`, `write.record.delete` | Record base cost excluding admission and the binding lookup (`read.key`): in-memory record ID counter, cap check, cell enumeration (D2) | `op.record_create` (in-memory allocator part) |
| `write.cell.create`, `write.cell.update`, `write.cell.delete` | Cell compute (D2) | `op.cell_write` / `op.cell_remove`, `op.cell_history_append`, `op.cell_changeset_write` |
| `write.cell_trie_update` | Cell-trie path, per node × `cell_trie_depth` (D2, D3) | `op.celltrie_path_rewrite` |
| `write.cell_byte` | Cell storage: bytes written, including their later copy into history (D2, D4) | byte term, plus the change-set copy |
| `write.index.join`, `write.index.leave` | Index membership change (D2) | `op.index_term_flip`, `op.secidx_container_rewrite`, `op.secidx_path_rewrite` |
| `write.index_term_create` | Creating a new term, including its later removal (D2) | new index row and index-trie leaf, plus their removal |
| `write.index_trie_update` | Index-trie path, per node × `index_trie_depth` (D2, D3) | `op.indextrie_path_rewrite` |
| `write.index_byte` | Index storage: bytes written (D2, D4) | byte term (new term keys) |
| Core read weights (`read.key`, `read.cell`, `read.cell_scan`, `read.byte`, `read.term`) | Point reads of records and index terms (D6.1), shared by `get`, write planning (D2) and query materialization | `op.key_resolve`, `op.cell_read`, `op.index_seek` |
| Query read weights (`read.base`, `read.admission.base`, `read.admission.entry`, `read.admission.byte`, `read.term_step`, `read.container`, `read.set_container`, `read.sort_compare`) | Read steps of `query` and `count` (D6.2); `get` also pays `read.base` and admission | read op classes |
| `write.log_append.base`, `write.log_append.byte` | Immutable-log append: fixed staging work, row bytes written and their shard residency (D7) | `op.immutable_data_append` plus the byte term |
| `read.log_row` | Immutable-log row read (D7); bytes read use the `read.byte` read weight (D6) | `op.immutable_data_read` |
| `host.*` (host-declared) | The host's own prices and limits; never used by GolemDB (D9) | the host's calibration and capacity planning |

Not weights: the D3 depth table (code, metering model version) and the D5 per-record caps (deployment parameters).

## Worked Example: Create, Patch, Delete

One record followed through `create`, `patch` and `delete`, priced by D2. It illustrates
the accounting for these operations; remaining specification gaps are tracked in
[Review Findings](#todo-review-findings). Receipt details are requested for each call.

**Weights.** Illustrative, not calibrated. Both tries sit at modeled depth 6 (about 10M cells and 10M terms), so every trie path costs 6 × 100 = 600.

Let `A_create`, `A_patch` and `A_delete` denote the admission costs calculated by D2
for these requests. The tables show the remaining costs; add the corresponding admission
cost to obtain the full charge. The illustrative `write.record` values below exclude admission
and the binding lookup (`read.key`); planning reads use the core read weights (D6.1).
Admission weights and exact reference counts are not assigned numerical values here.

| Weight | Value | Weight | Value |
| --- | --- | --- | --- |
| `read.key` | 100 | `read.term` | 100 |
| `read.cell` | 100 | `write.index.join`, `write.index.leave` | 650 |
| `write.record.create` | 200 | `write.index_term_create` | 400 |
| `write.record.patch` | 100 | `write.index_trie_update` | 100 per node |
| `write.record.delete` | 150 | `write.index_byte` | 2 |
| `write.cell.create`, `[update]`, `[delete]` | 350 | | |
| `write.cell_trie_update` | 100 per node | | |
| `write.cell_byte` | 2 | | |

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
| Record base | `read.key` (binding absent) + `write.record.create` (record ID): 100 + 200 | 300 |
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
| Record base | `read.key` (binding) + `write.record.patch` (cap check): 100 + 100 | 200 |
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
`spent` is the record base (200) plus the core reads performed: `#meta` (100), two cells (200)
and two index terms (200). Nothing is written.

**With `Limited(A_patch + 250)`**, admission completes and the record base with its binding
lookup costs 200; the next `#meta` read would cost 100, so it is not performed. The result is
`OutOfBudget{spent: A_patch + 200, required: None}`. With `Limited(A_patch + 6,444)`,
the patch succeeds at the exact budget. With `Unlimited`, the same planner returns
`A_patch + 6,444` for estimation without applying the patch.

**Rejected input also costs.** If validation finds an invalid cell name after inspecting
some entries and bytes, the call returns `InvalidArgument` with
`spent = write.admission.base + entries inspected × write.admission.entry + bytes validated × write.admission.byte`.
The counts include the work that detected the invalid name, but not uninspected input.
No binding lookup or write is charged. If the next validation step cannot be funded,
the call instead stops before that step with `OutOfBudget` and no `required` total.

### Delete

`owner` is the only member of its term, so the term is removed; its removal was paid at creation. "closed" keeps other members.

| Operation | Terms | Cost |
| --- | --- | --- |
| Record base | `read.key` (binding) + `write.record.delete` (cell enumeration): 100 + 150 | 250 |
| Delete binding | 350 + 600 (read in the record base) | 950 |
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

## Worked Example: Reads

The two reads Arkiv puts into consensus gas ([tokenomics](arkiv-tokenomics.md#inputs-from-golemdb)),
priced by D6.3: the owner `get` behind every patch, extend and delete, and the expiry purge's query.
Each shows its ledger, the counts the receipt reports, and the cost as those counts times the
weights.

**Weights.** Illustrative, not calibrated; similar steps start equal (D6).

| Weight | Value | Weight | Value |
| --- | --- | --- | --- |
| `read.admission.base` | 100 | `read.key` | 100 |
| `read.admission.entry` | 20 | `read.cell`, `read.cell_scan` | 100 |
| `read.admission.byte` | 1 | `read.byte` | 1 |
| `read.base` | 100 | `read.term`, `read.term_step` | 100 |
| `read.container` | 200 | `read.set_container` | 20 |

### Owner `get`

`get(Head, RecordOp::get(key, budget).only(["$owner"]))` on an existing entity. `$owner` is a
`bytes20` attribute: `(8 + 6) + (1 + 20)` = 35 bytes on the D4 basis.

| Step | Ledger | Cost |
| --- | --- | --- |
| admission: base, one projected name of 6 bytes | 1 `read.admission.base`, 1 `read.admission.entry`, 6 `read.admission.byte` | 126 |
| call base: snapshot at the head | 1 `read.base` | 100 |
| key resolution | 1 `read.key` | 100 |
| point read of `$owner`, then its bytes | 1 `read.cell`, 35 `read.byte` | 135 |
| **Total** | | **461** |

An unknown key stops after the key resolution with `NotFound` and costs 326; a budget of 460 stops
before the 35 bytes are copied, with `OutOfBudget` and 426 spent.

### Purge query

One block's purge reads the entities expiring at block `n` (Arkiv's
[purge order](arkiv-tokenomics.md#purge-budget)):

```
query(None, QueryOp::filter(Eq { $expiresAt = n as u64 }, budget).only([]).limit(100))
```

The term `$expiresAt = n` holds 250 entities, whose record IDs fall into 2 bitmap containers. The
page takes the first 100 in record-ID order; with an empty projection, each returns only its key.

| Step | Ledger | Cost |
| --- | --- | --- |
| admission: base; the `Eq` node and its value; the field name (10 bytes) and the `u64` (8 bytes) | 1 `read.admission.base`, 2 `read.admission.entry`, 18 `read.admission.byte` | 158 |
| call base | 1 `read.base` | 100 |
| `Eq`: term lookup, its 2 containers; one set, no union | 1 `read.term`, 2 `read.container` | 500 |
| page of 100 records: each record's `#key`, 45 bytes | 100 `read.cell`, 4,500 `read.byte` | 14,500 |
| **Total** | | **15,258** |

The full match set is built, all 250 records, but only the page is materialized: the purge's cost
grows with its `limit`, not with the size of the expiry term. The budget's stopping point is exact:

| Budget | Outcome | Spent | Ledger at the stop |
| --- | --- | --- | --- |
| `Limited(15258)` | the page, 100 keys | 15,258 | as above |
| `Limited(15257)` | `OutOfBudget { required: None }`, no records | 15,213 | 100 `read.cell`, 4,455 `read.byte`: the 100th record's `#key` was read, its 45 bytes did not fit |

A block with nothing expiring reads an absent term: admission, call base and one `read.term`, 358
in total, with an empty page. This is the cheap step the purge cursor takes through quiet blocks.

## Open Questions

1. **Is the D3 depth table worth it?** Realistic depths span 4–7 nodes, more likely 4–6. Is that range worth the extra code and a more complex pricing story for users, compared with one fixed depth folded into the weights?
2. **Depth table as code or weights?** [Design §4](golemdb-design.md#meteringmodel-recordid-32) expresses tunable constants such as a modeled depth as named weights. Should D3's thresholds and depths be weights, tunable without a new model version?
3. **Value of `#minRetention`.** How many commits must every node retain? It bounds full-node storage and sets the history residency that the write byte weights and the history part of `write.cell.{op}` are calibrated for (D4).
4. **Define the op classes.** D10 names the op classes (`op.*`) weights are calibrated from, but their definitions lived in the architecture document, which no longer exists. Before calibration, each op class needs a short definition: the work it measures, and how.
