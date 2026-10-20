# Write metering implementation plan

Implement the public receipt contract first, then per-record metadata, then budgeted write planning and execution. Metering orchestration belongs in `golemdb-record`; a new `golemdb-metering` crate owns pricing rules, weights and budget accounting. The branch crate continues to provide state access, isolation, rollback and commit.

This plan covers `create`, `patch` and `delete`, including their planning reads. `get`, `query` and `count` receive the new result shape immediately, but their pricing remains deferred until the read-metering design is complete. Returning a receipt does not yet imply that a call enforces a work budget.

**Release scope:** iterations **1–6 are priority 1 for the Mumbai release**. Iterations **7–9 are after Mumbai and are not priority 1**: estimation/deletion bounds, runtime model and weight updates, and broader calibration/verification. The focused acceptance tests in iterations 1–6 remain part of Mumbai scope.

Sources: [metering specification](golemdb-metering.md), [API receipts](golemdb-api.md#receipts), [agreed API changes](golemdb-api.md#agreed-changes-not-yet-implemented), and [API priorities](api-priorities.md). This is an implementation proposal, not a report of implemented functionality.

## 1. Decisions and boundaries

- Change the API before introducing actual charges. All six operations return `Metered<T>`, including failures. Initially `cost = 0`; successful write effects and `priced_at` must already be correct.
- Add `#meta` before enforcing write prices. Its four counters describe one record's user cells, not database-wide populations or accumulated operation charges.
- The record crate builds and applies write plans. The metering crate prices logical steps and mutations; it does not discover old values, access storage or construct an executable plan.
- Planning reads are budgeted while the plan is being built. Building an unrestricted plan and pricing it afterward would not meet the specification.
- Use index-term existence at the branch base for the initial creation surcharge. Do not implement a branch-aware index overlay for pricing in this work.
- Keep actual index maintenance and global population counts exact. The pricing simplification does not change committed database contents.
- Leave public read pricing, immutable-data pricing, and unrelated API features outside this work. In particular, this plan does not require implementing generated record keys or a new query planner.

## 2. Ownership and interfaces

| Layer | Responsibility |
| --- | --- |
| `api` | Public requests, budgets, `Metered<T>`, receipts and error conversion; genesis/admin integration and database construction. |
| `record` | Admission, record semantics, metered planning reads, `WritePlan`, metadata and effect calculation, final approval and application. |
| `metering` (new) | Typed schedules, supported model identifiers, checked cost arithmetic, modeled depths, logical operation prices and per-call budget accounting. |
| `branch` | A stable state view while planning/applying, origin/head validation, overlay mutation, rollback, sealing and commit; access to base state needed by record planning. |
| `cells` / `index` | Encodings and storage primitives, including a point lookup that checks whether a term exists without loading its posting list. Record calls using these primitives are charged through the meter; the storage crates neither choose prices nor track a caller's budget. |
| `query` | Existing query planning/execution and receipt context now; its own metered execution later. |

### The metering crate does not create plans

The record layer knows what a patch means, which old cells are needed, whether a change is a no-op, and which system cells must change. These decisions produce a record-layer `WritePlan` containing:

- The resolved record identity and the old values needed for application.
- User-cell mutations and resulting user-effect counts.
- Per-record system mutations, including `#meta`, `#key` and the binding where applicable.
- Any proposed allocator change, without changing the allocator during planning. For example, a create can propose record ID `42` and next allocator value `43`; only approved application advances the allocator. Failed planning or estimation leaves ID `42` available. This is the internal numeric record ID, not the caller's record key.
- The approved total cost and the captured pricing context.

Index joins/leaves are derived and priced during planning. Their approved charges are included in the write's receipt, even though the physical index changes happen later during sealing. Sealing derives the final net changes and does not charge them again. For two patches changing an attribute `red -> blue -> green`, each patch pays for its leave and join, while sealing can apply the net `red -> green` change.

The old user-cell value comes from current branch state, including earlier writes. Only the term-existence decision for the creation surcharge uses committed branch-base index state; the simplification does not make record planning ignore earlier branch writes.

`WritePlan` belongs to the record crate. A future plan for record reads, if needed, would also belong there; a query plan belongs to the query crate. Write-planning reads do not require a separate `ReadPlan` object: the write planner performs them incrementally through metered helpers. The metering crate accepts charge descriptions and has no executable read or write plans.

### Proposed metering interface

The following describes responsibilities rather than a final Rust API. One `WriteMeter` instance owns all accounting for one call: its budget, completed planning cost (`spent`), prospective mutation cost, and optional diagnostic counts. The record layer owns and uses that instance but does not maintain a second balance or manually subtract charges.

```rust
enum PlanningStep {
    AdmissionBase,
    AdmissionEntry,
    AdmissionBytes(u64),
    RecordBase(RecordOpKind),
    CellRead,
    IndexRead,
}

enum MutationCharge {
    Cell { kind: CellMutationKind, bytes_written: u64 },
    Index {
        kind: IndexMutationKind,
        bytes_written: u64,
        charge_term_creation: bool,
    },
}

struct ModeledDepths {
    cell_trie: u8,
    index_trie: u8,
}

// Opaque, single-use approval for one planning step; fields are private.
// Exact representation is an implementation choice.
struct StepCharge {
    step: PlanningStep,
    amount: u64,
}

// Conceptual methods; signatures and ownership can follow crate conventions.
WriteMeter::new(schedule, modeled_depths, budget)
meter.check_step(step) -> StepCharge | MeteringFailure
meter.complete_step(charge)
meter.add_planned_mutation(mutation) -> () | MeteringFailure
meter.spent() -> u64 // Completed planning cost, including on a non-budget error.
meter.finish() -> ApprovedCost | MeteringFailure
```

`modeled_depths` contains the cell-trie and index-trie pricing depths from D3. Derive them from the committed `#alloc.#liveCells` and `#alloc.#indexTerms` counters at the branch base using the pinned table (depths 4, 5, 6 or 7). These are pricing approximations, not measurements of actual traversed paths. For example, the cell path charge is `cell_trie * w_cell_trie_update`. The counters are not implemented in the current crates; iteration 5 adds their initialization, persistence and maintenance. The arithmetic can be implemented and tested with supplied counts in iteration 3.

`PlanningStep` describes work actually performed while building the plan: input checks, the binding lookup, and cell/index reads. It does not describe a future mutation. For each step:

1. `check_step(step)` looks up its price, checks arithmetic and verifies that `spent + price` fits the limited budget. On failure it returns `OutOfBudget` before the step runs. On success it returns a `StepCharge` containing the approved amount and the step kind for accounting/diagnostics. It does not yet add that amount to `spent`.
2. The record helper performs the approved work.
3. `complete_step(charge)` consumes the approval and records its amount in the meter's `spent`. It does not calculate a second price, ask the record layer to update a balance, or charge twice.

`StepCharge` is an internal approval token, not a receipt or a separate budget. It must be single-use, completed on the meter that issued it, and used in a serialized check/work/complete sequence. Hide this sequence behind a helper or guard; callers must not accumulate multiple approvals against the same unchanged balance. The exact token/guard representation can be chosen during implementation.

A successful read returning “missing” is a completed, charged read. A validation check that detects invalid input is also completed and must be charged before its domain error is propagated. Local storage faults are operational failures, not deterministic validation outcomes; define their accounting at the helper boundary rather than letting an incidental early return decide it.

`add_planned_mutation` accumulates prospective write cost without adding it to `spent`. `finish` compares the complete total with the budget. It either approves application or returns `OutOfBudget` with the completed planning cost and `required: Some(total)`. An earlier budget stop, missing required weight or overflow returns `required: None`. No extra reads run just to discover a full required price after an early stop.

For example, with budget `10`, planning `spent = 4`, and `w_cell_read = 2`, `check_step(CellRead)` approves `2` while `spent` stays `4`. After the read, `complete_step` advances `spent` to `6`. A later planned mutation costing `5` leaves `spent` at `6` and makes the proposed total `11`; final approval fails with receipt cost `6` and `required: Some(11)`. With budget `11`, application succeeds and the receipt cost is `11`, including the planning cost once.

The core crate also exposes pure pricing/size-independent arithmetic helpers and schedule validation. It must distinguish a missing weight from an explicit zero. `Unlimited` disables budget comparisons only; validation and checked arithmetic still apply.

Keep `CommitId` and database access outside this core: `CommitId` currently belongs to the branch crate. A record integration context can combine the origin commit, an immutable schedule and modeled depths. Avoid creating a dependency from `metering` back to `record`, `branch` or `api`.

### Record-side planning reads

Add a small record-side adapter over the existing branch read view and `WriteMeter`:

```text
check logical read charge
perform the corresponding state read
record the completed charge
return the observed value to the planner
```

Conceptually, the adapter's cell-read helper contains:

```rust
let charge = meter.check_step(PlanningStep::CellRead)?;
let old_value = reader.get(key)?; // A successful None is still a completed read.
meter.complete_step(charge);
Ok(old_value)
```

This illustrates the normal read path; the helper must also explicitly handle the local-storage-fault policy described above. Record planning calls `read_cell_metered(...)` rather than repeating this sequence throughout CRUD. Whether a missing cell means create, no-op or `CellNotFound` is decided after this read has been charged. For admission validation, record the completed check before returning the validation error.

Use it for `#meta`, touched user cells and index terms. The binding lookup is covered by `w_rec[op]`. Configuration reads and existing identity/integrity checks need an explicit mapping to the reference procedure; do not automatically charge every physical `get` as `w_cell_read`.

This is the read machinery needed first. It is small and reusable in principle, but it does not depend on finalizing Get/Query read prices, sorting, intersections or pagination accounting.

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

The receipt's `index_joins` remains the actual number of logical joins. Do not label the number of creation surcharges as “terms created.” The committed `#indexTerms` counter comes from actual net index changes at seal time.

## 4. Worked example — creating a record in the record crate

Consider a caller-keyed create with two user cells:

```text
field:     age = i32(30)
attribute: color = str("red")
```

Assume the key is unused, the next internal record ID is `42`, and the `color = "red"` term is absent from the branch-base index. The record starts with zero metadata counts. No old user cells or old `#meta` need reading for a create.

### Planning and execution

This pseudocode runs in the record crate. A helper keeps each planning step in the required check/work/complete order:

```text
charged_step(step, action):
    approval = meter.check_step(step)     // Stop before work if unaffordable.
    outcome = perform action
    meter.complete_step(approval)        // Record completed work, including a
    return outcome or its error          // check that detects invalid input.
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

    PLAN — read state and prepare changes; do not write anything:
        charged_step(AdmissionBase, validate fixed request fields)
        for each input entry in the defined admission order:
            charged_step(AdmissionEntry, validate header and declared lengths)
            n = validation byte count from those lengths
            charged_step(AdmissionBytes(n), validate name and value bytes)

        binding = charged_step(RecordBase(Create), read key binding)
        if binding exists: fail with AlreadyExists

        Inspect the allocator and verify the next ID is usable.
        Propose ID 42 and next allocator value 43; leave the allocator unchanged.

        for each user cell in name order:
            Add its create to the plan and update user-effect/metadata counts.
            if it is an attribute:
                exists = charged_step(IndexRead, look up its term at branch base)
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

On an early failure, completed cost comes from `meter.spent()`; a `finish` failure carries it in the failure result. Planned mutation costs are not spent until successful application. Application reuses the prepared values and makes no further pricing decisions or state reads.

Allocator work is covered by `w_rec[create]`; the allocator is excluded from the priced cell creates. Persistence of its final value is host-funded. The planned index joins are charged now; sealing later applies the net index changes without charging again. Estimation follows the same flow through approval and skips application.

Admission includes retained builder errors at their defined checkpoints. Its exact counting and limits are pinned in iteration 3. Missing weights and arithmetic overflow also stop planning with only completed work charged.

### Concrete charges and receipts

Use these illustrative test weights, with both modeled depths equal to `4`. For this example's admission convention, the fixed key/form checks belong to the base; each of the two entries has one header check, and validated bytes are the cell-name bytes plus value payload bytes. That is `(3 + 4) + (5 + 3) = 15` bytes. This is an example fixture, not a finalized admission rule or a calibrated schedule.

| Component | Test weights / calculation | Cost |
| --- | --- | --- |
| Admission | base `2`, entry `1`, byte `1`: `2 + 2 × 1 + 15 × 1` | 19 |
| Record base | `w_rec[create] = 3` | 3 |
| Index read | `w_idx_read = 2` | 2 |
| Cell creates | `w_cell[create] = 5`, trie-node `1`, byte `1`: five cells at `5 + 4 × 1`, plus 173 bytes | 218 |
| Index join | join `3`, trie-node `1`, term-create `7`, byte `1`: `3 + 4 × 1 + 7 + 10 × 1` | 24 |
| **Total** | **Planning 24 + prospective mutations 242** | **266** |

The five cell sizes are `age: 16`, `color: 17`, `#key: 45`, `#meta: 46`, and the binding: `49`, totaling `173`. The new metadata is `{ cells: 2, cell_bytes: 33, indexed_cells: 1, index_bytes: 10 }`. All user and per-record system creates contribute to cost; only the two user cells contribute to effect details.

| Budget / outcome | Work completed | Returned accounting |
| --- | --- | --- |
| `266`: success | Full planning; five cell creates and allocator change staged | Cost `266`; user effects below. |
| `265`: final refusal | Full planning; no writes or allocator change | Cost `24`; `OutOfBudget { required: Some(266) }`; zero effects. |
| `23`: early refusal | Admission and binding/base work; index read cannot fit | Cost `22`; `OutOfBudget { required: None }`; zero effects. |
| Sufficient budget, key already exists | Admission and binding lookup only | Cost `22`; `AlreadyExists`; zero effects. |

The API facade maps `RecordOutcome` to public `Metered<T>` without recalculating charges or rereading head. For the successful case at pricing commit `100`, it returns:

```text
result: Ok(the requested record key)
receipt:
  cost: 266
  priced_at: Some(100)
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

**Outcome:** callers can adopt the final result shape before the pricing implementation exists.

1. Add public `Metered<T>`, `Receipt` and `Details`, with the fields and helpers in the API specification. Preserve receipts when mapping errors; dropping a receipt must be explicit through `into_result()` or equivalent.
2. Return `Metered<RecordKey>` / `Metered<Record>` / the existing operation payload from CRUD, and `Metered<QueryPage>` / `Metered<u64>` from query/count. Preserve current payload semantics.
3. Carry execution context and user effects out of the lower layers on success and failure. The API crate converts these internal reports to public receipts; lower layers must not depend on `api`.
4. Capture `priced_at` from the same state selection used by execution. Branch calls use their validated origin; committed reads use the captured head even when reading an older commit. Query admission errors also need the captured head when snapshot selection succeeded. Do not perform a separate head lookup after execution.
5. Compute accurate user effects for successful writes, including no-ops. Failures and reads report zero effects; system cells are excluded. Introduce one shared record-layer effect/byte-counting helper for later metadata and planning use.
6. Change builders to retain invalid steps and report them when the call executes, with a receipt. Preserve enough input/order/count information to reproduce admission charges later, not merely an error string.
7. Make budgets explicit in requests, selecting one construction convention for CRUD/query/count. A required constructor argument is the recommended default. Initially retain/re-export the existing query `Budget` type at the API boundary; do not make record depend on query. Move the type to the metering crate in iteration 3.
8. Align `set` with the documented preserved-kind behavior and `CellNotFound`, since these determine effects. Fix handle/admission/state validation order. Update API consumers, examples, mocks and error handling together.

For reviewability, split this iteration into result/receipt plumbing and request/semantic alignment if needed. Both precede metering enforcement. Cost remains zero and budgets are explicitly documented as unenforced during this increment.

**Likely areas:** `crates/api/src/{lib.rs,database.rs,record_op.rs}`, record outcomes and effect helpers, `crates/query/src/queries.rs`, API/integration tests.

**Acceptance:** receipts survive ordinary success and error paths; historical failures report the head used for pricing; invalid handles have zero cost and no pricing snapshot; a concurrent head advance cannot mix receipt context with execution state; actual user effects match changes, including no-op and failed patches.

### Iteration 2 — `#meta`, shared sizes and record caps

**Priority:** 1 — Mumbai release.

**Outcome:** every user record carries the metadata needed for planning and deletion bounds.

1. Implement the specified `bytes32` encoding of four big-endian `u64` counters: `cells`, `cell_bytes`, `indexed_cells`, `index_bytes`.
2. Use the effect/size helper from iteration 1. Cell size is `(8 + name length) + (1 + value length)`; index size is `name length + 2 + value length`. System cells contribute to neither metadata nor receipt user counts.
3. Create `#meta` with each user record. Update it only when one of its counts changes; delete it with the record. Do not add it to reserved records.
4. Implement the per-record cell/indexed-cell caps and their genesis configuration. Compute resulting counts and reject a violating operation atomically.
5. Include `#meta` in full reads, support explicit projection and `Record::meta()`, and keep query materialization consistent with record reads.
6. Specify the compatibility boundary for existing databases without metadata: use the repository's schema/version policy, with an explicit development reset or a separate migration if required. Do not silently scan and repair records during ordinary metered calls.

**Acceptance:** create, update, kind change, removal, no-op, deletion and rollback preserve all four counters; equal-size replacements can leave metadata unchanged; cap failures leave cells and IDs unchanged; malformed metadata is a state error rather than an invented zero value.

### Iteration 3 — Metering core and explicit budget plumbing

**Priority:** 1 — Mumbai release.

**Outcome:** deterministic pricing can be exercised without a database.

1. Add `golemdb-metering` to the workspace. Move `Budget` there and re-export it from the public API and existing query path where useful. Thread it to record operations without adding a record-to-query dependency.
2. Implement typed weights, supported model identifiers, immutable schedules, depth selection and checked arithmetic. Keep host weights available in the schedule but unused by GolemDB's own write pricing.
3. Implement the per-call meter interface described above, separating completed planning cost from prospective mutation cost. The meter owns the budget and all cost totals; the record layer does not duplicate them. Keep step approvals internal and single-use. Retain logical charge counts when diagnostic detail is requested, without changing the computed cost.
4. Price system-cell mutations as required by D2 even though receipt effects omit them. Do not turn every system-cell cost into admission cost.
5. Define the reference admission procedure: deterministic validation order, entry/byte counting, checkpoints, hard request-size/change-count limits and treatment of builder errors. Pin planning-read and mutation-evaluation order as part of the model too.
6. Add fixed test schedules and conformance vectors. These are test weights, not claims of calibrated production prices.

**Acceptance:** exact-budget equality succeeds; a step exceeding the remaining budget is refused before work; final refusal reports planning `spent` and full `required`; missing weights and overflow fail under both budget variants; explicit zero weights remain valid; modeled-depth thresholds and byte charges match the specification.

### Iteration 4 — Record plan/apply separation and metered planning reads

**Priority:** 1 — Mumbai release.

**Outcome:** create, patch and delete have a shared, read-only planning path usable for execution and estimation.

1. Refactor current record CRUD, which interleaves reads and overlay writes, into internal planning and application functions. First establish equivalent behavior with a supplied test pricing context.
2. Implement the record-side metered planning adapter. Check admission before record reads, binding before metadata, and subsequent reads in the pinned order.
3. Build user mutations, effects and metadata deltas from the observed values. Derive index charges, including the base-absence creation surcharge.
4. Add the origin index-term point lookup to the index/branch access interfaces. Keep the branch unaware of metering weights or record-operation budgets.
5. Make delete enumeration incremental. Do not collect all old cells/values before the budget checkpoints. Audit the merged overlay iterator's skipping of hidden/tombstoned entries so a logical read has a bounded, justified work envelope; counting iterator calls alone is not sufficient evidence.
6. Retain old values needed by application. Planning must not consume an ID, modify metadata/counters, append undo entries or alter the branch version.
7. Keep planning, final approval and application inside one existing branch guard. No intervening write may invalidate the plan. Avoid re-entering branch access from inside that guard.
8. Keep plans internal. The current journal-length branch version can decrease on rollback and is not a unique state identity for a reusable public plan.

**Acceptance:** an instrumented read source proves that no later logical reads run after a cutoff; early refusal and validation failures change no branch state; no-op patches still perform required reads but create no mutations; all three operations apply exactly their approved plan without a second pricing/validation pass.

### Iteration 5 — Persisted pricing context and global counters

**Priority:** 1 — Mumbai release.

**Outcome:** production calls can derive their schedule and modeled depths from committed state, including after restart.

1. Add genesis encoding/loading of the initial `@meteringModel` and `@modelWeight` records, caps, limits and activation-delay configuration. Cache immutable configuration rather than rereading it as an incidental planning expense.
2. Construct an immutable pricing context from the committed head and pin it for each call. Keep snapshot loading/storage integration outside the pure metering crate.
3. Add `#alloc.#liveCells` and `#alloc.#indexTerms`, which are specified but not yet implemented in the current crates. Initialize them at genesis and persist them as committed state. Live-cell counts include all CellTrie leaves, including reserved records, root-history cells and the counters themselves.
4. Update these populations once per commit from net changes, including actual term-root transitions. Do not sum receipt effects or creation surcharges to obtain committed populations.
5. Adjust sealing so final global-counter values are included before computing the final cell root. The current seal path applies cells before index changes; account for that ordering explicitly. Do not patch counters after the root has already been computed.
6. Derive modeled depths from the branch-base populations and keep them fixed for the call/branch origin, even if the branch grows.
7. Rebuild pricing state on open/recovery. Unsupported active code versions or malformed schedules must prevent priced service; missing individual weights remain absent and fail only operations needing them.

This iteration can proceed independently of the planner refactor after the core/schema contracts are agreed. Start with the persisted initial model; delayed runtime installation and patches are iteration 8.

**Acceptance:** reopen reconstructs identical prices and depths; empty/admin commits and root-history growth count correctly; create-delete net cancellation and last-member term removal give exact populations; counter cells are included in the committed root; concurrent calls retain a coherent head/schedule pair.

### Iteration 6 — Enforce real CUD prices and budgets

**Priority:** 1 — Mumbai release.

**Outcome:** all three write operations return actual charges and reject unfunded work before application.

1. Connect the planner to the persisted pricing context. Enable create, then patch, then delete in reviewable increments; do not claim complete write-budget enforcement until all three paths are covered.
2. Route admission, base, cell-read and index-read charges through the meter; accumulate the corresponding user/system mutation prices and approve the full total before applying.
3. Return the full approved cost on success. On planning failure return only completed planning cost, zero effects, and the specified error/`required` value. Expose the specification's optional per-part diagnostic ledger separately from user-effect `Details`; system charges and failed planning can appear in diagnostics even though they are not user effects.
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
      -> 5 committed pricing / counters -+-> 6 enforced CUD
                                               -> 7 estimation / bounds
                              5 + 6 ----------> 8 schedule lifecycle
                                               -> 9 release checks
```

Iterations 4 and 5 can be developed separately against agreed interfaces. Iterations 7 and 8 are also separable after their prerequisites. Within each iteration, split commits by crate or operation only when the intermediate state remains clear and usable.

**Mumbai completion:** iterations 1–6, including their acceptance checks, deliver functional CUD metering using a persisted initial schedule. Public estimation/deletion bounds, runtime schedule changes and the broader calibration/verification work are not Mumbai requirements.

**After Mumbai:** iterations 7–9 complete the host-facing facilities, runtime lifecycle and broader calibration/verification.

The full plan is complete when every CUD path has deterministic admission and planning charges, checks budgets before charged planning work, applies only a fully approved plan, returns correct receipts on success/failure, maintains metadata and committed populations, and supports execution/estimation and schedule updates from coherent persisted state, with the calibration evidence from iteration 9. API receipts alone, metadata alone, or a pure cost calculator are useful increments but do not meet the Mumbai completion criterion either.
