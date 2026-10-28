# Sorting: a pluggable strategy, fetch-and-sort first

## Context

`QueryOp.sort` exists, but [queries.rs:88](../crates/query/src/queries.rs#L88) rejects any sort with `NotImplemented { "query: sort" }`.

Sorting can be done in two ways, and which is cheaper depends on the data:
- **Fetch-and-sort:** read every matched record's sort cell, then order the values in memory. This is what the spec describes: design §12 and rule 8 of the query proposal.
- **Index walk:** visit the index entries for the sort key in value order.

So sorting becomes a strategy. `SortingStrategy` is an abstract interface, and the strategy is injected when `Queries` is created.

- **This change** builds the interface and its first implementation, `FetchAndSort`, for a single sort key.
- **Later:** multi-key sorting, then the index walk as the last phase, behind the same interface.

## Decisions

- **Absent values come first under ASC and last under DESC.** This follows the spec: they sort below every present value (§12, [golemdb-design.md:3120](golemdb-design.md#L3120)). Ties are broken by ascending record ID in both directions. Two places that say "absent values sort last" are brought in line by this change: the `SortKey` doc and rule 8 of the query proposal (step 6).
- **Sorting on `bytes` is not allowed.** `SortKey` becomes marker-typed and bounded by `Indexable`, like `ExistsType`, so `bytes` does not compile. The runtime path rejects it with `ValuesError::NotIndexable`.
- **Fields are sortable.** §12 says so at [golemdb-design.md:3276](golemdb-design.md#L3276), and its type test ignores the kind bit (`tag & 0x7F`). A field and an attribute of the matching type both sort. A cell of another type counts as absent.
- **Sorting is a strategy, injected into `Queries`.**
  - The contract: every strategy returns the same page for the same state and query. Strategies differ only in what they read, and so, once metering is enforced, in what they cost.
  - **Open.** Anyone can implement the trait. Its inputs therefore become public API: `PostingSource` (the index reader), `CellSource` (the cell reader), `SortPlan` and `Window`. The contract is documented on the trait. The crate's conformance tests check its own strategies only.
  - **Static dispatch.** `Queries<S, H, O: SortingStrategy = FetchAndSort>`. If a runtime choice is ever needed, for example from genesis, an enum over the strategies can implement the trait.
  - **One strategy per deployment.** Once budgets are enforced, the strategy changes a query's cost, so every node of a deployment must use the same one. `Database` hardcodes `FetchAndSort` for now, as it hardcodes the query caps.
- **An explicit override for where absent values go** (like SQL's `NULLS FIRST` / `NULLS LAST`) will be offered later, at the lowest priority. See Later phases.
- **More than one key** stays `NotImplemented` for now, as `"query: multi-key sort"`.
- **No metering yet.** Budgets are not enforced anywhere in queries.

## 1. Typed `SortKey`

File: [crates/query/src/query_op/sort.rs](../crates/query/src/query_op/sort.rs). Follow the pattern of [exists_type.rs](../crates/query/src/query_op/exists_type.rs).

- Fields become private: `field: CellName`, `ty: CellType`, `direction: Direction`.
- Constructors:
  - `SortKey::asc<T: Indexable>(field: impl AsRef<[u8]>, _: T)` and `SortKey::desc(..)` take the name raw. It is validated when the query runs, as today.
  - `SortKey::try_new(field, ty: CellType, direction) -> Result<Self, ValuesError>` is the runtime path. It reuses `values::indexable(ty)`, so `bytes` gives `ValuesError::NotIndexable`.
- Accessors: `field()`, `cell_type()`, `direction()`.
- Update the doc comment to say:
  - the key is typed;
  - a field or attribute of that type sorts;
  - a cell of another type is absent;
  - absent values come first under ASC and last under DESC;
  - ties go to ascending record ID.
- Add a `compile_fail` doctest for `SortKey::asc("x", Bytes)`, as `ExistsType` has.
- Unit tests, inline:
  - the marker sets the type;
  - the runtime path builds what the marker builds;
  - the runtime path rejects `bytes`.
- Add `SortKey::try_new` to the list of runtime constructors in the `on_unimplemented` notes of [marker.rs](../crates/cells/src/marker.rs) (the `CellMarker` and `Indexable` notes).
- Update the callers, which use `CellType`:
  - the tests in [op.rs](../crates/query/src/query_op/op.rs) (`U64`, `Date32`);
  - `by_size()` in [tests/query.rs](../crates/query/tests/query.rs) (`U32`);
  - [integration-tests/tests/api/receipts.rs:411](../crates/integration-tests/tests/api/receipts.rs#L411) (`I32`).

## 2. Reading cells without storage in tests

New file: `crates/query/src/cell_source.rs`, mirroring [posting_source.rs](../crates/query/src/posting_source.rs).

- A trait `CellSource` with one method: `fn cell(&self, record_id: u64, name: &CellName) -> golemdb_cells::Result<Option<CellValue>>`.
- It is implemented for `golemdb_cells::CellReader` as `CellReader::get(&CellKey::new(id, name.clone()))`, which is one point read of `Cell[id ‖ name]`.
- Add `impl From<CellError> for QueryError`, mapping to `Internal`, in [error.rs](../crates/query/src/error.rs), next to the existing `IndexError` one.

## 3. The `sorting` module: the interface and `FetchAndSort`

New module: `crates/query/src/sorting/`. It is named `sorting`, after the trait, so it isn't confused with `query_op::sort`. Unit tests go inline.

### `sorting/mod.rs`: what every strategy shares

- **`SortPlan`** is the sort after admission: one key for now, holding `field`, `ty` and `direction`. When multi-key sorting arrives, `SortPlan` gets more keys and the trait doesn't change.
  - It is public, with read-only accessors, because strategies outside the crate receive it. Only admission builds it, so every `SortPlan` a strategy sees is valid.
  - It is built by `pub(crate) SortPlan::admit(&[SortKey]) -> Result<Option<SortPlan>>`: `None` for no sort.
  - It checks each name with `IndexTerm::check_cell_name`. A bad name fails with `QueryError::InvalidArgument`, built by `invalid_argument` in [plan.rs](../crates/query/src/plan.rs), which becomes `pub(crate)`.
- **`SortPlan::compare(a, b)`** is the one ordering every strategy uses. It is public, so outside strategies use the same order. It is defined over `(record_id, Option<&[u8]>)`, where the bytes are the order-encoded payload from `CellValue::value()`.
  1. Compare the payloads. `None < Some`, so absent values sort below every present value.
  2. Reverse that comparison for DESC.
  3. Then compare record IDs, ascending. The tie-break is never reversed.
- **`Window { offset: u64, page_size: usize }`** is the slice of the order the page takes. It is public, with public fields.
- **The trait:**
  ```rust
  /// How a match set is put in sort order.
  pub trait SortingStrategy {
      /// The record IDs of `window`, in the order `sort` defines over `matches`.
      /// Every strategy returns the same IDs for the same state and inputs.
      fn sorted_page(
          &self,
          index: &impl PostingSource,
          cells: &impl CellSource,
          matches: &Bitmap,
          sort: &SortPlan,
          window: Window,
      ) -> Result<Vec<u64>>;
  }
  ```
  - `index` is passed although `FetchAndSort` doesn't use it, so the index walk can be added without changing the trait.
  - `PostingSource` in [posting_source.rs](../crates/query/src/posting_source.rs) changes from `pub(crate)` to `pub`, and so does the new `CellSource`. Its implementation `IndexSource` stays crate-private. Their docs describe what an implementation must return, since outside code can now implement them too, for example as fakes in its own tests.
- **Conformance tests,** generic over `impl SortingStrategy`, so the index walk can later be run through the same tests by adding one line:
  - ASC puts absent first and DESC puts it last;
  - ties go to ascending ID in both directions;
  - another type under the same name is absent;
  - a field sorts like an attribute of the same type;
  - offset and limit windows, including an offset past the end and a page size of 0;
  - a proptest checks the strategy against a naive reference (a full sort, then skip and take) over random values, absent cells, other-type cells, fields, directions and windows.
- **Test fakes,** shared through a `#[cfg(test)]` module, `crates/query/src/fakes.rs`:
  - `FakeSource` and its `bitmap` helper move there from the tests in [eval.rs](../crates/query/src/eval.rs), which then import them;
  - a new `FakeCells` (a `BTreeMap<(u64, CellName), CellValue>`);
  - a builder that writes both from one list of records, so the index and the cells always agree, including the rule that only attribute cells are indexed.

### `sorting/fetch_and_sort.rs`: `FetchAndSort`

- `pub struct FetchAndSort;` derives `Debug, Clone, Copy, Default`.
- **Fetch:** for every ID in `matches.treemap().iter()`, in ascending order, read `cells.cell(id, &field)`. Keep the value only if `value.cell_type() == ty`; any kind is accepted. Otherwise it is absent. Every matched value is fetched, as rule 8 requires.
- **Select:** let K = `offset + page_size`, saturating, as a `usize`.
  - Keep a buffer of at most 2K `(id, Option<CellValue>)` entries (capacity `min(2K, N)`).
  - When it is full, run `select_nth_unstable_by(K - 1, SortPlan::compare)` and truncate it to K.
  - At the end, sort what is left, skip `offset` and take `page_size`.
  - This takes O(N) time on average and O(K) memory. When K ≥ N it is a plain full sort.
- **Its own unit tests:**
  - the buffer gets truncated, with N > 2K;
  - it never reads the index (the test uses a `PostingSource` that fails on every call).

## 4. Inject the strategy into `Queries`

File: [crates/query/src/queries.rs](../crates/query/src/queries.rs).

- `pub struct Queries<S, H, O = FetchAndSort>` gets a `sorting: O` field. `Queries::new(store, hasher, max_query_depth, max_query_limit, sorting: O)`. The `impl` blocks gain `O: SortingStrategy`.
- `golemdb_query` exports `SortingStrategy`, `FetchAndSort`, `SortPlan`, `Window`, `PostingSource` and `CellSource`. `golemdb_api` doesn't re-export them until a deployment can choose its strategy.
- Callers:
  - [database.rs:256](../crates/api/src/database.rs#L256) passes `FetchAndSort`, hardcoded like the caps, with a comment saying the choice belongs to the deployment;
  - the two `Queries::new` calls in [tests/query.rs](../crates/query/tests/query.rs) (around lines 484 and 500) pass `FetchAndSort`.
- In `query_report`, the order of checks stays as QR5 requires:
  1. `op.sort.len() > 1` → `NotImplemented { operation: "query: multi-key sort" }`. This replaces the current "any sort" check and still runs before the limit check.
  2. `page_size(..)`, unchanged.
  3. `Plan::admit(&op.filter, ..)`, unchanged.
  4. `SortPlan::admit(&op.sort)?`. Sort keys are admitted after the filter, in the order the query is written.
  5. Build the match set, as today.
  6. Choose the IDs:
     - no sort: `matches.iter_skipping(offset).take(page_size)`, as today;
     - with a sort: `self.sorting.sorted_page(&index_source, &cells, &matches, &plan, window)`, using the same snapshot's `IndexSource` and `CellReader`.
  7. Materialize with `read_committed_by_id`, unchanged. `total_matched` is unchanged.
- Update the docs:
  - the module docs and the `query` doc in `queries.rs` (they currently say "Sort returns NotImplemented for now"), and the `new` doc for the new parameter;
  - the crate doc in [lib.rs](../crates/query/src/lib.rs), if it mentions record ID order only.

## 5. Integration tests

New file: `crates/query/tests/sort.rs`, using `support::{TestDb, record, key, eq, native_cmp, small_value, ...}`. These go through `Database`, so they test `FetchAndSort` end to end. Cases:
- a `u32` key, ASC and DESC, with:
  - records that lack the cell (first under ASC, last under DESC);
  - records with another type under the name, which count as absent;
  - tied values, which come out in creation order in both directions;
- a field cell (`RecordOp::field`) sorts among attributes of the same type;
- type order is respected: negative `i32`, `str` byte order, `f64` with ±inf;
- `offset` and `limit` page through the whole sorted sequence, and `with_total` is unaffected;
- a bad sort name gives `InvalidArgument`; a bad filter is reported before a bad sort name;
- a proptest: random records of small values, with the sorted query's key order equal to a model sorted with `native_cmp`, absent first under ASC, ID tie-break.

Changes to existing tests:
- `sort_is_not_implemented` in [tests/query.rs](../crates/query/tests/query.rs) becomes a two-key case expecting `"query: multi-key sort"`.
- `the_limit_is_checked_after_unbuilt_parts_and_before_the_filter` uses two keys.
- In `receipts.rs`, the unsupported-part case uses two keys.

## 6. Spec text

- [docs/query-proposal.md](query-proposal.md):
  - in rule 8, change "absent values sort last" to "absent values sort below every present value: first under ASC, last under DESC (design §12)";
  - in the Staging table, change the Sort row to "single key done (fetch-and-sort); multi-key and index walk open";
  - in the Gaps list, change the sort entry to match.

## Verification

- `cargo fmt -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p golemdb-query`, which covers the unit and conformance tests, doctests (including `compile_fail`) and the integration tests.
- `cargo test -p golemdb-api -p golemdb-integration-tests`, for the receipts and API error tests.

## Later phases, in order (not in this change)

1. **Multiple keys, in `FetchAndSort`.** Done (`ca0e1a9`).
   - `SortPlan` holds one `SortLevel` per key, and each level's `compare` orders the records the levels before it tie.
   - A deeper level's value is fetched only for records still tied on the earlier keys, in groups that overlap the page window. Design §12 and rule 8 now say so.
   - The `#maxSortKeys` cap is `max_sort_keys` on `Queries`; `Database` sets it to 4.
   - The conformance tests have multi-key cases.
2. **Index walk, last: a second strategy, `IndexWalk`.** Planned in detail under [Phase 2: `IndexWalk`](#phase-2-indexwalk) below.
- **Metering hooks,** with the metering work. Each strategy charges what it reads:
  - `FetchAndSort`: the fetches as cell reads, plus the modeled comparisons `S × N × ceil_log2(N)`;
  - `IndexWalk`: the index entries stepped through, the containers loaded, the set operations, and the fetches.
- **Lowest priority: an explicit override for where absent values go.**
  - This is like SQL's `NULLS FIRST` / `NULLS LAST`. GolemDB has no null value (design §3, [golemdb-design.md:879](golemdb-design.md#L879)), so the override is about absent values.
  - It would be an optional setting on each `SortKey`, for example `.absent(Absent::First | Absent::Last)`. Without it, the order is the spec's default: first under ASC, last under DESC.
  - In `SortPlan::compare`, absent values are placed first or last before the direction is applied, so the direction only reverses present values. The record-ID tie-break is unchanged.
  - It is a spec addition: §12 says the direction applies to absent values as to any other. The setting is part of the sort, so a cursor's fingerprint (D11) covers it like the rest of the sort.

## Phase 2: `IndexWalk`

### What it is for

A second `SortingStrategy` that finds the first sort key's values in the index instead of fetching them. The `Index` table is already sorted by value: one `(name, type)` run holds every attribute value of the key, in order, each with the records that hold it. Walking the run gives the first level's order and its tie groups at the price of term reads, not cell reads.

It returns exactly the pages `FetchAndSort` returns: the `SortingStrategy` contract. Only what it reads differs.

Notation: N is the number of matches, K is offset + page size, V is the number of terms in the key's run.

| Query | `FetchAndSort` | `IndexWalk` |
|---|---|---|
| Sort by `status` (5 values), N = 1M, limit 20 | 1M cell reads | at most 5 terms; no cell read when every match has the attribute |
| Broad filter, N = 1M, sort by `price` with 50k values | 1M cell reads | at most 50k terms |
| `Range ts [last day]` (N = 10k), sort by `ts` DESC, limit 20 | 10k cell reads | about 20 terms |
| `owner = X` (N = 1k), sort by a unique `ts` over 10M records | 1k cell reads | at most about 2k reads, thanks to the switch rule |

What it does not change: the filter's own cost (a wide `Range` or `Exists` on the key is still evaluated in full), and the cost of a deep `offset`, which only the cursor (D11) will fix.

### Decisions needed before code

1. **How a deployment picks its strategy.** Every node must sort the same way, since the strategy decides what a sorted query reads.
   - (a) `Database` keeps `FetchAndSort` hardcoded; `IndexWalk` is reachable through `Queries::new` only, for tests and the benchmark, until the spec and metering adopt it. **Recommended for this phase.**
   - (b) `Database` switches to `IndexWalk` now.
   - (c) A genesis parameter.
2. **The switch rule** (below): walk while the terms stepped through are fewer than the matches still unplaced, then fetch the rest. It bounds the cost at about 2N, with no statistics. **Recommended.**
3. **When the filter guarantees the key.** Early stop at K is only correct when every match holds the first key as an attribute of its type: then no match is absent and none is a field. Detected from the written filter alone, at admission. **Recommended rule:** the root of the filter, or a direct child of a root `And`, is one of:
   - an `Eq` or `In` on the key's name, of the key's type;
   - a `Range` on the key's name, of the key's type;
   - a `Prefix` on the key's name, when the key is `str`;
   - an `Exists` on the key's name, typed with the key's type (an untyped `Exists` does not count).

   `Or`, `Not` and deeper nesting are not looked into.
4. **The spec change and its timing.** §12 rejects index-ordered emission, rule 8 and D6.2 describe fetch-and-sort only. **Recommended:** write the spec text last, after the benchmark, with its numbers.

### How it orders

The first level comes from the index; deeper levels are ordered exactly as `FetchAndSort` orders them: by fetching, only for tied groups the page reaches.

**General case** (the filter does not guarantee the key):
1. `unplaced` starts as the match set. Walk the key's run in ascending key order. For each term: `hits = postings ∩ unplaced`; the hits take the term's value; `unplaced -= hits`.
2. Stop when `unplaced` is empty, the run ends, or the terms stepped through reach `|unplaced|` (the switch rule).
3. Fetch the cells of the records still unplaced, in ascending record ID order. Each is a field of the key's type (its value), or absent. Fields can hold any value, so this step cannot be skipped unless `unplaced` is empty.
4. Order every match by its value, as `FetchAndSort` does: the same top-K buffer, absent placement and record ID tie-break, then the deeper levels.

Direction plays no part in the walk here: the walk cannot stop at K, since an unplaced field may belong anywhere.

**Guaranteed case** (every match holds the key as an attribute):
1. Walk the run in the sort direction: ascending for ASC, descending for DESC. Each term's hits are one whole tie group, in order.
2. Skip whole groups before the offset by counting them; stop after the group that reaches K.
3. The switch rule still applies. If it fires first, fetch the cells of the unplaced matches, keep their least K in order, and append them: their values all lie beyond the terms walked.
4. Deeper levels for the groups the page reaches, as above.

At the first level this reads no cell at all, unless the switch rule fires.

**Cost bound.** At the switch, the terms stepped through are at most the matches still unplaced, and each of those costs at most one fetch. So the walk and the fetches together stay within about 2N reads, against N for `FetchAndSort`, and far below it in the cases the table shows.

### Steps

1. **Reverse range scan, in `golemdb-storage`.** A `scan_rev(tx, table, lower, upper)` beside `scan`, built on `ReadCursor::prev`, which both backends implement. It mirrors `scan`'s bound rules: reversed bounds rejected, an excluded endpoint, an empty range, errors yielded once. Tests mirror `scan`'s, on the memory store and MDBX.
2. **Index reads, in `golemdb-index`.**
   - `Index::terms_rev(tx, range)`, the descending twin of `Index::terms`.
   - `IndexTerm::value()`: the order-encoded payload after `name ‖ 0x00 ‖ tag`, the same bytes `CellValue::value` gives. Tests: a term built from a cell gives that cell's payload back, for every indexable type.
3. **`PostingSource::walk(range, direction)`, in `golemdb-query`.** Like `scan`, but yields each term with its postings, in ascending or descending key order. `IndexSource` implements it with `terms` / `terms_rev`; `FakeSource` with its `BTreeMap`, forward or reversed. `scan` stays as it is for the evaluator.
4. **The guarantee, at admission.** `SortPlan::admit` takes the filter too and records whether it guarantees the first key (decision 3). A public accessor tells strategies. Unit tests for each leaf that counts and each that does not: another field, another type, an untyped `Exists`, an `Or`, a `Not`, a leaf nested two levels down.
5. **Shared ordering code.** Move the top-K buffer, the tie groups and the deeper-level fetches out of `fetch_and_sort.rs` into a `sorting` submodule both strategies use. An entry's value becomes its payload bytes, so a value read from a term and one read from a cell are the same thing. No behavior change: `FetchAndSort`'s tests stay as they are.
6. **`sorting/index_walk.rs`: `IndexWalk`.** Both cases and the switch rule. Unit tests on what it reads, through a logging `PostingSource` and the logging `CellSource`:
   - every match found in the index: no cell read;
   - fields and absent cells: only the unplaced matches are fetched;
   - the switch rule fires when the terms stepped reach the unplaced count, and not before;
   - guaranteed, DESC, small page: only the terms up to K are read;
   - deeper levels read as `FetchAndSort` reads them.
7. **Conformance.** `conformance!(index_walk, IndexWalk)` runs every shared case. A new property test checks `IndexWalk` returns the same page as `FetchAndSort` over random cells, matches, keys and windows, with the guarantee set exactly when every match holds the key as an attribute.
8. **End to end.** The `tests/sort.rs` property test also runs through `Queries` built with `IndexWalk`, including filters that guarantee the key.
9. **Benchmark.** `crates/integration-tests/benches/sort.rs`, with criterion, both strategies on the scenarios of the table above, scaled to what the memory store holds comfortably (about 100k records).
10. **Spec text,** after the benchmark: §12's "The Alternative" becomes a second strategy with the switch rule and the guarantee; rule 8 describes both; D6.2 lists what `IndexWalk` is charged (terms stepped, terms read, containers, set combinations, fetches). Plus decision 1's outcome.

Not in this phase: metering hooks for either strategy; using the index for deeper levels too (ranking each tied record by its term's position); narrowing the walk to the filter's own range on the key.
