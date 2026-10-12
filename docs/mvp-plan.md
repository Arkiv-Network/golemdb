# MVP plan: first Arkiv + GolemDB testnet

Status: draft, 2026-10-12, for iteration.

## Goal

A **meaningful testnet in 14 days** (target: 2026-10-26) that runs Arkiv on GolemDB and is good
enough 

Testnet ready for the first hackathons. Hackathon developers will create, update and delete entities,
read them back, query them by attribute, and look up their blocks and transactions over RPC.

## Principles for these two weeks

- **Integration first.** The biggest risk is not a missing feature but Arkiv and GolemDB not
  fitting together. The plan is built around one vertical slice that Arkiv can run end to end
  early, and widened as far as the testnet needs. Any additional features come after features required for hackathons.
- **Final API shape, thin implementation.** Every call has its target signature from the start
  ([golem-db-api.md](golem-db-api.md)); what is not built returns `NotImplemented`. Later work
  fills in behaviour without moving the contract.
- **Freeze early.** The API the integration builds on is frozen within the first days; later
  changes need a reason the integration owner agrees to.
- **Resets are allowed.** The testnet may be reset, so stored encodings and genesis parameters
  need not be final yet.

## MVP scope

| In | 2nd priority or later |
| --- | --- |
| Record calls and branches in the target API shape: `Metered<T>`, `Budget`, `RecordOp` with an optional key ([api-priorities.md](api-priorities.md) priority 1) | Generated record keys |
| `#meta` on every user record | Enforcing the per-record caps |
| Query MVP: `Eq`, `In`, `Range`, `And`; `count`; `limit` / `offset`; record-ID order ([query-proposal.md](query-proposal.md)) | `Or`, standalone `Not`, `Prefix`, `Exists`, sort, cursor |
| Immutable logs MVP: append on an open or sealed branch, get by ordinal and by key, `range_of`, `rows_of`, stored in an uncommitted MDBX table |   |
| Persistence and restart on MDBX | History, historical reads, GC, pruning, proofs, `rewind` |
| Receipts on every call. Write metering, if ready by the feature freeze (see Deep): `create`, `patch` and `delete` then report real costs; reads report 0 until read metering exists | Read metering for queries; the metering admin API |
| Gas charged by Arkiv on its own side (intrinsic plus bytes), whatever the receipts report | GolemDB costs converted into Arkiv's gas (needs the fee mapping) |

Two simplifications keep this feasible:

- **Expiry needs only equality** if Arkiv purges every block: "entities with
  `expiresAtBlock = N`" while building block N.
- **Arkiv translates its query language into the query tree.** That translation is Arkiv-side
  work, to be assigned on that side.

## People

| Person | Primary assignment | Status |
| --- | --- | --- |
| Raj | Arkiv + GolemDB integration: reth modifications, the Arkiv node against the `Api` trait, the integration contract | working assumption |
| Francesco | The query part: query MVP per [query-proposal.md](query-proposal.md), on `feature/golem-db-api` | working assumption |
| Deep | Load and performance tests, then write metering | [below](#deep-load-and-performance-tests-then-write-metering) |
| Kris | API completion per [api-priorities.md](api-priorities.md), then API owner during integration | [below](#kris-api-completion) |
| Emanuel | Support Raj on the integration, starting for example with the immutable logs MVP | [below](#emanuel-support-raj-on-the-integration); focus to agree with Raj |
| Matthias | MVP contract and decisions, specs, acceptance criteria | to be detailed |

## Tasks

### Deep: load and performance tests, then write metering

**1. Load and performance tests (first, on the critical path).**

- An end-to-end load test with an Arkiv-like workload: entities created, patched and deleted per
  block, queries per second, block production through seal and commit.
- Report what a block can carry within the target block time. That number sets the testnet's
  block gas limit and is the early warning for performance problems.

**2. Write metering (second, into the testnet only if ready by the feature freeze).**

Implement the write cost model of [metering](golem-db-metering.md) D2 to D5, which is specified
and has been reviewed, so `receipt.cost` carries real costs for `create`, `patch` and `delete`.
It changes no signature. Simplifications for the testnet, each allowed by the spec:

- a constant modeled trie depth instead of the global counters (metering D3, Open Question 1);
- weights in code, as metering model version 1, instead of the `@modelWeight` records and the
  admin API;
- charges counted during execution, with the branch rolled back on `OutOfBudget`, instead of a
  separate planner; nothing is written and the stopping point is deterministic;
- a simple admission charge, per entry and per byte;
- an index lookup per join, so that "terms created" can be charged (`w_idx_term_create`).

**Why both go to one person:** both work in the write path. The load tests produce the measured
work per operation class that calibrating the metering weights needs (metering D2, *Calibration
and verification*).

**Depends on:** the frozen API shape (`Metered`, `Budget`) for step 2.

### Kris: API completion

**Focus: complete the API to its target shape** ([api-priorities.md](api-priorities.md),
priority 1), so the integration builds on the final contract:

- `Metered<T>` and `Budget` first, since others build on them;
- then `RecordOp` with an optional key, never-failing builder steps, `set` keeping the stored kind
  and no `insert`, and `#meta`.

When it is done, the API is frozen for the MVP, and Kris is its owner during the integration: first
responder when the integration hits the API, and gatekeeper for any later change. What follows is
decided in a later iteration.

### Emanuel: support Raj on the integration

**Main goal: take load off Raj's critical path.** The integration is the plan's biggest risk and
currently rests on one person. 
Top priority is Raj and Emanuel agreeing and understanding the focus.

A potential first task is the **immutable logs MVP** ([design §11](golem-db-design.md#appending-to-a-branch)):
appends on open or sealed branches, gets by ordinal and by key, `range_of`, `rows_of`, over an
uncommitted MDBX table, including the API part of these calls. Arkiv's block and transaction RPC
depends on it. If Emanuel takes it:

- it builds on Kris's `Budget` and `Metered` types;
- staged rows must follow the branch (recorded per frame at a checkpoint, dropped on rollback,
  persisted with the commit), which touches the branch and commit code, so the hooks are agreed
  with Kris early.

Other Arkiv-facing work Raj may hand over instead or afterwards: translating Arkiv's query language
into the query tree, the RPC lookups over the immutable-data API, the devnet setup.

## Documents still to write

| Document | Owner | Feeds | Needed by |
| --- | --- | --- | --- |
| Read cost model for queries (metering D6, from the query proposal's execution rules) | Matthias | Francesco's charge checkpoints, Deep's weights | while the query evaluator is written |
| Arkiv fee mapping: GolemDB cost → gas, failed calls, storage over time, prepaid expiry, block gas limit | Matthias | Raj's gas integration | once GolemDB costs are to be charged as gas |
| MVP contract: the API calls Arkiv uses, their semantics, what returns `NotImplemented` | Matthias, with Raj | everyone | mid week 1 |
| Testnet acceptance criteria and hackathon developer docs | Matthias | the testnet launch | week 2 |

## Milestones

| Days | Milestone |
| --- | --- |
| 1–2 | MVP contract and open decisions agreed |
| 3–4 | API in target shape; **API frozen for the MVP** |
| 5–7 | Query MVP and immutable logs MVP behind the frozen API; first Arkiv node producing blocks on a devnet |
| 8–10 | End to end on the devnet: entities, queries, RPC lookups; load test results. **Feature freeze on day 10** |
| 11–14 | Stabilize, fix, deploy the testnet, finish the docs |

## Decisions needed

1. The query tree replaces the ordered DNF (D01), and the query MVP operators.
2. Cap values, or no caps for the MVP.
3. Immutable logs in an uncommitted MDBX table for the MVP.
4. Testnet resets allowed.
5. Arkiv charges gas on its own side at launch. Receipts report write costs once write metering
   lands, and 0 for reads until read metering; converting them into gas waits for the fee mapping.
