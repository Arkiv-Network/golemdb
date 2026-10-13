# Public Rust API

`golemdb-api` defines one synchronous, dyn-compatible `Api` trait:

- Records: create, get, patch, delete.
- Queries: query, count, over committed state (from `golemdb-query`).
- Branches: head, begin, branch_info, checkpoint, rollback, seal, commit, discard.

Consumers accept `&dyn Api` or `Arc<dyn Api + Send + Sync>`. The trait does not
impose threading bounds; consumers add them when needed. Adding required methods
requires updating concrete implementations and mocks.

`Database` implements `Api` and hides backend and hash-provider types. Clones share
one engine and branch registry, so handles work across clones and threads. The
hash is selected on opening and stays a concrete type within the engine. Import
`Api` to use its methods on `Database`.

The examples use persistent MDBX storage. `Database::open(path, &config)` opens
MDBX internally; no feature flag is needed.
Use a fresh database directory for the catalogue examples.

```rust
use golemdb_api::{
    Api, CellLimits, CellValue, Database, Genesis, HashAlgorithm, OpenConfig, ReadTarget,
    RecordKey, RecordOp,
};

let config = OpenConfig::new(Genesis::new(
    HashAlgorithm::Keccak256,
    CellLimits {
        max_cell_name_len: 32,
        max_str_len: 64,
        max_bytes_len: 128,
    },
));
let db = Database::open("./golemdb-quickstart", &config)?;
let branch = db.begin()?;
let key = RecordKey([0x42; 32]);
db.create(
    branch,
    RecordOp::create(key)
        .attribute("price", CellValue::from_i32(50))?
        .field("description", CellValue::from_str("A product"))?,
)?;
let pending = db
    .clone()
    .get(ReadTarget::Branch(branch), RecordOp::get(key))?;
db.commit(branch)?;
assert_eq!(db.get(ReadTarget::Head, RecordOp::get(key))?, pending);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Only `commit` publishes changes. Dropping the last clone discards pending work;
committed MDBX state survives reopening. Handles belong to one engine: separate
opens over shared storage have separate registries. Use `clone` to share handles.

## Inputs and reads

```rust
use golemdb_api::{CellValue, RecordKey, RecordOp};

let key = RecordKey([0x42; 32]);

let record = RecordOp::create(key)
    .attribute("price", CellValue::from_i32(50))?
    .field("description", CellValue::from_str("A product"))?;
let patch = RecordOp::patch(key)
    .attribute("price", CellValue::from_i32(75))?
    .remove("description")?;
let read = RecordOp::get(key).only(["price", "#key"]);
# Ok::<(), golemdb_api::ApiError>(())
```

`RecordOp<op::Create>`, `RecordOp<op::Patch>`, `RecordOp<op::Get>`, and
`RecordOp<op::Delete>` bundle the key with that operation's inputs. Each CRUD method
accepts only its matching type; branch or read target stays a separate argument.
Caller-provided keys are required in every constructor. For deletion, use
`db.delete(branch, RecordOp::delete(key))`.

Explicit cell constructors own value encoding. The operation helpers choose field or
attribute kind; create's `.insert()` and patch's `.set()` preserve the supplied
value's kind. Builders reject invalid names, unindexable attributes, and duplicate
names, including a set and removal for the same name. Errors are returned immediately
and identify the step and cell, for example `attribute("payload"): invalid cell value`;
the typed cause is available through `Error::source()`. Error messages provide context
without repeating the cause. Deployment limits and record invariants remain
execution-time checks in record.

Operations implement `Clone`, `PartialEq`, and `Eq` for mocks and retry preparation.
All expose `record_key()`. Creates expose `value()` and `cells()`; patches expose
`value()`, `removes()`, and `changes()` (a value for sets, `None` for removals).
Reads expose `names()`. The API no longer re-exports `RecordCells`, `RecordPatch`,
or `CellPatch`; those types remain available in the lower-level record crate.
Metering options and receipts will be designed in a later increment.

`ReadTarget::Branch(id)` reads work in progress. `ReadTarget::Head` resolves head
and reads the record in one storage snapshot. `ReadTarget::Commit(id)` currently
accepts only that snapshot's head; historical reads are deferred.
`RecordOp::get(key)` reads all cells. Its `.only(names)` helper accepts text or raw
byte names for reserved records, deduplicated in byte order. Calling `.only()` again
replaces the projection; an empty projection still checks existence.

`ApiError` exposes common failures directly and retains diagnostic sources for
invalid input and internal failures. A live stale branch's commit returns
`Conflict` and consumes the handle. Subsequent calls, unknown handles, and handles
already invalidated by another operation return `HandleInvalid`. `SealInfo`
contains only the candidate commit ID and the state/index roots.

`BranchId` and `CommitId` are distinct types. Use `CommitId::new(number)` for an
explicit committed target and `.get()` to retrieve an ID's underlying `u64`.
Sealed branches remain readable; writes, checkpoints, and rollback return
`ApiError::Sealed`. The `#roots` cell added during sealing appears after commit.
Storage capacity exhaustion returns `ApiError::StoreFull`. A capacity failure during
publication leaves committed state unchanged and retains the sealed branch for retry
or discard.

## Empty records

A record may have zero user cells. A full read still contains its system `#key`;
its binding and identity remain until `delete`. Removing the last user cell does
not delete the record. An empty projection returns no cells but still verifies
that the record exists. `#meta` and metering remain deferred.

```rust
use golemdb_api::{
    Api, ApiError, CellLimits, CellValue, Database, Genesis, HashAlgorithm, OpenConfig,
    OpenMode, ReadTarget, RecordKey, RecordOp,
};

let config = OpenConfig::new(Genesis::new(
    HashAlgorithm::Keccak256,
    CellLimits {
        max_cell_name_len: 32,
        max_str_len: 64,
        max_bytes_len: 128,
    },
))
.with_mode(OpenMode::CreateNew);
let db = Database::open("./golemdb-empty-records", &config)?;
let branch = db.begin()?;
let key = RecordKey([7; 32]);
db.create(branch, RecordOp::create(key))?;
db.patch(
    branch,
    RecordOp::patch(key).attribute("price", CellValue::from_i32(50))?,
)?;
db.patch(branch, RecordOp::patch(key).remove("price")?)?;
db.commit(branch)?;

let empty = db.get(ReadTarget::Head, RecordOp::get(key))?;
assert_eq!(empty.cells.len(), 1);
assert_eq!(empty.cells[b"#key".as_slice()].as_bytes32(), Some(key.0));

let branch = db.begin()?;
db.delete(branch, RecordOp::delete(key))?;
db.commit(branch)?;
assert!(matches!(
    db.get(ReadTarget::Head, RecordOp::get(key)),
    Err(ApiError::NotFound)
));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use a fresh directory to run this example again.

## Example: two commits over a small product catalogue

This complete example creates three records, reads committed data, then changes
the catalogue in a second branch. The second commit updates a price, adds a cell,
removes a cell, creates a record, and deletes another record. Prices are integer
cents, and every record has an explicit caller-provided key. `CreateNew` requires
uninitialized storage at `./golemdb-catalogue`, so the two commits start at 1 and 2.
Use a different fresh directory to run this example again.

Both commits explicitly call `seal` to compute their roots before publication.
Sealing is optional (`commit` seals automatically if needed). It freezes branch
writes without advancing the head or reserving the candidate commit ID;
another branch can still win the commit race.

```rust
use golemdb_api::{
    Api, ApiError, CellLimits, CellValue, Database, Genesis, HashAlgorithm, OpenConfig, OpenMode,
    ReadTarget, RecordKey, RecordOp,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = OpenConfig::new(Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 128,
            max_bytes_len: 1024,
        },
    ))
    .with_mode(OpenMode::CreateNew);
    let db = Database::open("./golemdb-catalogue", &config)?;
    let laptop = RecordKey([1; 32]);
    let keyboard = RecordKey([2; 32]);
    let mouse = RecordKey([3; 32]);
    let monitor = RecordKey([4; 32]);

    // 1. Create three records in one branch and publish them together.
    let branch = db.begin()?;
    for (key, name, price_cents, description) in [
        (laptop, "Laptop", 120_000, "14-inch laptop"),
        (keyboard, "Keyboard", 8_000, "Mechanical keyboard"),
        (mouse, "Mouse", 3_000, "Wireless mouse"),
    ] {
        db.create(
            branch,
            RecordOp::create(key)
                .attribute("name", CellValue::from_str(name))?
                .attribute("price_cents", CellValue::from_i32(price_cents))?
                .field("description", CellValue::from_str(description))?,
        )?;
    }
    // Seal computes roots and freezes the branch; nothing is published yet.
    let first_seal = db.seal(branch)?;
    assert_eq!(first_seal.commit_id.get(), 1);
    assert_eq!(db.head()?.get(), 0);
    assert!(db.branch_info(branch)?.sealed);
    let first_commit = db.commit(branch)?;
    assert_eq!(first_commit, first_seal.commit_id);
    assert_eq!(first_commit.get(), 1); // Opening created genesis at commit 0.

    // 2. Read a full record and a projection from committed state.
    let saved_laptop = db.get(ReadTarget::Head, RecordOp::get(laptop))?;
    assert_eq!(
        saved_laptop.cells[b"price_cents".as_slice()].as_i32(),
        Some(120_000)
    );
    assert_eq!(
        saved_laptop.cells[b"#key".as_slice()].as_bytes32(),
        Some(laptop.0)
    );

    // An explicit commit target is supported while that commit is the head.
    let saved_keyboard = db.get(
        ReadTarget::Commit(first_commit),
        RecordOp::get(keyboard).only(["name", "price_cents"]),
    )?;
    assert_eq!(saved_keyboard.cells.len(), 2);
    assert_eq!(
        saved_keyboard.cells[b"name".as_slice()].as_str(),
        Some("Keyboard")
    );

    // 3. Start a new branch: change a price, add stock, remove a description,
    //    add a monitor record, and delete the mouse record.
    let branch = db.begin()?;
    db.patch(
        branch,
        RecordOp::patch(laptop)
            .attribute("price_cents", CellValue::from_i32(110_000))?
            .field("stock", CellValue::from_u32(5))?
            .remove("description")?,
    )?;
    db.create(
        branch,
        RecordOp::create(monitor)
            .attribute("name", CellValue::from_str("Monitor"))?
            .attribute("price_cents", CellValue::from_i32(30_000))?
            .field("description", CellValue::from_str("27-inch monitor"))?,
    )?;
    db.delete(branch, RecordOp::delete(mouse))?;

    // Branch reads see the changes immediately; head still contains commit 1.
    let pending_laptop = db.get(ReadTarget::Branch(branch), RecordOp::get(laptop))?;
    assert_eq!(
        pending_laptop.cells[b"price_cents".as_slice()].as_i32(),
        Some(110_000)
    );
    assert_eq!(
        db.get(ReadTarget::Head, RecordOp::get(laptop))?,
        saved_laptop
    );
    assert!(matches!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(mouse)),
        Err(ApiError::NotFound)
    ));
    assert!(db.get(ReadTarget::Head, RecordOp::get(mouse)).is_ok());

    // Sealing preserves branch reads and rejects writes, checkpoints, and rollback.
    // The seal-added #roots cell becomes visible only after commit.
    let second_seal = db.seal(branch)?;
    assert_eq!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(laptop))?,
        pending_laptop
    );
    println!("Candidate commit: {}", second_seal.commit_id);
    println!("State root: {:02x?}", second_seal.state_root);
    println!("Index root: {:02x?}", second_seal.index_root);
    assert_eq!(db.head()?, first_commit);
    assert_eq!(
        db.get(ReadTarget::Head, RecordOp::get(laptop))?,
        saved_laptop
    );
    assert_eq!(db.seal(branch)?, second_seal); // Repeated seal returns the same result.

    // Commit publishes the sealed changes and consumes the branch handle.
    let second_commit = db.commit(branch)?;
    assert_eq!(second_commit, second_seal.commit_id);
    assert_eq!(second_commit.get(), 2);
    assert_eq!(db.head()?, second_commit);

    // 4. Read again: laptop changed, keyboard stayed the same, monitor appeared,
    //    and mouse disappeared. Use Head now; historical commit 1 is deferred.
    let updated_laptop = db.get(ReadTarget::Head, RecordOp::get(laptop))?;
    assert_eq!(updated_laptop, pending_laptop);
    assert_eq!(updated_laptop.cells[b"stock".as_slice()].as_u32(), Some(5));
    assert!(!updated_laptop.cells.contains_key(b"description".as_slice()));

    let unchanged_keyboard = db.get(
        ReadTarget::Head,
        RecordOp::get(keyboard).only(["name", "price_cents"]),
    )?;
    assert_eq!(unchanged_keyboard, saved_keyboard);

    let new_monitor = db.get(ReadTarget::Head, RecordOp::get(monitor))?;
    assert_eq!(
        new_monitor.cells[b"name".as_slice()].as_str(),
        Some("Monitor")
    );
    assert_eq!(
        new_monitor.cells[b"price_cents".as_slice()].as_i32(),
        Some(30_000)
    );
    assert!(matches!(
        db.get(ReadTarget::Head, RecordOp::get(mouse)),
        Err(ApiError::NotFound)
    ));

    Ok(())
}
```

## Opening and genesis

Provide immutable startup values explicitly, as `Genesis` or a caller-selected YAML file:

```yaml
hash_function: keccak-256
cell_limits:
  max_cell_name_len: 32
  max_str_len: 64
  max_bytes_len: 128
```

`blake3` is also supported. Missing, duplicate, and unknown fields are rejected.
Use `Genesis::load(path)` or `Genesis::from_yaml(text)`; the loader
does not search for files or read environment overrides.

```rust
use golemdb_api::{Database, Genesis, OpenConfig};

let genesis = Genesis::load("genesis.yaml")?;
let config = OpenConfig::new(genesis); // CreateIfMissing
let db = Database::open("./golemdb", &config)?;
# Ok::<(), golemdb_api::OpenError>(())
```

`Database::open(path, &config)` initializes or reopens MDBX storage with default
capacity settings. For local capacity settings, use
`Database::open_with_options(path, &config, options)` with `MdbxOptions`.
These settings are separate from `Genesis` and do not affect genesis identity.

For tests, `Database::open_memory(&genesis)` initializes a fresh memory store.
`Database::from_store(store, &config)` accepts a caller-supplied implementation of
`golemdb_storage::Store`, including a clone of a shared store. `MemoryStore` and
`MdbxStore` implement `Store` in the `golemdb-storage` and `golemdb-storage-mdbx`
crates respectively. The API depends on MDBX directly.

Use `OpenConfig::new(genesis).with_mode(mode)` to select an opening mode.
Clone an existing handle to share its engine. Do not independently open the same
MDBX directory twice within one process.

| Mode | Pristine storage | Initialized storage |
| --- | --- | --- |
| `CreateIfMissing` (default) | Create commit 0 | Validate and reopen |
| `ExistingOnly` | `NotInitialized` | Validate and reopen |
| `CreateNew` | Create commit 0 | `AlreadyInitialized` after validation |

Pristine means no tables or rows. A pre-existing empty directory is allowed for
creation. Tables without a head, including empty or foreign tables, are rejected
as incomplete state. `ExistingOnly` never writes genesis or creates a missing
directory; opening an existing empty directory can create MDBX environment files.

All detection, validation, and initialization occurs under one storage writer.
Creation writes the fixed reserved-record catalogue from cells, identities and
bindings, the configured limits, allocator at 64, and a real cell trie. Every
genesis cell is a field, so the index root is empty. The transaction writes format,
hash, Roaring, genesis identity, and head metadata before publishing commit 0.
`#roots` and `#rootIndex` contain their identity cells but no history entries.
The two metering records have identities and bindings only; metering is deferred.

The opener validates physical backend ceilings, including complete cell/index
keys and value tags. Format 1 also requires room for 16 KiB engine values, covering
trie nodes and canonical bitmap containers. Limits are user-cell admission policy;
system root-history bytes do not consume the configured user bytes allowance.

Reopening compares the canonical genesis identity, validates format IDs, required
reserved cells, allocator, current roots, and previous-root history. Required
cells are checked against their committed trie paths. This is startup validation,
not a full audit of every user row or index subtree. It never rewrites parameters
or resets allocation. Formatting or field order in YAML does not affect identity;
changed deployment settings return `GenesisMismatch`.

`OpenError` distinguishes configuration, format, initialization, and storage
failures. `Database::info()` describes the opening snapshot; call `head()` for the
live head, or `get(ReadTarget::Head, ...)` for a consistent current record read.

## Current scope

Immutable data has an interface only. `Api` exposes:

- `immutable_data_append(branch, segment, key, row) -> ordinal`, where `key` is
  `Option<ImmutableDataKey>` and `row` is `Vec<Vec<u8>>` (one byte array per column).
- `immutable_data_get(segment, address) -> row`, with
  `ImmutableDataAddress::Ordinal(u64)` or `ImmutableDataAddress::Key(key)`.
- `immutable_data_range_of(segment, commit) -> Range<u64>`.
- `immutable_data_rows_of(segment, commit) -> Vec<ImmutableDataRow>`.

`ImmutableDataKey` contains 32 caller-provided bytes and is distinct from a record
key. The intended contract makes keys unique within a segment, with rows and key
bindings published together from a sealed branch at commit. Keys are optional;
all rows retain ordinal addressing. Duplicate keys will never overwrite rows.

**Every immutable-data call currently returns `ApiError::NotImplemented { operation }`
immediately**, including calls with invalid handles or unknown segments. Nothing
is validated, staged, allocated, read, or written. Segment configuration, files,
key indexes, and commit integration are deferred. The
[immutable-data specification](../../docs/golem-db-api.md#immutable-data) describes
the future behavior, including pruning and rewind of key bindings.

All four iterations are implemented: typed cells, the facade contract and builders,
atomic opening/genesis, and a cloneable `Database` with shared branch state.
Queries support `Eq` and `In` filters with full or projected records, and `count`.
Sort, limit and offset return `ApiError::NotImplemented`, and a branch target
returns `ApiError::InvalidQuery`. Query budgets are accepted but not enforced. Cost
receipts, generated keys, OCC, and transport serialization are deferred.

The [consumer test](src/tests/consumer.rs) demonstrates a single facade mock and
commit failure injection behind `Arc<dyn Api + Send + Sync>`. Use scripted mocks
for consumer error paths. For tests, replace the storage opening with
`Database::open_memory(&config.genesis)?` to get a fresh in-memory database. Arkiv can
pass `Arc::new(Database::open_memory(&config.genesis)?)`
as `Arc<dyn Api + Send + Sync>` to run behavioral tests with real database semantics.
The [facade contract tests](../integration-tests/tests/api/facade.rs) run the same scenarios against memory
and MDBX with both hash algorithms. The [error tests](../integration-tests/tests/api/facade_errors.rs)
exercise backend diagnostics and retries through the real facade.
CLI/HTTP/TCP adapters own transport parsing and serialization.
