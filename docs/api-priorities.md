# API priorities

Where to spend effort on the Golem DB API over the next weeks, and why. A working document for
discussion, not a spec.

Status: draft, 2026-10-09.

## The situation

- **One implementation counts: `feature/golem-db-api`** (`95a24ea`). `feat/record-ops` was an
  experiment to find a good shape for the API. The API spec, [golem-db-api.md](golem-db-api.md), records
  the shape it arrived at, so the spec is the target and `feature/golem-db-api` is the code that
  gets there.
- **Breaking changes are nearly free for about four weeks.** No consumer depends on the API yet,
  and every database is disposable. After that, as integration starts, each break costs more.
- **The lower layers already match the spec:** branch lifecycle, seal and commit, stale-branch rules,
  `get` semantics, the no-op rule, opening, genesis identity and reopen checks, `StoreFull`, the
  immutable-data stubs. The gap is in the public record-call surface and in the stored format.

So the question is not "what can we avoid breaking", but **which gaps become expensive when the
window closes**, and which work does not depend on the window at all.

## The rule

Use the window for what is free only now. Postpone what stays cheap later.

| Becomes expensive after the window | Stays cheap later |
| --- | --- |
| Changing the signature of calls that callers use everywhere | Adding a method, variant, trait impl or constructor |
| Changing what is stored in every record or in genesis | Adding behaviour behind an existing signature |
| Removing a public method callers may have adopted | Renaming with a deprecated alias |

## Priority 1: fix the shape of the record calls, and add `#meta`

**Why first.** `create`, `get`, `patch` and `delete` are what every caller writes. Their shape
spreads into every test, mock and adapter built from week five on. Today, changing it costs an
afternoon; the experiment has already shown how. `#meta` belongs here too: it is the one
stored-format change whose later migration grows with the data.

| Gap in `feature/golem-db-api` | Target | Reference |
| --- | --- | --- |
| Record calls return `Result<T>` | `Metered<T>`: result plus receipt, also on failure | API [Receipts](golem-db-api.md#receipts) |
| `RecordOp::create(key)` requires a key | `create()` with an optional `.key(k)`, decided by the key mode | API [Record keys](golem-db-api.md#record-keys) |
| No budget | explicit `Budget::Limited(n) \| Unlimited`, no default | API [Agreed changes](golem-db-api.md#agreed-changes-not-yet-implemented) |
| `insert` (create) and `set` (patch) keep the *value's* kind, which defaults to field | `insert` removed; `set` keeps the **stored** cell's kind and fails with the new `CellNotFound { name }` if the cell does not exist | API [Agreed changes](golem-db-api.md#agreed-changes-not-yet-implemented) |
| Builder steps return `Result<Self>` | **proposed:** steps never fail; the first invalid step is reported by the call, with a receipt, which is the shape the metering spec asks for | API [`RecordOp`](golem-db-api.md#recordop) |
| No `#meta` cell | `#meta` on every user record: four counts over its user cells, maintained by every write | API [`#meta`](golem-db-api.md#meta) |

The receipt can report cost 0 and the budget can be accepted and ignored until metering exists;
what matters is that the signatures carry them. `Details` and `#meta` count the same effects, so
building them together costs little more than building either.

**`#meta`.** Adding it later means rewriting every user record: a migration that scans the
whole database and grows with it. It is needed for the deletion bound (metering D5), completeness
proofs, sizing Arkiv's `extend`, and pricing a patch without reading the whole record.

**Keep while closing the gaps.** `feature/golem-db-api` has two things the spec does not cover:
`Genesis` as `#[non_exhaustive]` with `Genesis::new`, and the branch concurrency, lifecycle and
publication tests with the integration-test crate. Do not lose them in the rework. (Its
`CommitId` / `BranchId` newtypes and `RecordOp` accessors are now in the spec.)

**Proposed: never-failing builder steps** (2026-10-09). An invalid step is kept in the operation
and reported by the database call as `InvalidArgument`, naming the step. This is the shape the
metering spec asks for: D2 charges input errors found while building a request as admission. A host
translating untrusted input, such as Arkiv, then gets every input error the same way, an error
together with a receipt, and has one place to charge. With fail-fast steps, builder errors would
happen in the host's code before Golem DB is called, with no receipt.

## Priority 2: remaining format changes and small fixes

Each of these is small. The format changes still change the genesis identity, so bundling them
means one break instead of several, but each one alone is a minor migration later.

- Per-record caps `#maxRecordCells`, `#maxRecordIndexedCells` in `#params`. Without them there
  is no deletion bound (metering D5), and Arkiv's prepaid expiry depends on one. Records created
  before the caps exist can exceed them, so they are best added soon after `#meta`.
- `#keyMode` and `#keySeed` in `#params`, for generated keys. Could also be made additive: no
  `#keyMode` cell means caller-assigned.
- Global counters `#liveCells`, `#indexTerms` in `#alloc` (metering D3); only metering uses them.
- `#minRetention` in `#params`, once its value is decided (metering Open Question 3).
- `OpenConfig` → `Config` (naming decision N11).
- `#[non_exhaustive]` on `OpenMode`: adding it later is itself a breaking change.
- Leftover "engine" and "backend" wording in docs and messages.
- `ApiError::Internal` showing its cause.

## Can wait

Additive, so the window does not matter. Schedule by need, not by risk.

- `IntoCellValue`: changing the builder parameter from `CellValue` to `impl IntoCellValue` is
  backward compatible.
- `Genesis::DEV`, `StoreConfig` and the store file, the `internals` feature, a `mdbx` feature
  flag.
- `OutOfBudget`, `KeyModeMismatch` and other new error variants (the enum is non-exhaustive).
- Metering itself: costs, budget enforcement, ledger, admin API.
- Immutable-data storage, including the row-key index (design §11), query and count, proofs
  with `id_of` / `key_of`, history.

## When the window closes

Treat the API as stable once one of these happens, whichever is first:

- a consumer (Arkiv) builds against it outside this repository;
- a database must survive an upgrade.

From then on, prefer additive changes, deprecate before removing, and plan stored-format changes
as migrations.
