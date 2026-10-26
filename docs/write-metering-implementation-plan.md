# Write metering implementation plan

Implement the public receipt contract first, then per-record metadata, then budgeted write planning and execution. Metering orchestration belongs in `golemdb-record`; `golemdb-metering` owns `Budget`, pricing rules, weights, ledgers and budget accounting. The branch crate continues to provide state access, isolation, rollback and commit.

This plan covers `create`, `patch` and `delete`, including their planning reads. `get`, `query` and `count` receive the new result shape immediately, but their pricing remains outside this write-metering implementation. Returning a receipt does not yet imply that a call enforces a work budget.

**Release scope:** iterations **1–6 are priority 1 for the Mumbai release**. Iterations **7–9 are after Mumbai and are not priority 1**: estimation/deletion bounds, runtime model and weight updates, and broader calibration/verification. The focused acceptance tests in iterations 1–6 remain part of Mumbai scope.

Sources: [metering specification](golemdb-metering.md), [API receipts](golemdb-api.md#receipts), [agreed API changes](golemdb-api.md#agreed-changes-not-yet-implemented), and [API priorities](api-priorities.md). This is an implementation proposal, not a report of implemented functionality.

## 1. Decisions and boundaries

- Record builders only collect input. Admission validation and normalization belong in record execution, after handle/snapshot selection; there is no public unmetered validation shortcut. Pass raw ordered entries and budgets across the API/record boundary. Input allocation, typed value construction and transport decoding remain host responsibilities.
- Change the API before introducing actual charges. All six operations return `Metered<T>`, including failures. Initially `cost = 0`; successful write effects and `priced_at` must already be correct.
- Add `#meta` before enforcing write prices. Its four counters describe one record's user cells, not database-wide populations or accumulated operation charges.
- The record crate builds and applies write plans. The metering crate prices logical steps and mutations; it does not discover old values, access storage or construct an executable plan.
- Define `Budget` in the metering crate and re-export it from API and query. Keep `QueryOp` and `CountOp` in query for now; moving their public builders into API is deferred.
- Planning reads are budgeted while the plan is being built. Building an unrestricted plan and pricing it afterward would not meet the specification.
- Use index-term existence at the branch base for the initial creation surcharge. Do not implement a branch-aware index overlay for pricing in this work.
- Keep actual index maintenance exact. Use the named `INITIAL_DEPTHS` constant (6 for both tries) initially; database-wide population counters and D3 depth selection are deferred. The pricing simplifications do not change committed user data or index contents.
- Leave public read pricing, immutable-data pricing, and unrelated API features outside this work. In particular, this plan does not require implementing generated record keys or a new query planner.

## 2. Ownership and interfaces

| Layer | Responsibility |
| --- | --- |
| `api` | Public CRUD requests, query/count and `Budget` re-exports, `Metered<T>`, receipts and error conversion; genesis/admin integration and database construction. |
| `record` | Admission, record semantics, metered planning reads, `WritePlan`, metadata and effect calculation, final approval and application. |
| `metering` | `Budget`, immutable schedules, supported code identifiers, checked pricing/count arithmetic, explicit modeled depths, ledgers and per-call accounting. No database dependencies. |
| `branch` | A stable state view while planning/applying, origin/head validation, overlay mutation, rollback, sealing and commit; access to base state needed by record planning. |
| `cells` / `index` | Encodings and storage primitives, including a point lookup that checks whether a term exists without loading its posting list. Record calls using these primitives are charged through the meter; the storage crates neither choose prices nor track a caller's budget. |
| `query` | Query/count request builders, existing query planning/execution and receipt context now; its own metered execution later. |

### The metering crate does not create plans

The record layer knows what a patch means, which old cells are needed, whether a change is a no-op, and which system cells must change. These decisions produce a record-layer `WritePlan` containing:

- The resolved record identity and the old values needed for application.
- User-cell mutations and resulting user-effect counts.
- Per-record system mutations, including `#meta`, `#key` and the binding where applicable.
- Any proposed allocator change, without changing the allocator during planning. For example, a create can propose record ID `42` and next allocator value `43`; only approved application advances the allocator. Failed planning or estimation leaves ID `42` available. This is the internal numeric record ID, not the caller's record key.

Accounting remains in `WriteMeter`; `Records` holds the immutable `WritePricing`
configuration. The plan contains the mutations and effects, not the budget or schedule.

Index joins/leaves are derived and priced during planning. Their approved charges are included in the write's receipt, even though the physical index changes happen later during sealing. Sealing derives the final net changes and does not charge them again. For two patches changing an attribute `red -> blue -> green`, each patch pays for its leave and join, while sealing can apply the net `red -> green` change.

The old user-cell value comes from current branch state, including earlier writes. Only the term-existence decision for the creation surcharge uses committed branch-base index state; the simplification does not make record planning ignore earlier branch writes.

`WritePlan` belongs to the record crate. A future plan for record reads, if needed, would also belong there; a query plan belongs to the query crate. Write-planning reads do not require a separate `ReadPlan` object: the write planner performs them incrementally through metered helpers. The metering crate accepts charge descriptions and has no executable read or write plans.

### Metering interface (iteration 3)

One `WriteMeter` owns one call's budget, completed planning accounting and prospective
mutation accounting. `Accounting` contains a `cost` and a `Ledger`: counts per exact
weight name, in name order. `Details` continues to describe user-data effects. System
mutations and reads appear in the ledger even though they are absent from effects.
Internally, `planning_accounting` holds charged admission and planning work, while
`mutation_accounting` holds prospective mutation charges. Combined overflow checks
compute a temporary total without changing either accounting value.

```text
PlanningStep:
    AdmissionBase | AdmissionEntry | AdmissionBytes(count)
    RecordBase(Create | Patch | Delete) | CellRead | TermRead

MutationCharge:
    CellCreate(bytes_written) | CellUpdate(bytes_written) | CellDelete
    IndexJoin(bytes_written, create_term) | IndexLeave

WriteMeter.new(schedule, modeled_depths, budget)
meter.run_step(step, action) -> action result | MeteringFailure
meter.add_planned_mutation(mutation) -> () | MeteringFailure
meter.into_accounting() -> completed planning cost and ledger; discard planned writes
meter.finish() -> approved total Accounting | MeteringFailure; consumes meter
```

`Schedule` has immutable named `u64` weights and a supported write code identifier.
The initial identifier is `WRITE_CODE_VERSION`; it identifies the pricing code, not
an activated database model version. Missing weights remain absent, explicit zero
weights remain valid, and duplicate names and unsupported code versions are rejected.
Extra names, including host weights, remain inspectable but the write meter selects
only its own declared weights. Genesis/name declarations and activation validation
remain iteration 5/8 responsibilities. A failure retains the missing weight name for
integration-layer diagnostics.

`ModeledDepths` is passed explicitly. `INITIAL_DEPTHS` is the temporary named constant
`{ cell_trie: 6, index_trie: 6 }` for initial integration. Tests may provide other
values; the worked example below deliberately retains depth 4. There is no database
counter lookup or population-to-depth function in iteration 3. Restoring D3's 4–7
table later changes the model's counting rules. The cell path charge is
`depths.cell_trie × write.cell_trie_update`.

`run_step` implements one atomic check-and-charge-before-work checkpoint:

1. Price all components, check cost and ledger-count arithmetic, and verify that the
   whole step fits the remaining limited budget. `RecordBase(op)` is the indivisible
   sum `read.key + write.record.{op}`; neither component can be charged alone.
2. Record the charge and ledger counts, then invoke the action once. No action runs
   and no charge is added if the checkpoint fails.
3. Return the action's result. An absent lookup or a validation error still costs the
   performed step. An action returning a domain `Result` yields a nested result;
   the record adapter must propagate that error before attempting any later step.

This combines the earlier `check_step`/`complete_step` proposal into one helper. It
prevents reused or uncompleted approval tokens and follows D6.3's charge-before-work
convention. An operational storage fault or panic retains a started step's charge;
these faults are outside deterministic validation/fee policy. The action itself has
no access to the mutably borrowed meter. A metering failure stops that meter permanently.

`add_planned_mutation` checks prices and arithmetic without spending mutations or
comparing their prospective total with the budget. `finish` approves the complete
representable total, or returns completed planning accounting with
`required: Some(total)`. Earlier budget stops, missing weights or overflow return
`required: None`. No extra work runs to discover a total after a failure. Mutations
are absent from a failure's ledger. `Unlimited` skips budget comparisons only.

For example, with budget `10`, spent `4` and `read.cell = 2`, a funded `CellRead`
records cost `6` and performs the read. A planned mutation priced at `5` leaves spent
at `6`; final refusal reports cost `6` and `required: Some(11)`. Budget `11` succeeds
with cost `11`, including planning only once. All count additions, products and sums
are checked, including counts whose weights are zero.

`Outcome` carries accounting separately from effects. The API forwards it to
`Receipt.cost` and `Receipt.ledger`; metering failures map to the existing
`OutOfBudget { required }`. Other failures also retain their charged work, while
failed effects remain zero. Database writes use an explicit zero-weight schedule
and return full ledgers at zero cost; read calls still return empty ledgers.
Public request signatures and `Budget` are unchanged. Read-related
`evaluated_at` changes remain deferred.

Keep `CommitId` and database access outside the metering core. Record integration
combines the pricing commit, immutable schedule and depths. Persisting this context
is iteration 5; supplying logical work from real CRUD execution is iteration 4.

### Record-side planning reads (iteration 4)

Use a small record adapter around the existing branch read view and the meter:

```text
read_cell_metered(key):
    result = meter.run_step(CellRead, read key from branch state)
    return result or its error
```

Use `CellRead` for `#meta` and touched cells, `TermRead` for index-term lookups, and
`RecordBase(op)` for the binding lookup plus fixed record work. Required reads pay
when missing too. Configuration reads and identity checks need an explicit mapping
to the reference procedure; not every physical get is a separate `read.cell`.
Write planning uses the shared `read.key`, `read.cell` and `read.term` vocabulary,
without public get/query metering or `read.byte` charges for caller-returned data.

### Implemented admission and planning order (iteration 4)

The initial write code version fixes the following rules. Input allocation and
canonical typed-value construction happen before the call and remain host work.

1. After branch admission, `AdmissionBase` checks the reserved key.
2. Visit entries in caller order, including duplicates. `AdmissionEntry` checks
   name and value lengths against genesis limits, then counts name bytes plus
   value payload bytes for admission pricing; remove counts only its name.
   Type/kind headers and the fixed record key belong to the fixed charges.
   A rejected length incurs no byte-validation charge. There are no additional
   request-entry or total-input-byte caps; record caps apply to the resulting state.
3. `AdmissionBytes(n)` guards name grammar, requested-kind conversion and insertion
   into the sorted map, including duplicate detection. Invalid diagnostics show
   at most 64 name bytes. Values already have canonical types and encodings.
4. `RecordBase(op)` reads and decodes the binding. Create stops on an existing
   binding; patch/delete stop on an absent one. Create proposes an allocator value
   and checks its vacancy as fixed `write.record.create` work, without consuming it.
5. Patch/delete read `#meta`, then `#key`, charging `read.cell` for each. The latter
   preserves the implementation's existing identity check; it is an explicit
   additional patch read compared with D2's minimal system-cell table.
6. User cells are planned in bytewise name order. Patch reads every requested cell,
   including absent removals and no-ops. For each changed indexed cell, read the old
   term before the new term, one `read.term` per leave/join. Only joins use the
   branch-base absence result for the term-creation surcharge. Each completed
   mutation description is accumulated immediately; caps are checked on the final
   metadata before system mutation charges and final approval.
7. Delete scans exactly the user-cell count from `#meta`, one `read.cell` before
   each step, with index-leave planning after each indexed cell. `#meta` and `#key`
   are reused from their point reads. One bounded end probe checks for extra cells,
   covered by the fixed `write.record.delete` scan work; counters are also verified
   against the resulting effects. Binding, `#key` and `#meta` are then deleted.

`Records::new` requires a `WritePricing` containing the schedule, depths and
immutable cell/record limits. The API supplies validated genesis limits and
`Schedule::zero_write` with `INITIAL_DEPTHS` (6). Admission and planning call
`WriteMeter` directly: there is no optional adapter or unpriced path, and admission
performs no database reads. Zero weights still produce ledgers and budget checks;
missing weights remain errors. Persisted schedule loading remains iterations 5/6.
The example's depth-4 create remains 266 total, with 24 spent planning. Under the
same fixture, deleting those two cells costs 61 total with 15 spent planning;
empty-record deletion costs 33.

**Bounded deletion scans.** The branch maintains live overlay keys and coalesced
intervals of hidden origin rows. Its read-only `RecordScan` merges cells, while
`locate_cell` returns opaque `CellLocation` metadata describing a key's committed
position and immediate successor, using at most two origin cursor steps. Neither
operation makes a mutation or pricing decision. Record obtains this metadata
within the cell-read checkpoint and creates its own `PlannedDelete`. After budget
approval, record calls the branch writer's `delete_at`, which applies the tombstone
and updates the auxiliary indexes in memory without storage reads. The journal
restores those indexes on rollback or failure.

A user-cell scan step reads at most two origin rows and one live overlay entry,
regardless of the number of preceding tombstones. Thus a patch removal checkpoint
covers one point read plus at most two location-read steps; a delete checkpoint
covers at most four origin steps, including location discovery. These bounded
implementation costs must be included when calibrating `read.cell`.
There are no extra receipt counts for tombstones and no second index writer.
Trusted low-level writers must use `locate_cell` and `delete_at` for user cells that
will later be scanned by this bounded interface; raw tombstones are rejected there
instead of silently performing an unbounded scan. The existing general-purpose
scan is unchanged.

## 3. Simplified index-term pricing

For a logical join, use:

```text
charge_term_creation = !term_exists_at_branch_base(term)
```

Each logical index join/leave still pays its normal read and mutation charges. A no-op cell assignment produces no join or leave. Reusing a cached lookup must not discount a logical charge.

| Situation within one branch | Creation surcharge |
| --- | --- |
| Term absent at base; first join | Charged. |
| Same term absent at base; another operation joins it | Charged again. |
| Term present at base; another record joins it | Not charged. |
| Term present at base; emptied and later joined again | Not charged. |
| Term absent at base; created, emptied and joined again | Charged on each join. |

The repeated-join case implements the accepted overcharging. The remove/recreate case means this policy is not universally an upper bound on a branch-aware creation tariff. Treat it as an explicit initial pricing rule, and add it to D2 and its conformance examples before enabling charges. Returning to exact branch-state pricing later changes the model's counting rules and must be versioned.

Expose a point lookup of the term's existence in the origin index through the branch read context. Do not load the full posting bitmap to answer this question or reconstruct index state by replaying the branch journal.

The receipt's `index_joins` remains the actual number of logical joins. Do not label the number of creation surcharges as “terms created.” When population counters are implemented later, `#indexTerms` must come from actual net index changes at seal time.

## 4. Worked example — creating a record in the record crate

Consider a caller-keyed create with two user cells:

```text
field:     age = i32(30)
attribute: color = str("red")
```

Assume the key is unused, the next internal record ID is `42`, and the `color = "red"` term is absent from the branch-base index. The record starts with zero metadata counts. No old user cells or old `#meta` need reading for a create.

### Planning and execution

This pseudocode runs in the record crate. A helper keeps each planning step behind its charge checkpoint:

```text
charged_step(step, action):
    outcome = meter.run_step(step, action) // Check and charge before action.
    return outcome or its error             // Rejected validation is charged.
```

The meter tracks all costs; record code builds the plan and performs the work. Local storage faults follow the separate operational-failure policy described earlier.

```text
create(request, branch):
    Validate the handle and acquire the branch guard.
    If rejected: return error, cost 0, no pricing commit, zero effects.
    Keep the guard until planning and application finish.

    pricing = capture the schedule and modeled depths at the branch origin
    meter = WriteMeter.new(pricing.schedule, pricing.depths, request.budget)
    plan = empty create plan

    ADMISSION — inspect the request and immutable configuration; no record reads:
        charged_step(AdmissionBase, validate fixed request fields)
        for each input entry in caller order:
            charged_step(AdmissionEntry, validate header and declared lengths)
            n = validation byte count from those lengths
            charged_step(AdmissionBytes(n), validate bytes and requested kind,
                         reject duplicates and prepare the validated entry)

    STATE-DEPENDENT PLAN — read state and prepare changes; do not write anything:
        binding = charged_step(RecordBase(Create), read key binding)
        if binding exists: fail with AlreadyExists

        Inspect the allocator and verify the next ID is usable.
        Propose ID 42 and next allocator value 43; leave the allocator unchanged.

        for each user cell in name order:
            Add its create to the plan and update user-effect/metadata counts.
            if it is an attribute:
                exists = charged_step(TermRead, look up its term at branch base)
                Add a planned join, with creation surcharge when not exists.

        Check the resulting metadata against the record caps.
        Add system-cell creates: binding, #key and #meta.

        for each planned user or system cell create:
            meter.add_planned_mutation(CellCreate, encoded cell bytes)
        for each planned index join:
            meter.add_planned_mutation(IndexJoin, term bytes, creation surcharge)

    APPROVE:
        approved = meter.finish()        // Checks planning + mutation cost.

    ON ANY PLANNING OR APPROVAL FAILURE:
        Return the error, completed planning cost, pricing commit, zero effects.
        Preserve OutOfBudget.required when provided; apply nothing.

    APPLY — only after approval:
        Stage the planned cell creates and allocator value 43 in the overlay.
        Return the record key, approved total cost, pricing commit, user effects.
```

On an ordinary planning failure, `meter.into_accounting()` returns the completed planning cost and ledger. Metering failures, including final budget refusal from `finish`, carry that accounting in `MeteringFailure.accounting`. Planned mutation costs are not spent until successful application. Application reuses the prepared values and makes no further pricing decisions or state reads.

In-memory allocator work is covered by `write.record.create`; its separate binding lookup uses `read.key`; the allocator is excluded from the priced cell creates. Persistence of its final value is host-funded. The planned index joins are charged now; sealing later applies the net index changes without charging again. Estimation follows the same flow through approval and skips application.

Admission validates the original entries at their defined checkpoints; builders have done no request validation. Each failed validation check must complete its charge before its error is propagated. Process entries in input order, checking name/value lengths before name grammar, requested kind and duplicates; fixed target checks come first. Iteration 3 pins checkpoint accounting; iteration 4 pins request counting and enforces the existing genesis limits alongside the actual admission adapter. Missing weights and arithmetic overflow also stop planning with only completed work charged.

### Concrete charges and receipts

Use these illustrative test weights, with both modeled depths equal to `4`. For this example's admission convention, the fixed key/form checks belong to the base; each of the two entries has one header check, and validated bytes are the cell-name bytes plus value payload bytes. That is `(3 + 4) + (5 + 3) = 15` bytes. This is an example fixture, not a finalized admission rule or a calibrated schedule.

| Component | Test weights / calculation | Cost |
| --- | --- | --- |
| Admission | base `2`, entry `1`, byte `1`: `2 + 2 × 1 + 15 × 1` | 19 |
| Record base (separate from admission) | `read.key = 1` + `write.record.create = 2` | 3 |
| Index read | `read.term = 2` | 2 |
| Cell creates | `write.cell.create = 5`, trie-node `1`, byte `1`: five cells at `5 + 4 × 1`, plus 173 bytes | 218 |
| Index join | join `3`, trie-node `1`, term-create `7`, byte `1`: `3 + 4 × 1 + 7 + 10 × 1` | 24 |
| **Total** | **Planning 24 + prospective mutations 242** | **266** |

The five cell sizes are `age: 16`, `color: 17`, `#key: 45`, `#meta: 46`, and the binding: `49`, totaling `173`. The new metadata is `{ cells: 2, cell_bytes: 33, indexed_cells: 1, index_bytes: 10 }`. All user and per-record system creates contribute to cost; only the two user cells contribute to effect details.

| Budget / outcome | Work completed | Returned accounting |
| --- | --- | --- |
| `266`: success | Full planning; five cell creates and allocator change staged | Cost `266`; user effects below. |
| `265`: final refusal | Full planning; no writes or allocator change | Cost `24`; `OutOfBudget { required: Some(266) }`; zero effects. |
| `23`: early refusal | Admission and binding/base work; index read cannot fit | Cost `22`; `OutOfBudget { required: None }`; zero effects. |
| Sufficient budget, key already exists | Admission and binding lookup only | Cost `22`; `AlreadyExists`; zero effects. |

The API facade maps `Outcome` to public `Metered<T>` without recalculating charges or rereading head. For the successful case at pricing commit `100`, it returns:

```text
result: Ok(the requested record key)
receipt:
  cost: 266
  priced_at: Some(100)
  ledger:
    read.key: 1
    read.term: 1
    write.admission.base: 1
    write.admission.byte: 15
    write.admission.entry: 2
    write.cell.create: 5
    write.cell_byte: 173
    write.cell_trie_update: 20
    write.index.join: 1
    write.index_byte: 10
    write.index_term_create: 1
    write.index_trie_update: 4
    write.record.create: 1
  details:
    cells_created: 2
    cells_updated: 0
    cells_deleted: 0
    index_joins: 1
    index_leaves: 0
    cell_bytes_written: 33
    cell_bytes_deleted: 0
    index_bytes_written: 10
    index_bytes_deleted: 0
```

The meter alone tracks the budget and costs throughout this example. Record code supplies logical step/mutation descriptions, performs the approved reads, builds the plan, and applies it only after `finish` succeeds.

## 5. Iterations

Each iteration should leave the workspace compiling and have its own focused checks. The initial zero-cost increments are development milestones, not a production budget guarantee.

### Iteration 1 — Public results and request contract

**Priority:** 1 — Mumbai release.

**Status:** implemented. CRUD, query and count return receipts; constructors require
an explicit budget; builders only collect raw ordered input; patch `set` preserves stored kind.
Record execution now owns admission and projection normalization. `first_error` and
builder `validate()` have been removed; budgets and raw entries reach the record crate.
Receipt effects and pricing context are populated, while costs remain zero and
budgets remain unenforced. See the [Rust API guide](../crates/api/README.md).

**Outcome:** callers can adopt the final result shape before the pricing implementation exists.

1. Add public `Metered<T>`, `Receipt` and `Details`, with the fields and helpers in the API specification. Preserve receipts when mapping errors; dropping a receipt must be explicit through `into_result()` or equivalent.
2. Return `Metered<RecordKey>` / `Metered<Record>` / the existing operation payload from CRUD, and `Metered<QueryPage>` / `Metered<u64>` from query/count. Preserve current payload semantics.
3. Carry execution context and user effects out of the lower layers on success and failure. The API crate converts these internal reports to public receipts; lower layers must not depend on `api`.
4. Capture `priced_at` from the same state selection used by execution. Branch calls use their validated origin; committed reads use the captured head even when reading an older commit. Query admission errors also need the captured head when snapshot selection succeeded. Do not perform a separate head lookup after execution.
5. Compute accurate user effects for successful writes, including no-ops. Failures and reads report zero effects; system cells are excluded. Introduce one shared record-layer effect/byte-counting helper for later metadata and planning use.
6. Make record builders passive: retain original ordered entries, duplicates and requested kinds without validation, conversion or sorting. Remove saved builder errors and `validate()`. Pass record-owned raw inputs and budgets into execution; a private shared admission module checks fixed targets, entry lengths, name grammar, explicit kind compatibility and duplicates before record lookup. Only execution prepares validated maps and normalizes read projections. Preserve typed error causes and return failures with receipts.
7. Make budgets explicit in requests, selecting one construction convention for CRUD/query/count. A required constructor argument is the recommended default. Define the shared `Budget` type in `golemdb-metering` and re-export it from API and query; do not make record depend on query.
8. Align `set` with the documented preserved-kind behavior and `CellNotFound`, since these determine effects. Fix handle/admission/state validation order. Update API consumers, examples, mocks and error handling together.

For reviewability, split this iteration into result/receipt plumbing and request/semantic alignment if needed. Both precede metering enforcement. Cost remains zero and budgets are explicitly documented as unenforced during this increment.

**Likely areas:** `crates/api/src/{lib.rs,database.rs,record_op.rs}`, record outcomes and effect helpers, `crates/query/src/queries.rs`, API/integration tests.

**Acceptance:** builders preserve malformed input and duplicates without validation; submitted errors follow input order and leave state unchanged; projection normalization occurs only during execution; receipts survive ordinary success and error paths; historical failures report the head used for pricing; invalid handles have zero cost and no pricing snapshot; a concurrent head advance cannot mix receipt context with execution state; actual user effects match changes, including no-op and failed patches.

### Iteration 2 — `#meta`, shared sizes and record caps

**Priority:** 1 — Mumbai release.

**Implemented:** record metadata and `Record::meta()`, shared receipt-effect accounting,
explicit genesis `RecordLimits`, atomic final-count checks, and full/projected/query
reads. Database format remains 1 because there are no deployments yet; this iteration
adds no format-version bump or migration. Pricing and budget enforcement remain deferred.

**Outcome:** every user record carries the metadata needed for planning and deletion bounds.

1. Implement the specified `bytes32` encoding of four big-endian `u64` counters: `cells`, `cell_bytes`, `indexed_cells`, `index_bytes`.
2. Use the effect/size helper from iteration 1. Cell size is `(8 + name length) + (1 + value length)`; index size is `name length + 2 + value length`. System cells contribute to neither metadata nor receipt user counts.
3. Create `#meta` with each user record. Update it only when one of its counts changes; delete it with the record. Do not add it to reserved records.
4. Implement the per-record cell/indexed-cell caps and their genesis configuration. Compute resulting counts and reject a violating operation atomically.
5. Include `#meta` in full reads, support explicit projection and `Record::meta()`, and keep query materialization consistent with record reads. Reads return stored cells without validating metadata; patch and delete validate it before using its counters.
6. Keep database format 1 during pre-deployment development; no format-version bump or migration is needed for this iteration. Do not silently scan and repair records during ordinary metered calls.

**Acceptance:** create, update, kind change, removal, no-op, deletion and rollback preserve all four counters; equal-size replacements can leave metadata unchanged; cap failures leave cells and IDs unchanged; patch and delete reject missing or malformed metadata as a state error rather than inventing zero counts.

### Iteration 3 — Metering core and accounting transport

**Priority:** 1 — Mumbai release.

**Implemented:** a database-independent write meter and receipt accounting path.
Actual CRUD calls remain unpriced until planner/schedule integration.

1. Add typed `WeightName` constants using D10's `write.*` and shared `read.*` names,
   immutable `Schedule`, supported `WRITE_CODE_VERSION`, and checked arithmetic.
   Host weights remain inspectable but unused by GolemDB write pricing.
2. Add pure `PlanningStep` and `MutationCharge` pricing with explicit depths and the
   temporary `INITIAL_DEPTHS` constant. Distinguish an absent weight from zero.
3. Add `WriteMeter`, separating completed and prospective cost/ledger counts.
   `run_step` charges a whole checkpoint before work; any metering failure stops
   further work. `finish` approves total cost or reports only charged planning.
4. Add per-weight ledgers and accounting propagation through `Outcome` and the public
   `Receipt`. Preserve cost/ledger on failures while zeroing effects. Map failures
   to the existing public `OutOfBudget { required }` shape.
5. Verify the worked example, exact cutoffs, composite record-base charging, system
   mutation prices, deletion prepayment, missing/zero weights, and overflow under
   limited and unlimited budgets. Tests reconstruct costs from ledgers.

**Acceptance:** exact-budget equality succeeds; unfunded actions never execute;
final refusal preserves planning cost and full required total; missing weights and
overflow stop both budget variants; zero-price steps still count; API conversion
preserves failure accounting. The original depth-4 example remains 266/24/22.

Admission already validates input in record execution. Its integration with these
checkpoints and exact request byte/entry counting belong to iteration 4.
This increment introduces no builder validation, CRUD planner, persisted schedule,
production enforcement, population counters or public read pricing.

### Iteration 4 — Record plan/apply separation and metered planning reads

**Priority:** 1 — Mumbai release.

**Outcome:** create, patch and delete have a shared, read-only planning path usable for execution and estimation.

**Implemented:** shared CUD planning/application, metered admission and state reads,
prospective user/system/index charges, bounded live-cell scanning, and the PR #87
metadata-decoding cleanup. `Records` requires immutable pricing and calls
`WriteMeter` directly. Public database writes use explicit zero weights with full
ledgers; tests supply nonzero schedules through the same constructor. Persisted
schedules and their production connection remain iterations 5/6. Public
estimation remains iteration 7.

1. Refactor current record CRUD, which interleaves reads and overlay writes, into internal planning and application functions. First establish equivalent behavior with a supplied test pricing context.
2. Call `WriteMeter` directly from record planning and admission. Pin deterministic input validation order, byte/entry counting, enforcement of existing genesis limits and bounded error formatting alongside execution. Add no separate request-size or change-count caps. Charge each check before doing it and preserve charges on validation failures. Check admission before record reads, binding before metadata, and subsequent reads/mutation evaluation in the pinned order.
3. Build user mutations, effects and metadata deltas from the observed values. Derive index charges, including the base-absence creation surcharge.
4. Add the origin index-term point lookup to the index/branch access interfaces. Keep the branch unaware of metering weights or record-operation budgets.
5. Make delete enumeration incremental. Do not collect all old cells/values before the budget checkpoints. Audit the merged overlay iterator's skipping of hidden/tombstoned entries so a logical read has a bounded, justified work envelope; counting iterator calls alone is not sufficient evidence.
6. Retain old values needed by application. Planning must not consume an ID, modify metadata/counters, append undo entries or alter the branch version.
7. Keep planning, final approval and application inside one existing branch guard. No intervening write may invalidate the plan. Avoid re-entering branch access from inside that guard.
8. Keep plans internal. The current journal-length branch version can decrease on rollback and is not a unique state identity for a reusable public plan.
9. Address the deferred [PR #87 review comment](https://github.com/Arkiv-Network/golemdb/pull/87#discussion_r4348012982) in `crates/record/src/meta.rs`: remove the `try_into().unwrap()` from `RecordMeta::decode`, return `RecordError::CorruptState` on conversion failure, and propagate it with `?`. The current `[u8; 32]` input and fixed offsets guarantee eight-byte slices, so this is a production-code cleanup rather than a known malformed-input panic. Preserve the encoding and existing count validation.

**Acceptance:** an instrumented read source proves that no later logical reads run after a cutoff; early refusal and validation failures change no branch state; no-op patches still perform required reads but create no mutations; all three operations apply exactly their approved plan without a second pricing/validation pass.

### Iteration 5 — Persisted initial pricing context

**Priority:** 1 — Mumbai release.

**Outcome:** production calls load an initial schedule from committed state,
including after restart. Depths remain the temporary `INITIAL_DEPTHS` constant.

1. Define genesis encoding/loading of the initial `@meteringModel` and `@modelWeight`
   admin records and necessary model configuration. Reuse the deployment caps
   already implemented in iteration 2. Validate declared names and code versions;
   retain missing weights as unpriced. Cache immutable configuration.
2. Construct an immutable pricing context with the committed pricing head and
   schedule, pinned for each call/branch. Keep storage outside the pure meter.
3. Rebuild pricing state on open/recovery. Unsupported active code versions or
   malformed schedules prevent priced service; absent individual weights fail only
   operations needing them. Expose missing-weight diagnostics to operators.
4. Use fixed depth 6 for both tries. Defer `#liveCells`, `#indexTerms`, their genesis
   values, net commit tracking, sealing/root changes and population-based depths.
   Their eventual implementation must count actual net state, not receipt effects.

This iteration can proceed separately from the planner refactor. Runtime weight
patches, model installation and delayed activation remain iteration 8.

**Acceptance:** reopening restores identical prices; the context has a coherent
head/schedule pair; missing weights remain distinguishable from explicit zero;
unsupported or malformed initial models cannot silently use fallback prices.

### Iteration 6 — Enforce real CUD prices and budgets

**Priority:** 1 — Mumbai release.

**Outcome:** all three write operations return actual charges and reject unfunded work before application.

1. Replace the API's explicit zero schedule with the persisted pricing context. All CUD paths already use `WriteMeter`; verify their nonzero production prices in reviewable increments.
2. Verify admission, base, cell-read and index-read charges against the persisted schedule; retain the existing accumulation of user/system mutation prices and final approval before applying.
3. Return the full approved cost on success. On planning failure return only completed planning cost, zero effects, and the specified error/`required` value. Forward the meter's per-weight ledger separately from user-effect `Details`; it is always present in the in-process receipt, including system charges and failed planning.
4. Preserve receipts across rollback/discard; there are no refunds or additional rollback charges. Commit overhead and final global-counter/allocator persistence do not become additional client-call charges.
5. Add the simplified index rule to the model documentation and golden examples in the same change that enables it.
6. Keep `get`, `query` and `count` at zero cost with their correct receipt context. Document that their budgets remain unenforced until read metering lands; do not describe them as budget-bounded reads.

**Acceptance:** end-to-end conformance examples cover fields and attributes, system mutations, no-ops, preserved-kind `set`, caps, missing cells, exact cutoffs, missing weights and overflow. Repeated joins and remove/recreate sequences match the explicit base-term rule. Failed planning leaves IDs, overlay state and counters untouched.

### Iteration 7 — Shared estimation and deletion bounds

**Priority:** after Mumbai release — not priority 1.

**Outcome:** hosts can estimate an operation's database cost without executing it, using exactly the execution planner, and obtain deletion-cost bounds. Conversion from database cost to a monetary fee remains the host's responsibility.

1. Add the cost-estimation interface described by the API design, with explicit `Limited`/`Unlimited` budgets and captured pricing context. Finalize its Rust signature alongside the endpoint rather than exposing internal plans.
2. Invoke the same planner but skip application. Return prospective effects as estimate data, clearly distinct from effects of an executed write.
3. Verify estimation neither consumes record IDs nor changes metadata, counters or rollback journals. Execution against unchanged state and the same schedule must have the estimated cost.
4. Implement the D5 deletion bound from metadata, the applicable weights and captured depths. Test that it bounds actual delete cost under the same pricing context; it is not a guarantee after a future price/depth change.
5. Keep host authorization of unlimited estimation outside the pricing core. A multi-operation estimate needing earlier writes visible to later ones uses a disposable simulation branch.

**Acceptance:** estimate/execution agreement for all CUD shapes; limited estimates stop at the same checkpoints; unlimited estimates still reject overflow, missing weights and invalid input; estimates leave state unchanged; deletion-bound cases cover indexed and non-indexed records at caps.

### Iteration 8 — Runtime schedule administration and activation

**Priority:** after Mumbai release — not priority 1.

**Outcome:** hosts can install models, update weights and activate schedules according to D9 without changing a call's prices midway. Mumbai uses the persisted initial schedule from iteration 5.

1. Implement install/patch/introspection through the specified administrative API, including supported GolemDB/host code versions and host weight names.
2. Enforce one pending model, monotonic model versions and the minimum activation delay. Validate known active-model names and the specified weaker form checks for unsupported pending models.
3. Publish a new head and its corresponding schedule coherently after a successful commit. Failed/discarded administration must not change active pricing.
4. Handle activation, missing/ignored weights and unsupported active versions as specified. Missing weights are not silently zero and do not require disabling unrelated priced operations.
5. Provide reconstruction/invalidation hooks for recovery and any future rewind support. Do not build rewind or branch-scoped administration just for this milestone.

**Acceptance:** boundary commits use the correct old/new schedule; active-model patches affect subsequent calls only; in-flight calls keep captured prices; restart agrees with live activation; host weights share the same pricing context but do not alter database charges unless the host uses them.

### Iteration 9 — Calibration and release checks

**Priority:** after Mumbai release — not priority 1.

**Outcome:** broader conformance testing and benchmark-based weight calibration provide evidence for the model's prices and budget behavior. This extends the focused correctness tests already required in iterations 1–6.

After Mumbai, benchmark the implementation, then select calibrated weights and document their measurement basis. Include largest allowed values/records, index posting updates, cold and warm storage, overlay scans, failed planning, and deferred trie/history work covered by write prices. Verify the modeled-depth and base-term approximations against these workloads; the simplified tariff alone is not proof of a safe resource bound.

Run the completed conformance suite across supported storage implementations, with restart and rollback cases. Equivalent logical calls at the same state/schedule must produce the same receipt regardless of caches and physical I/O. Also verify actual sealed contents/roots, so matching receipts cannot conceal incorrect index or metadata maintenance.

If some prepaid deferred work is not implemented yet, mark that calibration evidence as outstanding rather than presenting placeholder weights as verified production prices. Public read pricing remains a separate release/design milestone.

## 6. Dependency order and completion criteria

```text
1 API / request contract
  -> 2 metadata / caps
    -> 3 metering core
      -> 4 planner and planning reads ----+
      -> 5 persisted initial pricing ----+-> 6 enforced CUD
                                               -> 7 estimation / bounds
                              5 + 6 ----------> 8 schedule lifecycle
                                               -> 9 release checks
```

Iterations 4 and 5 can be developed separately against agreed interfaces. Iterations 7 and 8 are also separable after their prerequisites. Within each iteration, split commits by crate or operation only when the intermediate state remains clear and usable.

**Mumbai completion:** iterations 1–6, including their acceptance checks, deliver functional CUD metering using a persisted initial schedule. Public estimation/deletion bounds, runtime schedule changes and the broader calibration/verification work are not Mumbai requirements.

**After Mumbai:** iterations 7–9 complete the host-facing facilities, runtime lifecycle and broader calibration/verification.

The full plan is complete when every CUD path has deterministic admission and planning charges, checks budgets before charged planning work, applies only a fully approved plan, returns correct receipts on success/failure, maintains per-record metadata, and supports execution/estimation and schedule updates from coherent persisted state, with the calibration evidence from iteration 9. API receipts alone, metadata alone, or a pure cost calculator are useful increments but do not meet the Mumbai completion criterion either.
