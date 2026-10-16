# Query API proposal

**Temporary working document.** It records the outcome of the query discussion of 2026-10-10 to
2026-10-12, so that the *Query* part of [golemdb-api.md](golemdb-api.md), which still assumes an
ordered DNF, can be replaced by what follows. Delete this file once the API spec and D01 carry it.

**Starting point:** `main` at `794cb44` (#50, "Implement Basic Type - API traits - Equal and In
query operators"), crate `golemdb-query`; see [Staging](#staging).

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
plan. Principles 3 to 5 apply the metering spec to reads ([metering](golemdb-metering.md) R1, R3,
R5, R7 and D6: reads are counted, not modeled, and abort when they cross their budget).

### Requirements

| # | Requirement | From |
| --- | --- | --- |
| QR1 | `query` and `count` each take an **explicit budget**, `Limited(n)` or `Unlimited`, with no default, received the same way as a `RecordOp`'s. `Unlimited` is for host-authorized estimation only. | P4; metering R7, D8 |
| QR2 | `query` and `count` return the result **together with a receipt**, on success and on failure, like the record calls. Today every API call returns `Result<T>` with no receipt; queries move to `Metered<T>` when the API does, for every call at once. | P4, P5; API *Receipts* |
| QR3 | The **execution order is a pure function of the query**: the order the execution rules derive from the written filter, sort and page. No statistics, caches or warm state change it or the cost. | P1, P2; metering R5, D1 |
| QR4 | **Same query, same committed state, same pricing snapshot → same result, same cost, same stopping point** on every node and implementation. | P3; metering R1 |
| QR5 | **Order of checks:** first the top-level parts not implemented yet, at no cost; then admission, validating the whole query before any state read and charged as admission; then one storage snapshot is opened and the requested commit is checked against its head; then the state reads, all from that snapshot. | metering D2 *Order of checks* |
| QR6 | **Every step that reads state or combines sets has a known, bounded charge, checked before the step runs:** a term read, a term stepped over, a set combination, a sort-value fetch, a materialized cell read, and the modeled sort comparisons before a sort. A charge equal to the remaining budget fits. | P5; metering D2, D6 |
| QR7 | **The first charge that does not fit stops execution** without performing that step: `OutOfBudget { required: None }`, the spent cost in the receipt, and **no partial results**. | P4, P5; metering D8 |
| QR8 | **Caps bound what the budget cannot,** independently of the budget, which may be `Unlimited` ([Caps](#caps)). | P4 |
| QR9 | **Pricing comes from the head at admission,** also for a pinned query of an older commit; each page is a new call with its own pricing. | metering D9 |

The execution rules below are also the reference execution that metering finding F1 asks for:
they fix the read operations, their order and their charge checkpoints.

## The call

```rust
fn query(&self, commit: Option<CommitId>, op: QueryOp) -> Result<QueryPage>;
fn count(&self, commit: Option<CommitId>, op: CountOp) -> Result<u64>;

QueryOp::filter(f, budget)                               // the filter tree and the budget; both required
    .sort([SortKey::asc("price", CellType::Uint(W8))])   // optional
    .only(["owner", "status"])                           // projection, as for get
    .limit(50)                                           // records per page; optional
    .max_bytes(n)                                        // bytes per page; optional
    .after(cursor) | .offset(n)                          // at most one
    .with_total()                                        // ask for total_matched

CountOp::filter(f, budget)                               // both required (QR1)

pub enum Budget { Limited(u64), Unlimited }

pub struct QueryPage {
    pub records: Vec<Record>,
    pub total_matched: Option<u64>,
    pub evaluated_at: CommitId,
    pub next: Option<Cursor>,
}
```

It follows the rest of the API:

- **The `RecordOp` pattern:** a typed builder whose steps never fail; the first invalid step is
  reported by the call.
- **The budget is a constructor argument, with no default.** A query without a budget cannot be
  written: the compiler rejects it. A caller who wants no limit passes `Budget::Unlimited`
  explicitly. A default of `Unlimited` would make a forgotten budget silent: an unbounded query on
  a node's RPC path, or unbounded work in a block, with no error to notice. A user who wants a
  default adds it in a thin layer of their own.
- **The same `Budget` as the record calls, received the same way:** `RecordOp`, `QueryOp` and
  `CountOp` all take it in their constructor. The budget is part of the request; a future
  `ImmutableDataOp` would follow the same pattern.
- **The same receipt as the record calls,** once the API returns `Metered<T>` (QR2): `cost`,
  `priced_at` (the head at admission, QR9) and `details`, which are zero for reads.
- **The same builder entry point for both:** `QueryOp::filter(f, budget)` and
  `CountOp::filter(f, budget)`. Filter and budget are required, so they are given at construction
  and are never steps. The code's `QueryOp::from_filter` is to be renamed.
- **A committed state as target:** `commit` is `None` for the head (live paging) or `Some(c)` for
  a named commit (pinned). Queries read committed state only, so no branch can be named. For now
  only the head can be read; a commit other than the head returns `CommitUnavailable`, and
  queries of older commits come later.
- **The commit is checked after admission** (QR5), against the head of the storage snapshot the
  query then reads. Admission is pure, so errors in the query itself do not depend on state.
  Checking a commit is a state read, the head, so admission first rejects bad queries without
  touching storage. And one snapshot serves both the check and the reads, so no commit can land
  between them.

`count` takes a filter and a budget, and runs rules 1 to 7 below: it evaluates the filter and
returns the size of the match set, with no sorting, paging or materialization.

## AST query format

A query's filter is an **abstract syntax tree**: `And`, `Or` and `Not` nodes over typed leaves.

```rust
#[non_exhaustive]
pub enum Filter {
    Eq     { field: CellName, value: EqValue },          // one indexable value
    In     { field: CellName, values: InValues },        // one indexable type
    Range  { field: CellName, bounds: Bounds },          // one indexable type
    Prefix { field: CellName, prefix: String },          // str only
    Exists { field: CellName, ty: CellType },
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
}
```

- **Leaves are typed,** as in `golemdb-query`: a value carries its type, and a cell of another
  type under the same name is absent. `bytes` cannot be queried, being field-only.
- **Operands are valid by construction.** `EqValue`, `InValues` and `Bounds` are built from a
  cell-type marker (`EqValue::of(marker::Str, "blue")`, `Bounds::new(marker::I64, ..)`), so mixed
  types and `bytes` do not compile. Their runtime constructors (`try_from` / `Bounds::try_new`)
  check `CellValue`s and fail with `ValuesError` before a query exists. A `Range` keeps its type
  even with two unbounded sides, which match every cell of that type.
- **`Prefix` is `str` only.** It matches the strings that start with the prefix, byte by byte in
  UTF-8. A prefix over `bytesN` is left out for now; it can be added as a separate leaf if a
  use case appears.
- **`Ne`, `!=` and "not in" are `Not(...)`.** Absent and other-typed cells match a `Not`
  (MongoDB-style, D01 1.5).
- **`Exists` needs no extra index:** it is a scan over all terms of one `(field, type)` run.

### Range bounds

Each side of a `Range` is an explicit `std::ops::Bound`: `Included(v)`, `Excluded(v)` or
`Unbounded`. There are no defaults; the caller states both sides.

| Bounds | Result |
| --- | --- |
| `Included(a)` and `Included(a)` | the same as `Eq a` |
| `Included(a)` and `Excluded(a)`, or the lower above the upper | **empty**: no error, nothing read |
| `Unbounded` on both sides | `InvalidQuery`: no condition at all |
| bounds of two types | `InvalidQuery`: no order between them |

Bounds that admit no value are a valid description of the empty set, not a malformed query, as in
SQL (`BETWEEN 5 AND 3`) or MongoDB (`{$gt: 5, $lt: 3}`). Business logic that computes the two
bounds independently can produce equal or inverted bounds; matching nothing spares every caller a
guard for that case.

**Slicing convention: `[lower, upper)`.** A caller who splits a value space into slices uses
`Included` for the lower and `Excluded` for the upper bound. Consecutive slices `[a, b)`, `[b, c)`
then neither overlap nor leave a gap, so results over slices are consistent. Explicit bounds keep
the other forms available, such as an inclusive upper bound at a type's largest value.

**To consider:** a builder helper taking Rust's range syntax (`RangeBounds`), so `10..20` is a
half-open slice and `10..=20` inclusive, if this makes Rust users feel at home when adopting
GolemDB. Rust ranges cannot express an excluded lower bound, so the explicit form stays.

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
   and per byte inspected (metering D2). It checks names and NaN; that `And` and `Or` are not
   empty; and the [caps](#caps). An operand's type is fixed when its `EqValue`, `InValues` or
   `Bounds` is built, before any query exists. Bounds that admit no value (inverted, or equal
   with a side excluded) are valid and match nothing.
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
7. **Leaves.** `Eq`: one term read. `In`: values given in strictly ascending order, in the
   type's order as for `Range`, which is also term key order; one term read each. Admission checks
   the order in one pass, so its cost is linear in the number of values. `Range`, `Prefix`, `Exists`: step through the run's terms in ascending key order from the
   lower bound, charging every term stepped.
8. **Order.** With `sort`: per level, fetch each matched record's sort value in ascending record ID
   order and charge `⌈N log₂ N⌉ × S` comparisons (D14 14.1); ties fall to the next key, then to
   record ID ascending; absent values sort last. Without `sort`: record ID ascending, which is
   creation order.
9. **Page:** cursor or offset, then the page ends at `limit` records or at `max_bytes`, whichever
   comes first. Records are taken in query order; a record that would push the page past
   `max_bytes` ends the page and starts the next one, but every page holds at least one record.
   Bytes are counted on metering's byte basis (metering D4), so every node cuts the page at the
   same record. Without `limit` or `max_bytes`, the caps `#maxQueryLimit` and `#maxQueryBytes` are
   the defaults. The next page's `offset` is the previous one plus the records returned.
10. **Materialize** the projection for the page's records only, in page order, as point reads. A
    full record's size is known from its `#meta` before its cells are read, so a record that does
    not fit is not read; with a projection, its size is known only after the reads, which are
    charged, and the record is left out.
11. **`total_matched`** is the size of the match set, free.

Every charge is checked before its step (QR6), so `OutOfBudget` stops at the same point on every
node (QR4, QR7).

### Caps

Four genesis parameters in `#params`, each `u32`, fixed per deployment and readable with `get`:

| Cap | Bounds | Why the budget does not |
| --- | --- | --- |
| `#maxQueryDepth` | nesting depth of the filter tree | a deeper tree overflows the evaluator's stack, which aborts the process before or regardless of any charge |
| `#maxQueryLimit` | records per page | the size of one response and the memory to materialize it, also under `Unlimited` |
| `#maxQueryBytes` | bytes per page, on metering's byte basis | the same, for pages of large records; a host derives its request's `max_bytes` from its transport's response cap |
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
| `InvalidQuery` | the target is a branch; the tree is malformed (empty `And` / `Or`); a cursor does not match its query. Mixed or `bytes` operands cannot reach a query: they do not compile, or fail as `ValuesError` (`InvalidArgument`) when the operand is built | 0 for a branch target, else admission work done |
| `LimitExceeded` | a cap is exceeded | admission work done |
| `NotImplemented { operation }` | a supported-later part is used, such as `"query: sort"` | 0 for a top-level part (sort, limit, offset, cursor), checked before admission; admission work done for an unbuilt filter node, found while admitting the tree |
| `CommitUnavailable` | a commit other than the head is named; older commits come later | admission work done |
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

**State on `main` (`794cb44`, #50).** `golemdb-query` and the `api` crate provide:

- `Api::query` and `Api::count` on committed state, at the head; records are returned as `get`
  returns them, in record ID order, with projection and `total_matched`.
- `Filter` with `Eq`, `In` and `Range`; `QueryOp` and `CountOp` with every field of the call
  above except the cursor.
- Admission before any state read, building a plan: one term read for `Eq`, one per distinct term
  in key order for `In`, a lazy scan of one typed run for `Range`, nothing for a leaf that admits
  no value.
- `PostingSource`, a storage seam, with `IndexSource` over an `Index` at one read snapshot.
- Typed semantics as specified. Query terms use the same `IndexTerm` encoding as the write path.
- Property tests per leaf against a model.

Gaps against this proposal:

- Sort, `limit` and `offset` return `NotImplemented`; `And`, `Or`, `Not`, `Prefix`, `Exists` and
  the cursor do not exist yet.
- The budget is an optional step, neither required nor enforced; it becomes a constructor
  argument. No receipt, no caps.
- `QueryOp::from_filter` is to be renamed to `QueryOp::filter`.
- `In` accepts values in any order and deduplicates them; it is to require strictly ascending
  values.
- `max_bytes` and `#maxQueryBytes` do not exist yet.

**Steps:**

| Step | Contents | Status |
| --- | --- | --- |
| Tree | `Filter`, `QueryOp`, `CountOp`, admission validation, caps | done except caps and the tree nodes |
| Boolean | `And`, `Or`, `Not` inside `And`, `In` | `In` done |
| Scans | `Range`, `Prefix`, `Exists` over a typed run | `Range` done |
| API | `query`, `count`, snapshot selection, record IDs → records, projection | done for the head |
| Sort | multi-key, typed, absent last, record ID tie-break | open |
| Paging | `limit`, `offset`, then the cursor | open |
| Metering hooks | counts of reads, steps, set combinations, fetches; budget checkpoints | open |
| Tests | property tests against a naive reference evaluator | per leaf; tree-level open |

A first cut (MVP): `Eq`, `In`, `Range`, `And`, materialization, `limit` / `offset`, `count`, and
`NotImplemented` for the rest. Still missing from it: `And`, `limit` and `offset`. Standalone `Not` waits for P08, the cursor for D11.

## Decisions needed

1. **The tree replaces the ordered DNF** assumed by the API spec and D01 (which recommends "flat
   DNF only"), for the reasons in [Why a tree](#why-a-tree).
2. **Standalone `Not`:** wait for P08, or decide now.
3. **The cap values,** and the code ceilings ([Caps](#caps)).
4. **The cursor contract** (D11), before paging by cursor.

## To do when folding this in

- `golemdb-api.md`: replace *Specified, not yet implemented → Query* with the call, the tree, the
  execution rules, the caps and the errors; update *Further errors* (`LimitExceeded` now names the
  four caps).
- `decisions_for_architect.md` and `CHANGES.md` D01: record the switch to a tree.
- `decisions_for_architect.md` D09 item 9.13: replace the DNF caps (`#maxFilterGroups`,
  `#maxPredicatesPerGroup`) with `#maxQueryDepth`, `#maxQueryLimit`, `#maxQueryBytes`, `#maxSortKeys`.
- `golemdb-design.md` §4 `#params` table: add the four caps.
- `golemdb-metering.md` D6: charge set combinations per bitmap container.
- `api-priorities.md`: add the query MVP to the 2-week scope.
- Delete this file.
