# golemdb

A Cargo workspace for GolemDB. Crates live under `crates/`. Production crates
are registered as workspace members and `[workspace.dependencies]` path entries
in the root [Cargo.toml](Cargo.toml), so consumers use `{ workspace = true }`.
The unpublished integration-test package is a workspace member only.

## Layout

- `docs/` - project-wide design and API documentation
- `crates/cells` (package `golemdb-cells`) — the wire format for "Cells",
  GolemDB's typed value encoding
- `crates/storage` (package `golemdb-storage`) — transactional ordered key/value
  traits, bidirectional cursors, range scans and an in-memory implementation
- `crates/storage-mdbx` (package `golemdb-storage-mdbx`) — MDBX implementation of
  the storage traits; owns the native backend dependencies
- `crates/index` (package `golemdb-index`) — transactional posting updates across
  bitmap/index tries, ordered terms and scans, canonical chunks, and query bitmaps
- `crates/merkle` (package `golemdb-merkle`) — branch-only persistent Merkle trie,
  canonical compact branches, shared hashing and YAML hash configuration
- `crates/branch` (package `golemdb-branch`) — head-validated branch IDs and guarded cell
  read/write views, encoded-key prefix scans, atomic writes, checkpoints and undo;
  seal buffers cell/index updates and roots; commit atomically persists them and
  advances head and collects obsolete trie/bitmap nodes, with history deferred
- `crates/record` (package `golemdb-record`) — caller-keyed record CRUD, allocation,
  identity and bindings; branch work-in-progress and committed-head reads
- `crates/integration-tests` (package `golemdb-integration-tests`) — backend and
  cross-layer integration tests, plus the index backend benchmarks

## Documentation

- [Technical design](docs/golem-db-design.md): storage architecture, data model,
  commitments, history, and query design.
- [API](docs/golem-db-api.md): the caller-facing interface and operation semantics.

## Adding a crate

1. `cargo new --lib crates/<name>`
2. Add it to `members` and `[workspace.dependencies]` in the root
   `Cargo.toml`.
3. In the new crate's `Cargo.toml`, set `version.workspace = true`,
   `edition.workspace = true`, `rust-version.workspace = true`,
   `license.workspace = true`.

## Build

```sh
cargo build --workspace
cargo nextest run --workspace
cargo bench --workspace --no-run  # compile benches without running them
```

## Test layout

Tests of a crate's own behavior live under its `src/`, including tests that use
`MemoryDatabase` or a fake transaction as a fixture. Calling public APIs does not
by itself make a test an integration test. Hash/codec vectors, trie algorithms,
cell batch semantics, bitmap/term encoding, branch state and lock behavior, and
local error handling belong here.

`crates/integration-tests/tests/` contains the `branch`, `index`, `record`, and `storage`
targets. These cover MDBX persistence, reopening, limits, and transactions, plus
scenarios that verify cells, indexes, trie roots, and head publication together.
Some end-to-end scenarios also run against memory storage as a backend comparison.
Fault-injection fixtures stay with the behavior they test: local branch admission
and panic recovery use unit fixtures; publication failures across storage tables
use integration fixtures.

`branch`, `cells`, `index`, `merkle`, and `record` depend on the shared storage abstraction,
never the MDBX adapter. The integration-test package combines them with
`golemdb-storage-mdbx`; MDBX tests run by default there, without feature gates.
Import MDBX types from `golemdb_storage_mdbx` and traits from `golemdb_storage`.
The old `mdbx` features on the core crates have been removed.

```sh
cargo test -p golemdb-branch --lib                  # branch unit tests, no MDBX
cargo nextest run -p golemdb-integration-tests      # all integration suites
cargo nextest run -p golemdb-integration-tests --test branch
cargo test --workspace --doc                      # API examples
cargo bench -p golemdb-integration-tests --bench index --no-run
```

The workspace-wide test command includes both unit and integration tests. Keep
MDBX and other external backend fixtures in the integration package so native
test dependencies do not leak back into the core crates. Using an existing
production dependency as a lightweight unit-test fixture is fine.

## CI

See [performance benchmarks](docs/benchmarks.md) for the cells, Merkle and index
Criterion suites, workload filters, timing boundaries and baseline comparisons.

`.github/workflows/ci.yml` builds the workspace, runs the test suite with
[cargo-nextest](https://nexte.st/) in one workspace run with all features enabled,
runs documentation examples, and checks that benchmarks compile. Individual
members without tests do not fail that workspace run. There are currently no
crate feature combinations requiring a separate cargo-hack matrix.

## Dev container (optional)

`.devcontainer/` holds a light container with the pinned Rust toolchain
and cargo-nextest.
Open the repo in VS Code and choose **Reopen in Container**;
native setups are unaffected.
Build output and the cargo caches live on named Docker volumes,
so rebuilding the container keeps them.

```sh
# Basic build and test steps inside the container
cargo build --workspace
cargo nextest run --workspace
```
