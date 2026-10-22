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
    golemdb_api::RecordLimits { max_cells: 100, max_indexed_cells: 100 },
    None,
));
let db = Database::open("./golemdb-quickstart", &config)?;
let branch = db.begin()?;
let key = RecordKey([0x42; 32]);
db.create(
    branch,
    RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))
        .attribute("price", CellValue::from_i32(50))
        .field("description", CellValue::from_str("A product")),
).into_result()?;
let pending = db
    .clone()
    .get(ReadTarget::Branch(branch), RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
db.commit(branch)?;
assert_eq!(db.get(ReadTarget::Head, RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?, pending);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Only `commit` publishes changes. Dropping the last clone discards pending work;
committed MDBX state survives reopening. Handles belong to one engine: separate
opens over shared storage have separate registries. Use `clone` to share handles.

## Inputs and reads

```rust
use golemdb_api::{CellValue, RecordKey, RecordOp};

let key = RecordKey([0x42; 32]);

let record = RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))
    .attribute("price", CellValue::from_i32(50))
    .field("description", CellValue::from_str("A product"));
let patch = RecordOp::patch(key, golemdb_api::Budget::Limited(u64::MAX))
    .attribute("price", CellValue::from_i32(75))
    .remove("description");
let read = RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX)).only(["price", "#key"]);
# Ok::<(), golemdb_api::ApiError>(())
```

`RecordOp<op::Create>`, `RecordOp<op::Patch>`, `RecordOp<op::Get>`, and
`RecordOp<op::Delete>` bundle the key with that operation's inputs. Each CRUD method
accepts only its matching type; branch or read target stays a separate argument.
Caller-provided keys are required in every constructor. For deletion, use
`db.delete(branch, RecordOp::delete(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()`.

Explicit cell constructors own value encoding. `.attribute()` and `.field()` record
an explicit requested kind without changing the supplied value. A patch's `.set()`
preserves the stored kind and fails with `CellNotFound` if the cell is absent; a
create has no `.set()` or `.insert()`.

Builder steps only collect input, preserving order, duplicates and malformed names.
They perform no request validation or normalization and provide no `validate()`
method. After handle/snapshot selection, record execution checks reserved keys,
then admits entries in input order: name/value lengths, name grammar, requested
kind and duplicate names. Admission errors retain their typed cause through
`Error::source()` and return a receipt with zero write effects.

Operations implement `Clone`, `PartialEq`, and `Eq`. All expose `record_key()`,
`budget()` and `max_cost()`. Create and patch expose `entries()` as an ordered slice
of `CreateEntry` or `PatchEntry`; these include invalid and duplicate entries.
`CreateEntry.kind` and `PatchChange::Write { kind, value }` keep the requested kind
separate from the original value. `PatchChange::Set` and `Remove` preserve the other
patch actions. Reads expose the original projection through `names()`.

The API transfers these record-owned entries and the budget to execution without
building a second validated map. The private record admission module prepares that
map. Metering will check each admission charge before the corresponding work;
validation that rejects input must still record its charge before returning the
error. Charging and total request-size/count caps remain planned work. Typed value
construction, input allocation and transport decoding remain host responsibilities.

## Receipts and budgets

CRUD, query and count return `Metered<T> { result, receipt }` on both success and
failure. Read the receipt before propagating an error; `.into_result()?` explicitly
discards it. `map` and `map_err` preserve the receipt.

`receipt.cost` is currently **zero**. `receipt.priced_at` is the branch origin or
committed snapshot's head, including for rejected historical reads. Invalid handles
and failures before selecting a snapshot have no pricing commit. `receipt.details`
counts actual user-cell changes and encoded bytes; it excludes system cells,
no-ops, failed writes and reads. Rollback does not revoke earlier receipts.

Every request constructor requires `Budget::Limited(n)` or `Budget::Unlimited`:
`RecordOp::create(key, budget)`, `patch(key, budget)`, `get(key, budget)`,
`delete(key, budget)`, `QueryOp::from_filter(filter, budget)` and
`CountOp::filter(filter, budget)`. Budgets are stored but **not enforced** yet,
including a zero budget. `OutOfBudget { required }` is defined for later enforcement
and is never returned in this increment. Unlimited use is an explicit host decision.

`ReadTarget::Branch(id)` reads work in progress. `ReadTarget::Head` resolves head
and reads the record in one storage snapshot. `ReadTarget::Commit(id)` currently
accepts only that snapshot's head; historical reads are deferred.
`RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))` reads all cells. Its `.only(names)` helper accepts text or raw
byte names for reserved records. The builder preserves duplicates and input order;
execution deduplicates and sorts after selecting the target. Calling `.only()` again
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

A record may have zero user cells. A full read still contains its system `#key` and `#meta`;
its binding and identity remain until `delete`. Removing the last user cell does
not delete the record. An empty projection returns no cells but still verifies
that the record exists. Operation pricing remains deferred.

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
    golemdb_api::RecordLimits { max_cells: 100, max_indexed_cells: 100 },
    None,
))
.with_mode(OpenMode::CreateNew);
let db = Database::open("./golemdb-empty-records", &config)?;
let branch = db.begin()?;
let key = RecordKey([7; 32]);
db.create(branch, RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
db.patch(
    branch,
    RecordOp::patch(key, golemdb_api::Budget::Limited(u64::MAX)).attribute("price", CellValue::from_i32(50)),
).into_result()?;
db.patch(branch, RecordOp::patch(key, golemdb_api::Budget::Limited(u64::MAX)).remove("price")).into_result()?;
db.commit(branch)?;

let empty = db.get(ReadTarget::Head, RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
assert_eq!(empty.cells.len(), 2);
assert_eq!(empty.cells[b"#key".as_slice()].as_bytes32(), Some(key.0));
assert_eq!(empty.meta(), Some(golemdb_api::RecordMeta::default()));

let branch = db.begin()?;
db.delete(branch, RecordOp::delete(key, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
db.commit(branch)?;
assert!(matches!(
    db.get(ReadTarget::Head, RecordOp::get(key, golemdb_api::Budget::Limited(u64::MAX))).into_result(),
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
        golemdb_api::RecordLimits { max_cells: 100, max_indexed_cells: 100 },
        None,
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
            RecordOp::create(key, golemdb_api::Budget::Limited(u64::MAX))
                .attribute("name", CellValue::from_str(name))
                .attribute("price_cents", CellValue::from_i32(price_cents))
                .field("description", CellValue::from_str(description)),
        ).into_result()?;
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
    let saved_laptop = db.get(ReadTarget::Head, RecordOp::get(laptop, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
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
        RecordOp::get(keyboard, golemdb_api::Budget::Limited(u64::MAX)).only(["name", "price_cents"]),
    ).into_result()?;
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
        RecordOp::patch(laptop, golemdb_api::Budget::Limited(u64::MAX))
            .attribute("price_cents", CellValue::from_i32(110_000))
            .field("stock", CellValue::from_u32(5))
            .remove("description"),
    ).into_result()?;
    db.create(
        branch,
        RecordOp::create(monitor, golemdb_api::Budget::Limited(u64::MAX))
            .attribute("name", CellValue::from_str("Monitor"))
            .attribute("price_cents", CellValue::from_i32(30_000))
            .field("description", CellValue::from_str("27-inch monitor")),
    ).into_result()?;
    db.delete(branch, RecordOp::delete(mouse, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;

    // Branch reads see the changes immediately; head still contains commit 1.
    let pending_laptop = db.get(ReadTarget::Branch(branch), RecordOp::get(laptop, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
    assert_eq!(
        pending_laptop.cells[b"price_cents".as_slice()].as_i32(),
        Some(110_000)
    );
    assert_eq!(
        db.get(ReadTarget::Head, RecordOp::get(laptop, golemdb_api::Budget::Limited(u64::MAX))).into_result()?,
        saved_laptop
    );
    assert!(matches!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(mouse, golemdb_api::Budget::Limited(u64::MAX))).into_result(),
        Err(ApiError::NotFound)
    ));
    assert!(db.get(ReadTarget::Head, RecordOp::get(mouse, golemdb_api::Budget::Limited(u64::MAX))).into_result().is_ok());

    // Sealing preserves branch reads and rejects writes, checkpoints, and rollback.
    // The seal-added #roots cell becomes visible only after commit.
    let second_seal = db.seal(branch)?;
    assert_eq!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(laptop, golemdb_api::Budget::Limited(u64::MAX))).into_result()?,
        pending_laptop
    );
    println!("Candidate commit: {}", second_seal.commit_id);
    println!("State root: {:02x?}", second_seal.state_root);
    println!("Index root: {:02x?}", second_seal.index_root);
    assert_eq!(db.head()?, first_commit);
    assert_eq!(
        db.get(ReadTarget::Head, RecordOp::get(laptop, golemdb_api::Budget::Limited(u64::MAX))).into_result()?,
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
    let updated_laptop = db.get(ReadTarget::Head, RecordOp::get(laptop, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
    assert_eq!(updated_laptop, pending_laptop);
    assert_eq!(updated_laptop.cells[b"stock".as_slice()].as_u32(), Some(5));
    assert!(!updated_laptop.cells.contains_key(b"description".as_slice()));

    let unchanged_keyboard = db.get(
        ReadTarget::Head,
        RecordOp::get(keyboard, golemdb_api::Budget::Limited(u64::MAX)).only(["name", "price_cents"]),
    ).into_result()?;
    assert_eq!(unchanged_keyboard, saved_keyboard);

    let new_monitor = db.get(ReadTarget::Head, RecordOp::get(monitor, golemdb_api::Budget::Limited(u64::MAX))).into_result()?;
    assert_eq!(
        new_monitor.cells[b"name".as_slice()].as_str(),
        Some("Monitor")
    );
    assert_eq!(
        new_monitor.cells[b"price_cents".as_slice()].as_i32(),
        Some(30_000)
    );
    assert!(matches!(
        db.get(ReadTarget::Head, RecordOp::get(mouse, golemdb_api::Budget::Limited(u64::MAX))).into_result(),
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
record_limits:
  max_cells: 100
  max_indexed_cells: 100
immutable_data:
  shard_span: 500000
  segments:
    - name: headers
      compression: lz4
      columns:
        - {name: block_height, type: u64}
        - {name: block_body} # bytes by default
```

`blake3` is also supported. Omit `immutable_data` or set it to `null` to disable
immutable storage. An enabled configuration requires at least one segment and a
positive shard span. Other fields are required except a column's `type`, which
defaults to `bytes`. Duplicate and unknown fields are rejected.
Use `Genesis::load(path)` or `Genesis::from_yaml(text)`; the loader
does not search for files or read environment overrides.

Typed construction uses `Genesis::new(hash, cell_limits, record_limits, None)` to disable
immutable data, or `Genesis::new(hash, cell_limits, record_limits,
Some(ImmutableDataConfig::new(shard_span, segments)))` to enable it. These
configuration types own their declarations and implement `Clone`, not `Copy`.

`RecordLimits { max_cells, max_indexed_cells }` contains explicit `u64` caps,
stored at genesis as `#params.#maxRecordCells` and `#params.#maxRecordIndexedCells`.
Creates and patches check the final user-cell counts before writing; violations
return `InvalidArgument` with zero applied effects and do not consume an ID.
System and admin records are exempt. Zero caps allow empty user records;
`max_indexed_cells: 0` allows fields only. The values above are examples, not defaults.

Each user record stores `#meta` as four big-endian `u64` counters in a `bytes32`
field: `cells`, `cell_bytes`, `indexed_cells`, `index_bytes`. Metadata and receipt
details use the same user effects and exclude system cells. Patches rewrite
`#meta` only when a counter changes. Full reads and `.only(["#meta"])` include it.
Reads return the stored cells without validating metadata. `Record::meta()` returns
its decoded `RecordMeta`, or `None` if the cell is absent or malformed. Patch and
delete validate metadata before using its counters: missing or malformed metadata,
or counter underflow, returns `Internal` without repair.

Segment names match `[a-z][a-z0-9_]{0,63}` and are rejected rather than normalized
when invalid. Column names use the cell user-name grammar, independently of the
user-cell length limits. Segment names are unique; column names are unique within
a segment, and their order is significant. Column types use the existing cell
types: `bool`, `str`, `bytes`, `bytes4/8/16/20/32`, `u32/64/128/256`,
`i32/64/128/256`, `dec32/64/128/256`, `f32/64`, `date32`, and `timestamp64`.
Compression is explicitly `none`, `lz4`, or ordinary `zstd`.

The current format allows up to 64 user segments and 1–64 columns per segment,
with positive `u64` shard spans. The encoded segment declaration is limited to
16,383 bytes so its tagged parameter cell fits the 16 KiB engine value budget.
These are configuration limits; they do not cap immutable row payloads.

When enabled, genesis writes `#params.#shardSpan` as a `u64` field and
`#params.#immutableDataSegments` as a bytes field. Both cells are absent when
disabled; no placeholder span or empty schema is persisted. Declarations are encoded with
fixed-width lengths and type/compression IDs, with segments sorted by name and
columns in declaration order. Zero-based `u32` segment IDs follow that sorted
order. YAML formatting, segment order and omitted versus explicit `bytes` types
have identical commitments. Changing a name, column order, type, compression or
shard span, or enabling/disabling immutable data, changes genesis identity. Reopening checks the stored parameter cells
and their trie commitments without rewriting them.

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
bindings, the configured limits and immutable declarations, allocator at 64, and a
real cell trie. Every genesis cell is a field, so the index root is empty. The transaction writes format,
hash, Roaring, genesis identity, and head metadata before publishing commit 0.
`#roots` and `#rootIndex` contain their identity cells but no history entries.
The two metering records have identities and bindings only; metering is deferred.

The opener validates physical backend ceilings, including complete cell/index
keys and value tags. The format also requires room for 16 KiB engine values, covering
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

## Immutable data

Genesis declares each segment's ordered, named, typed columns and compression.
The API re-exports the immutable-data types and cell codecs:

| Method | Behavior |
| --- | --- |
| `immutable_data_schema(segment)` | Return the declared column names and types. |
| `immutable_data_append(branch, segment, key, row)` | Stage a complete typed row and return its provisional `u64` ordinal. |
| `immutable_data_truncate(branch, segment, from_ordinal)` | Remove only the branch's staged suffix, releasing its keys and ordinals. |
| `immutable_data_get(segment, address, &options)` | Read a committed row by ordinal or optional 32-byte key. |
| `immutable_data_rows_of(segment, commit, &options)` | Read that commit's rows in append order; an existing empty commit returns no rows. |

Append and truncate work before and after seal, independently of live-state
checkpoints and rollback. Files are appended and synced during commit; MDBX then
publishes live state, key bindings, commit ranges and head atomically. Losing and
discarded branches publish no rows. Keys are unique per segment; duplicate keys
return `AlreadyExists`. Cell size limits do not constrain immutable payloads.

Reads use one published snapshot. `ImmutableDataReadOptions::all()` selects every
column; `columns(["hash", "height"])` selects named columns in caller order.
Explicitly empty, duplicate or unknown projections return `InvalidArgument`.
Unknown segments and absent rows return `NotFound`; future commits return
`CommitUnavailable`. Read rows own their values and layout metadata. Use
`row.get::<T>("column")` for checked native conversion, `value` for borrowed cell
access, or `iter` for named values. Schema types remain exact when projecting.
Row equality compares typed values in order, ignoring column names, schema and
projection metadata. A full read row equals its append input; projections of
different columns also compare equal when their ordered typed values match.

For a segment declared as `height: u64`, `hash: bytes32`:

```rust
use golemdb_api::{Api, CellValue, ImmutableDataAddress, ImmutableDataKey,
    ImmutableDataReadOptions, ImmutableDataRow};

let branch = db.begin()?;
let ordinal = db.immutable_data_append(
    branch,
    "headers",
    Some(ImmutableDataKey([7; 32])),
    ImmutableDataRow::new(vec![CellValue::from_u64(42), CellValue::from_bytes32([9; 32])]),
)?;
let commit = db.commit(branch)?;
let projection = ImmutableDataReadOptions::columns(["hash"]);
let row = db.immutable_data_get("headers", ImmutableDataAddress::Ordinal(ordinal), &projection)?;
let hash: [u8; 32] = row.get("hash")?;
let rows = db.immutable_data_rows_of("headers", commit, &projection)?;
```

`Database::open` defaults to `<database>/immutable-data`; override with
`OpenConfig::with_immutable_data_directory(path)`. `open_memory` uses memory for
both live state and immutable rows. `from_store` requires an explicit immutable
directory when segments are declared; it cannot infer the supplied store's
persistence. The directory is local configuration and does not change genesis
identity. `Genesis::immutable_data: None` disables immutable storage and creates no files.
Close all database clones before reopening the same immutable directory.
Startup recovers unpublished tails against durable commit metadata.

Retention/pruning, key expiry and digests remain deferred.

## Current scope

Typed cells, record CRUD, branch lifecycle, atomic opening/genesis, and typed
immutable data are implemented behind a cloneable `Database` with shared state.
Queries support `Eq`, `In`, `Range`, `Prefix` and `Exists` filters, combined by `And` and `Or`
trees, with full or projected records, and `count`. A filter nests at most 64 `And` / `Or`
levels, a hardcoded cap until it is read from genesis; a deeper one returns
`ApiError::LimitExceeded`. A query returns at most 100 records, the first matches in record ID
order, a hardcoded page size until it is read from genesis; `total_matched` and `count` still
count every match. `limit(n)` returns at most `n` records: a `limit` of 0 returns
`ApiError::InvalidQuery`, and one above the page size `ApiError::LimitExceeded`, whose `max` is
the page size. `offset(n)` starts the page past the first `n` matches, so the next page's
offset is the previous one plus the records returned; an offset past the last match returns an
empty page. A `Prefix` matches `str` values only. An `Exists` matches the records
holding an attribute of its name: of its type, or, with no type, of any type. `Eq` values, `In`
values, `Range` bounds and `Exists` types are of one indexable type by construction:
`EqValue::of(marker::Str, "blue")`, `InValues::of(marker::U32, [1, 2])`,
`Bounds::new(marker::Str, ..)` and `ExistsType::of(marker::U32)` take a cell-type marker, so mixed
types or `bytes` do not compile; `EqValue::try_from`, `InValues::try_from`,
`Bounds::try_new(ty, ..)` and `ExistsType::try_from(ty)` check at run time (`ValuesError`, which
converts into `ApiError::InvalidArgument`). A `Range` with two unbounded sides matches every cell
of its type.
Queries read committed state only: `None` reads the head, and an explicit commit
other than the head returns `ApiError::CommitUnavailable`. Sort returns
`ApiError::NotImplemented`. Query budgets are accepted but not enforced. Cost
receipts, generated keys, OCC, and transport serialization are deferred.

Use scripted mocks for consumer error paths behind `Arc<dyn Api + Send + Sync>`. For tests, replace the storage opening with
`Database::open_memory(&config.genesis)?` to get a fresh in-memory database. Arkiv can
pass `Arc::new(Database::open_memory(&config.genesis)?)`
as `Arc<dyn Api + Send + Sync>` to run behavioral tests with real database semantics.
The [facade contract tests](../integration-tests/tests/api/facade.rs) run the same scenarios against memory
and MDBX with both hash algorithms. The [error tests](../integration-tests/tests/api/facade_errors.rs)
exercise backend diagnostics and retries through the real facade.
CLI/HTTP/TCP adapters own transport parsing and serialization.
