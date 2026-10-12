# Query API proposal

**Temporary working document.** It records the outcome of the query discussion of 2026-10-10 to
2026-10-12, so that the *Query* part of [golem-db-api.md](golem-db-api.md), which still assumes an
ordered DNF, can be replaced by what follows. Delete this file once the API spec and D01 carry it.

**Starting point:** branch `francesco/query` (revision `de4be83`, "Query equality - first draft"),
crate `golemdb-query`; see [Staging](#staging).

## Principles and requirements

### Principles

1. **No query optimizer in v1.** GolemDB does not choose how to run a query. The caller knows its
   data and optimizes by how it writes the query; a host or SDK may rewrite a query before
   submitting it, off the consensus path.
2. **GolemDB executes the query as written.** The written query is the execution plan.
3. **Query execution is deterministic.** Every node reaches the same result, the same cost and the
   same stopping point.
4. **Every query runs against a budget.** No query obtains more work than its budget pays for.
5. **Pay as you go.** Cost is counted step by step as the work happens, and execution stops at the
   first step the remaining budget cannot pay for.

Principles 1 and 2 are two sides of one decision: with no optimizer, the query's form must be the
plan. Principles 3 to 5 apply the metering spec to reads ([metering](golem-db-metering.md) R1, R3,
R5, R7 and D6: reads are counted, not modeled, and abort when they cross their budget).

### Requirements

| # | Requirement | From |
| --- | --- | --- |
| QR1 | `query` and `count` each take an **explicit budget**, `Limited(n)` or `Unlimited`, with no default, received the same way as a `RecordOp`'s. `Unlimited` is for host-authorized estimation only. | P4; metering R7, D8 |
| QR2 | `query` and `count` return **`Metered<T>`**: the result together with a receipt, on success and on failure, like the record calls. | P4, P5; API *Receipts* |
| QR3 | The **execution order is a pure function of the query**: the order the execution rules derive from the written filter, sort and page. No statistics, caches or warm state change it or the cost. | P1, P2; metering R5, D1 |
| QR4 | **Same query, same committed state, same pricing snapshot → same result, same cost, same stopping point** on every node and implementation. | P3; metering R1 |
| QR5 | **Order of checks** as for every call: the target first, at no cost; then admission, validating the whole query before any state read and charged as admission; then the state reads. | metering D2 *Order of checks* |
| QR6 | **Every step that reads state or combines sets has a known, bounded charge, checked before the step runs:** a term read, a term stepped over, a set combination, a sort-value fetch, a materialized cell read, and the modeled sort comparisons before a sort. A charge equal to the remaining budget fits. | P5; metering D2, D6 |
| QR7 | **The first charge that does not fit stops execution** without performing that step: `OutOfBudget { required: None }`, the spent cost in the receipt, and **no partial results**. | P4, P5; metering D8 |
| QR8 | **Caps bound what the budget cannot,** independently of the budget, which may be `Unlimited` ([Caps](#caps)). | P4 |
| QR9 | **Pricing comes from the head at admission,** also for a pinned query of an older commit; each page is a new call with its own pricing. | metering D9 |

The execution rules below are also the reference execution that metering finding F1 asks for:
they fix the read operations, their order and their charge checkpoints.

## The call

```rust
fn query(&self, target: ReadTarget, op: QueryOp) -> Metered<QueryPage>;
fn count(&self, target: ReadTarget, op: CountOp) -> Metered<u64>;

QueryOp::filter(f)                                       // the filter tree; required
    .sort([SortKey::asc("price", CellType::Uint(W8))])   // optional
    .only(["owner", "status"])                           // projection, as for get
    .limit(50)
    .after(cursor) | .offset(n)                          // at most one
    .with_total()                                        // ask for total_matched
    .budget(Budget::Limited(n))                          // required (QR1)

CountOp::filter(f)
    .budget(Budget::Limited(n))                          // required (QR1)

pub enum Budget { Limited(u64), Unlimited }

pub struct QueryPage {
    pub records: Vec<Record>,
    pub total_matched: Option<u64>,
    pub evaluated_at: CommitId,
    pub next: Option<Cursor>,
}
```

It follows the rest of the API:

- **The `RecordOp` pattern:** a typed builder whose steps never fail; the first invalid step, or a
  missing budget, is reported by the call.
- **The same `Budget`** as the record calls, received the same way. How a builder receives a
  required budget (a constructor argument, or a step checked at the call) is open for `RecordOp`
  too and decided once for both.
- **The same `Metered<T>` and `Receipt`:** `cost`, `priced_at` (the head at admission, QR9) and
  `details`, which are zero for reads.
- **The same `ReadTarget`:** `Head` (live paging) or `Commit(c)` (pinned). Queries read committed
  state only, so `Branch` is rejected.

`count` takes a filter and a budget, and runs rules 1 to 7 below: it evaluates the filter and
returns the size of the match set, with no sorting, paging or materialization.

## AST query format

A query's filter is an **abstract syntax tree**: `And`, `Or` and `Not` nodes over typed leaves.

```rust
#[non_exhaustive]
pub enum Filter {
    Eq     { field: CellName, value: Value },
    In     { field: CellName, values: Vec<Value> },      // one type
    Range  { field: CellName, lower: Bound<Value>, upper: Bound<Value> },
    Prefix { field: CellName, prefix: Value },           // str, bytesN
    Exists { field: CellName, ty: CellType },
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
}
```

- **Leaves are typed,** as in `golemdb-query`: `Value` carries the type, and a cell of another
  type under the same name is absent. `bytes` cannot be queried, being field-only.
- **`Ne`, `!=` and "not in" are `Not(...)`.** Absent and other-typed cells match a `Not`
  (MongoDB-style, D01 1.5).
- **`Exists` needs no extra index:** it is a scan over all terms of one `(field, type)` run.

### Why a tree

The tree is a natural fit for the principles:

- **The written query is the plan** (principles 1 and 2). A tree states both what to compute and
  in which order. The caller controls the order at every level, and a condition shared by several
  branches is written, and evaluated, once: `(a ∨ b) ∧ c` reads `c` a single time.
- **Deterministic by construction** (principle 3). A fixed traversal of a given tree yields one
  order of steps; nothing about it depends on statistics or node state.
- **Budget and pay as you go map onto nodes** (principles 4 and 5). Each node's work is a known
  sequence of charged steps, so a query's cost is the sum over the nodes actually evaluated, in
  written order, and the budget is checked before each step. An `And` whose running set is
  already empty stops, and its remaining children cost nothing.
- **Bounded by depth, paid for by size.** A depth cap bounds the recursion; a large tree pays for
  itself, since admission charges per node and every leaf costs at least one read.
- **It is what callers produce anyway.** A query language parser, an SDK builder or a filter UI
  yields a tree; submitting it unchanged avoids a conversion on the caller's side.
- **Flat forms remain available.** An OR of AND-groups is a tree two levels deep, and costs the
  same as one.

### Alternative considered: ordered DNF

The earlier API spec and decision D01 assumed an **ordered DNF**: a flat OR of AND-groups of
possibly negated predicates, evaluated in the written order. Its appeal is simplicity: one fixed
evaluation shape, no recursion, and an obvious cost bound of groups × predicates. Against the
tree it has these drawbacks:

- **The caller must normalize every query into DNF** before submitting it.
- **Normalization multiplies:** `(a∨b)∧(c∨d)∧(e∨f)` becomes 8 groups, and `n` such factors 2ⁿ,
  which runs into the group cap or the budget for queries that are cheap as a tree.
- **Shared conditions are repeated and read again:** `(a∨b)∧c` becomes `(a∧c)∨(b∧c)`, reading
  `c` twice. With no optimizer to factor it back out, the caller cannot write the cheaper plan.
- **Negating a group needs De Morgan expansion,** which multiplies again: `NOT (a ∧ b)` becomes
  `¬a ∨ ¬b`, and negating `k` groups of `m` predicates yields `mᵏ` groups.

## Execution rules (normative)

The written order of the tree is the execution order. Cost and stopping point are a function of
`(state, query)`; the result set never depends on the order.

1. **Validate everything first** (admission, no state reads; QR5). Admission is charged per node
   and per byte inspected (metering D2). It checks names, types and NaN; that `And` and `Or` are
   not empty, an `In` has values of one type, and range bounds share a type with lower ≤ upper;
   and the [caps](#caps).
2. **Evaluate depth-first, children left to right.**
3. **`And`:** the running set starts with the first positive child; each further positive child
   intersects it, each `Not` child subtracts from it. When the running set becomes empty, the
   remaining children are skipped and not charged.
4. **`Or`:** every child is evaluated in order and unioned; no early exit.
5. **`Not`** needs a set to subtract from: inside an `And`, after a positive child, the running
   set. A standalone `Not`, or one inside an `Or`, needs the set of all live records (product
   decision P08): `NotImplemented` until then, or `InvalidQuery` if P08 says no.
6. **Set combination.** Every `And`, `Or` and `Not` is charged for combining its children's sets,
   per bitmap container processed, so the budget also bounds set work that nesting would
   otherwise multiply.
7. **Leaves.** `Eq`: one term read. `In`: values sorted by term key and deduplicated, one term read
   each. `Range`, `Prefix`, `Exists`: step through the run's terms in ascending key order from the
   lower bound, charging every term stepped.
8. **Order.** With `sort`: per level, fetch each matched record's sort value in ascending record ID
   order and charge `⌈N log₂ N⌉ × S` comparisons (D14 14.1); ties fall to the next key, then to
   record ID ascending; absent values sort last. Without `sort`: record ID ascending, which is
   creation order.
9. **Page:** cursor or offset, then `limit`.
10. **Materialize** the projection for the page's records only, in page order, as point reads.
11. **`total_matched`** is the size of the match set, free.

Every charge is checked before its step (QR6), so `OutOfBudget` stops at the same point on every
node (QR4, QR7).

### Caps

Three genesis parameters in `#params`, each `u32`, fixed per deployment and readable with `get`:

| Cap | Bounds | Why the budget does not |
| --- | --- | --- |
| `#maxQueryDepth` | nesting depth of the filter tree | a deeper tree overflows the evaluator's stack, which aborts the process before or regardless of any charge |
| `#maxQueryLimit` | records per page | the size of one response and the memory to materialize it, also under `Unlimited` |
| `#maxSortKeys` | sort keys per query | the memory and work of one sort, also under `Unlimited` |

They are genesis parameters, not node settings, because they decide whether a query is accepted,
and Arkiv runs queries during block execution (its expiry purge), so every node must agree.

**Policy within physics.** What depth is safe depends on the implementation, so the code defines a
hard ceiling for each cap, versioned with the format, and genesis validation rejects a cap above
it, as it already rejects limits that would not fit an MDBX key (design §4). There is no
node-count cap: the size of a tree is paid for by admission's per-node charge and bounded by the
request-size limit of metering's admission procedure.

### Errors

| Error | Raised when | Cost |
| --- | --- | --- |
| `InvalidArgument` | a builder step is invalid (cell name, NaN), or the budget is missing | admission work done |
| `InvalidQuery` | the target is a branch; the tree is malformed (empty `And` / `Or`, mixed types in an `In`, mismatched or inverted range bounds); a cursor does not match its query | 0 for a branch target, else admission work done |
| `LimitExceeded` | a cap is exceeded | admission work done |
| `NotImplemented { operation }` | a supported-later part is used, such as `"query: sort"` | admission work done |
| `OutOfBudget { required: None }` | a charge does not fit (QR7) | the work done before it |

### Helping callers optimize

- **Compare before submitting.** An estimate with `Unlimited` budget, plus the receipt's ledger,
  shows what a query costs, so two orderings can be compared.
- **A possible later leaf, `Check { field, op, value }`,** filters the running set by reading each
  candidate's cell instead of scanning the index. It lets the caller choose "few candidates, read
  their cells" over "scan a wide range", a choice a planner would otherwise make. It is additive,
  since `Filter` is non-exhaustive.

## Staging

The whole API above exists from the start; unsupported parts return `NotImplemented`, naming the
part. The shape never changes, only what is implemented behind it.

**Starting point: `francesco/query`.** It has:

- `Query` with one variant, `Predicate { field, op, value }`; `Op::Eq` only.
- `Value`: a typed literal for every indexable type, with `From` conversions for the common Rust
  types.
- `execute(&impl PostingSource, &Query) -> RoaringTreemap`: the matching record IDs, ascending.
- `PostingSource`, a storage seam, with `IndexSource` over an `Index` at one read snapshot.
- Typed semantics as specified. Query terms use the same `IndexTerm::new` as the write path, and
  tests check that both agree for every type.

It is based on the older `main` and does not yet build on `feature/golem-db-api`. Missing for
users: `Api::query` and `count`, more than one predicate, operators other than `Eq`, records
instead of IDs, sort, paging, budget and receipt.

**Steps:**

| Step | Contents |
| --- | --- |
| Rebase | onto `feature/golem-db-api` (storage renames, `api` crate) |
| Tree | `Filter`, `QueryOp`, `CountOp`, admission validation, caps |
| Boolean | `And`, `Or`, `Not` inside `And`, `In` |
| Scans | `Range`, `Prefix`, `Exists` over a typed run |
| API | `query`, `count`, snapshot selection, record IDs → records, projection |
| Sort | multi-key, typed, absent last, record ID tie-break |
| Paging | `limit`, `offset`, then the cursor |
| Metering hooks | counts of reads, steps, set combinations, fetches; budget checkpoints |
| Tests | property tests against a naive reference evaluator |

A first cut (MVP): `Eq`, `In`, `Range`, `And`, materialization, `limit` / `offset`, `count`, and
`NotImplemented` for the rest. Standalone `Not` waits for P08, the cursor for D11.

## Decisions needed

1. **The tree replaces the ordered DNF** assumed by the API spec and D01 (which recommends "flat
   DNF only"), for the reasons in [Why a tree](#why-a-tree).
2. **Standalone `Not`:** wait for P08, or decide now.
3. **The cap values,** and the code ceilings ([Caps](#caps)).
4. **The cursor contract** (D11), before paging by cursor.

## To do when folding this in

- `golem-db-api.md`: replace *Specified, not yet implemented → Query* with the call, the tree, the
  execution rules, the caps and the errors; update *Further errors* (`LimitExceeded` now names the
  three caps).
- `decisions_for_architect.md` and `CHANGES.md` D01: record the switch to a tree.
- `decisions_for_architect.md` D09 item 9.13: replace the DNF caps (`#maxFilterGroups`,
  `#maxPredicatesPerGroup`) with `#maxQueryDepth`, `#maxQueryLimit`, `#maxSortKeys`.
- `golem-db-design.md` §4 `#params` table: add the three caps.
- `golem-db-metering.md` D6: charge set combinations per bitmap container.
- `api-priorities.md`: add the query MVP to the 2-week scope.
- Delete this file.
