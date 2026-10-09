# Golem DB — API v2

The Rust interface Golem DB presents to the layer above it, as implemented on branch
`matthiaszimmermann/feat/record-ops` (revision `cb323d8`, crate `golemdb-api`). This document
is precise enough to reimplement that API: signatures, rules, check order, encodings and errors.

Status: draft, 2026-10-09. It runs in parallel with [golem-db-api.md](golem-db-api.md) (v1)
until the two converge. v1 describes the target interface, including parts not yet built; v2
describes what exists. Where they differ, [Differences from v1](#differences-from-v1) says how.

Related documents:

- [golem-db-design.md](golem-db-design.md): data model, reserved records, commitment, branches.
  Cited as _design §n_.
- [golem-db-metering.md](golem-db-metering.md): costs, budgets, receipts. Cited as _metering Dn_.
  Metering is not implemented: every cost is 0 and budgets are not enforced.

## Contents

- [Shape of the API](#shape-of-the-api)
- [Opening a database](#opening-a-database) — [Genesis](#genesis) · [Config and open modes](#config-and-open-modes) · [Stores](#stores) · [Constructors](#constructors) · [What genesis writes](#what-genesis-writes) · [Reopening](#reopening) · [Opening errors](#opening-errors)
- [Data model](#data-model) — [Records](#records) · [Record keys](#record-keys) · [Cell names](#cell-names) · [Cell types and kinds](#cell-types-and-kinds) · [Rust values](#rust-values) · [Reserved records](#reserved-records) · [`#meta`](#meta)
- [Record operations](#record-operations) — [`RecordOp`](#recordop) · [`create`](#create) · [`get`](#get) · [`patch`](#patch) · [`delete`](#delete)
- [Receipts](#receipts)
- [Branches](#branches)
- [Immutable data](#immutable-data)
- [Errors](#errors)
- [Agreed changes, not yet implemented](#agreed-changes-not-yet-implemented)
- [Not implemented](#not-implemented)
- [Differences from v1](#differences-from-v1)

---

## Shape of the API

One synchronous, dyn-compatible trait, `Api`, implemented by the cloneable `Database` facade.
Consumers hold `&dyn Api` or `Arc<dyn Api + Send + Sync>`; the trait imposes no threading
bounds.

```rust
pub trait Api {
    // Records: metered, return the result together with a receipt.
    fn create(&self, branch: BranchId, op: RecordOp<op::Create>) -> Metered<RecordKey>;
    fn get(&self, target: ReadTarget, op: RecordOp<op::Get>) -> Metered<Record>;
    fn patch(&self, branch: BranchId, op: RecordOp<op::Patch>) -> Metered<()>;
    fn delete(&self, branch: BranchId, op: RecordOp<op::Delete>) -> Metered<()>;

    // Branches: unmetered.
    fn head(&self) -> Result<CommitId>;
    fn begin(&self) -> Result<BranchId>;
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo>;
    fn checkpoint(&self, branch: BranchId) -> Result<()>;
    fn rollback(&self, branch: BranchId) -> Result<()>;
    fn seal(&self, branch: BranchId) -> Result<SealInfo>;
    fn commit(&self, branch: BranchId) -> Result<CommitId>;
    fn discard(&self, branch: BranchId) -> Result<()>;

    // Immutable data: interface only, every call returns NotImplemented.
    fn immutable_data_append(&self, branch: BranchId, segment: &str,
        key: Option<ImmutableDataKey>, row: ImmutableDataRow) -> Result<ImmutableDataOrdinal>;
    fn immutable_data_get(&self, segment: &str, address: ImmutableDataAddress)
        -> Result<ImmutableDataRow>;
    fn immutable_data_range_of(&self, segment: &str, commit: CommitId)
        -> Result<Range<ImmutableDataOrdinal>>;
    fn immutable_data_rows_of(&self, segment: &str, commit: CommitId)
        -> Result<Vec<ImmutableDataRow>>;
}
```

`Result<T>` is `std::result::Result<T, ApiError>`. Adding a required method to `Api` breaks
implementations and mocks.

`Database` adds two inherent accessors besides its constructors:

- `genesis() -> &Genesis`: the deployment settings validated at opening.
- `info() -> &OpenInfo`: a snapshot of the opening, `{created, commit_id, state_root,
  index_root, genesis_id}`. It does not follow the head; call `head()` for that.

**Clones share state.** All clones of a `Database` share one branch registry, so a branch
begun through one clone works through any other and across threads. Open branches live in
memory: dropping the last clone discards them; only `commit` writes to the store.

**Dispatch.** The store and hash function are chosen at opening and hidden: `Database` has no
type parameters. Each call costs one dynamic dispatch; hashing within a call is static.

**Data-plane only.** `Database` exposes the validated operations above and nothing else: no
raw store writes, no direct access to reserved cells.

---

## Opening a database

### Genesis

`Genesis` holds the immutable deployment settings. They are identical on every node and can
never change for a database.

```rust
pub struct Genesis {
    pub hash_function: HashAlgorithm,   // Keccak256 | Blake3
    pub cell_limits: CellLimits,        // { max_cell_name_len, max_str_len, max_bytes_len }: u32
    pub record_keys: RecordKeys,        // CallerAssigned | Generated { seed: [u8; 32] }
}
```

As YAML, every field is required; missing, duplicate and unknown fields are rejected:

```yaml
hash_function: keccak-256        # or blake3
cell_limits:
  max_cell_name_len: 32
  max_str_len: 64
  max_bytes_len: 128
record_keys: caller_assigned     # or:  record_keys: { generated: { seed: "0x…" } }
```

- The seed is `0x` followed by exactly 64 hex digits.
- `Genesis::from_yaml(&str)` parses text; `Genesis::load(path)` reads that one file. Neither
  searches for files or reads environment variables.
- `Genesis::DEV` is for development, tests and examples: Keccak-256, limits 32 / 64 / 128,
  caller-assigned keys. Its values may change between releases.

Validation at opening, against the store's physical limits:

- `max_cell_name_len` must be positive.
- Required key size: `max(name + 8, name + 2 + max(max_str_len, 32), 40)` bytes, where `name`
  is `max_cell_name_len`. It must fit the store's maximum key.
- Required value size: `max(1 + max(max_bytes_len, max_str_len), 16 KiB)`. The 16 KiB floor
  covers internal values: trie nodes, metadata, root pairs and Roaring containers. It must fit
  the store's maximum value.

A failure is `OpenError::InvalidConfig`.

### Config and open modes

```rust
#[non_exhaustive]
pub struct Config { pub genesis: Genesis, pub mode: OpenMode }
// Config::new(genesis) sets mode = CreateIfMissing; Config::with_mode(mode) changes it.

#[non_exhaustive]
pub enum OpenMode { CreateIfMissing /* default */, ExistingOnly, CreateNew }
```

| Mode | Pristine store | Initialized store |
| --- | --- | --- |
| `CreateIfMissing` | write genesis, commit 0 | validate and reopen |
| `ExistingOnly` | `NotInitialized` | validate and reopen |
| `CreateNew` | write genesis, commit 0 | validate, then `AlreadyInitialized` |

A store is **pristine** if it has no tables and no rows. A store with tables or rows but no
head is `CorruptState`. Detection, validation and initialization run under one store writer.

### Stores

`StoreConfig` selects a built-in store. Store settings are local to a node, may change
between restarts, and are never part of the genesis identity.

```rust
#[non_exhaustive]
pub enum StoreConfig {
    Memory,                                            // fresh, in-memory
    #[cfg(feature = "mdbx")]
    Mdbx { path: PathBuf, options: MdbxOptions },      // MDBX environment in `path`
}
```

- With the `mdbx` feature, `&str`, `String`, `&Path` and `PathBuf` convert to
  `StoreConfig::Mdbx` with default options; `StoreConfig::mdbx(path)` does the same.
- `MdbxOptions::default()`: `max_tables` 128, `max_map_size` 1 GiB, `growth_step` 16 MiB. A
  larger `max_map_size` takes effect on reopening; a smaller one than the store has is ignored.
- A store file is YAML, kept separate from the genesis file:

  ```yaml
  mdbx:
    path: data                  # relative paths resolve against this file's directory
    options:                    # optional; each omitted option keeps its default
      max_map_size: 8589934592
  ```

  or `memory`. `StoreConfig::load(path)` resolves a relative `path` against the file's
  directory; `StoreConfig::from_yaml(text)` leaves it relative to the current directory.
  Unknown fields are rejected (`OpenError::StoreYaml`). A file that selects MDBX in a build
  without the `mdbx` feature is `InvalidConfig`.

### Constructors

| Constructor | Use |
| --- | --- |
| `Database::open(store: impl Into<StoreConfig>, &Config)` | The standard way: a built-in store. |
| `Database::open_memory(&Genesis)` | A fresh in-memory database, for tests. Takes no mode: every call starts empty. |
| `Database::from_store(store: S, &Config)` where `S: Store + Send + Sync + 'static` | A caller-supplied store, for custom or wrapped stores and tests. The store is trusted as supplied. |

MDBX rules for `open`:

- With `ExistingOnly`, a missing directory is `NotInitialized` and nothing is created. An
  existing empty directory is also `NotInitialized`, but MDBX has created its environment
  files in it by then.
- Open a directory at most once per process and share it with `clone`. A second process may
  open it: its branches are separate, and of two competing commits one gets `Conflict`.
- A store should back one open database at a time. Reopening it after the database is dropped
  is fine.

With the `internals` feature, the lower layer is public for trusted tooling:
`open_store(store, &Config) -> OpenResult<OpenedStore<S>>`, with `OpenedStore::info()`,
`genesis()`, `into_database()` and `into_store()`. A store returned by `into_store` bypasses
every record and reserved-record check.

### What genesis writes

Initialization writes, in one store transaction:

1. **Superblock rows:** `format` = 1 (`u32` BE), `hash_fn` (`u16` BE: 1 Keccak-256,
   2 Blake3), `roaring` = 1 (`u16` BE), `genesis_id` (32 bytes), and `head`.
2. **Genesis cells,** all of kind `field`:
   - for each reserved record (see [Reserved records](#reserved-records)): its `#key` cell
     (`bytes32`, the record's key) and its binding in `#recordKeys` (cell name = the raw
     32-byte record key, value `u64` = the record ID);
   - `#params`: `#maxCellNameLen`, `#maxStrLen`, `#maxBytesLen` (`u32` each), `#keyMode`
     (`u32`: 0 caller-assigned, 1 generated), and `#keySeed` (`bytes32`, generated mode only);
   - `#alloc`: `#nextRecordID` = 64 (`u64`).
3. **The cell trie** over those cells. `head = (0, StateRoot, IndexRoot)` with
   `IndexRoot = H("")`, the empty root, since no genesis cell is indexed.

`#roots` and `#rootIndex` hold only their identity cells. The two metering records hold only
their identities and bindings.

**Genesis identity:**

```
genesis_id = H( "golemdb/genesis/v1" ‖ 0x00
              ‖ format: u32 ‖ hash_id: u16 ‖ roaring: u16
              ‖ cell count: u32
              ‖ for each genesis cell in key order:
                    len(key): u32 ‖ key ‖ len(value): u32 ‖ value )
```

All integers are big-endian. `key` is `recordID: u64 ‖ name`; `value` is the encoded cell
(type tag ‖ payload). YAML formatting and field order do not affect the identity.

### Reopening

Reopening never writes. It checks:

- the `format`, `hash_fn` and `roaring` rows (`UnsupportedFormat`, `UnsupportedHash`,
  `UnsupportedRoaring`);
- `hash_fn` and `genesis_id` against the supplied genesis (`GenesisMismatch`);
- both roots reopen;
- the allocator: a field `u64` ≥ 64; exactly 64 at commit 0; no cells under the next ID;
- every genesis cell is present with its expected value;
- at commit 0: no other cells, and the empty index root;
- at a later commit: the `#roots` cell of the previous commit exists, as a 64-byte field;
- the required cells verify against their committed trie paths.

Failures are `CorruptState`. This is startup validation, not an audit of every user row.

### Opening errors

`OpenError` (non-exhaustive) is separate from `ApiError`:

| Variant | Meaning |
| --- | --- |
| `Io` | reading a configuration file or path failed |
| `Yaml`, `StoreYaml` | invalid genesis or store YAML |
| `InvalidConfig(String)` | settings do not fit the store, or MDBX selected without the feature |
| `NotInitialized`, `AlreadyInitialized` | see the open-mode table |
| `GenesisMismatch` | the supplied genesis differs from the stored one |
| `CorruptState(&str)` | incomplete or inconsistent stored state |
| `UnsupportedFormat(u32)`, `UnsupportedHash(u16)`, `UnsupportedRoaring(u16)` | unknown format identifiers |
| `Storage`, `Cells`, `Branch`, `Index` | lower-layer failures, as sources |

---

## Data model

### Records

A record is its key, zero or more user cells, and two cells the database maintains:

| Cell | Written by | Content |
| --- | --- | --- |
| user cells | the caller, through `create` and `patch` | typed values |
| `#key` | the database | the record key, `bytes32` field |
| `#meta` | the database | four counts over the user cells ([`#meta`](#meta)) |
| binding in `#recordKeys` | the database | record key → record ID, `u64` field |

So `create` writes n + 3 cells and `delete` removes n + 3. **Empty records are legal:** a
create may name no cells and a patch may remove the last one. Only `delete` removes a record.

Records are addressed internally by a dense `u64` ID, allocated from `#nextRecordID` and never
reused; a re-created key gets a new ID. The ID is not exposed by the API.

```rust
pub struct RecordKey(pub [u8; 32]);          // From<[u8; 32]>
pub struct Record { pub key: RecordKey, pub cells: BTreeMap<CellName, CellValue> }
// Record::meta() -> Option<RecordMeta>: decodes #meta if the read included it.
```

`Record::cells` is ordered bytewise by name.

### Record keys

The key mode is fixed in genesis (`record_keys`) for the database's whole life:

| Mode | `create` | Key |
| --- | --- | --- |
| `CallerAssigned` | must name the key | the caller's; an existing key is `AlreadyExists` |
| `Generated { seed }` | must not name a key | `H("golemdb/record-key/v1" ‖ seed ‖ id)` |

- `H` is the deployment's hash; `id` is the new record's ID as `u64` BE, that is
  `#nextRecordID` before it is incremented.
- Generated keys are deterministic and unique within the database, and differ between
  deployments with different seeds. The seed is not secret: it is a readable `#params` cell.
- **The modes are exclusive** because generated keys are predictable: a caller-assigned
  create could otherwise claim a future generated key. With exclusive modes, a generated
  create never fails with `AlreadyExists`.
- A create that does not match the mode fails with `KeyModeMismatch`, before anything is
  written or allocated.
- A caller-assigned key equal to a reserved record's key is `Reserved`.

### Cell names

User names follow this ASCII grammar, case-sensitive:

```
name  = ["$"] first *rest        ; 1 .. #maxCellNameLen bytes, "$" included
first = ALPHA
rest  = ALPHA / DIGIT / "_" / "-" / "." / ":"
```

`#` and `@` start reserved names; a user name can start with neither. `$` is ordinary user
syntax. Names contain no `0x00`, so the index key's separator is unambiguous. A reserved
record's cells may have raw binary names, such as a commit number in `#roots`.

### Cell types and kinds

A cell's value is a **type tag** byte followed by the payload. The tag's low 7 bits are the
type ID; bit `0x80` is set for an attribute (indexed) and clear for a field. Tag 0 is never a
valid value.

| ID | Type | Payload |
| --- | --- | --- |
| 1 | `bool` | 1 byte, `00` / `01` |
| 2 | `str` | UTF-8, at most `#maxStrLen` bytes |
| 3 | `bytes` | as-is, at most `#maxBytesLen` bytes; **field only** |
| 4 | `bytes20` | 20 bytes |
| 8–11 | `bytes4`, `bytes8`, `bytes16`, `bytes32` | 4 · 2^w bytes |
| 12–15 | `u32` … `u256` | big-endian |
| 16–19 | `i32` … `i256` | big-endian, sign bit flipped |
| 20–23 | `dec32` … `dec256` | as `i`, at scale 4, 6, 18, 18 |
| 24, 25 | `f32`, `f64` | IEEE total order |
| 28 | `date32` | days since the Unix epoch, sign bit flipped |
| 29 | `timestamp64` | microseconds since the Unix epoch, sign bit flipped |

All other IDs up to 127 are reserved. Fixed-width types have no configurable limit.

**Kind** is chosen per write, not per name: `attribute` or `field`. Two records may give the
same name different types and kinds.

### Rust values

Builders take any `impl IntoCellValue`; the Rust type decides the cell type:

| Rust type | Cell type |
| --- | --- |
| `bool` | `bool` |
| `i32`, `i64`, `i128` | `i32`, `i64`, `i128` |
| `u32`, `u64`, `u128` | `u32`, `u64`, `u128` |
| `f32`, `f64` | `f32`, `f64`; NaN fails |
| `&str`, `String`, `&String` | `str` |
| `&[u8]`, `Vec<u8>` | `bytes` |
| `[u8; 4]`, `[u8; 8]`, `[u8; 16]`, `[u8; 20]`, `[u8; 32]` | `bytes4`, `bytes8`, `bytes16`, `bytes20`, `bytes32` |
| `CellValue` | its own type; the builder sets its kind |

An unsuffixed integer literal is `i32`, so write `50i64` or `50u64` for other widths. Types
without a Rust conversion (`u256`, `dec…`, `date32`, …) are built as `CellValue`.

### Reserved records

Records 0–63 are reserved; user records start at 64. Each reserved record's key is its name,
ASCII, zero-padded to 32 bytes. The implemented catalogue:

| ID | Record | Holds today |
| --- | --- | --- |
| 0 | `#params` | `#maxCellNameLen`, `#maxStrLen`, `#maxBytesLen`, `#keyMode`, `#keySeed` |
| 1 | `#alloc` | `#nextRecordID` |
| 2 | `#roots` | one cell per earlier commit: name = `commitNr` (`u64` BE), value = `StateRoot ‖ IndexRoot` (64-byte `bytes` field), written lag-one by each commit |
| 3 | `#recordKeys` | bindings: name = raw record key, value = record ID (`u64`) |
| 4 | `#rootIndex` | identity only |
| 32 | `@meteringModel` | identity only |
| 33 | `@modelWeight` | identity only |

`create`, `patch` and `delete` on a reserved key return `Reserved`. `get` works on reserved
records like on any other; a full read returns all their cells.

### `#meta`

```rust
pub struct RecordMeta { pub cells: u64, pub cell_bytes: u64, pub indexed_cells: u64, pub index_bytes: u64 }
```

| Field | Counts, over user cells only |
| --- | --- |
| `cells` | user cells |
| `cell_bytes` | Σ `(8 + |name|) + (1 + |value|)`: cell key plus encoded value |
| `indexed_cells` | user cells of kind attribute |
| `index_bytes` | Σ over attributes of `|name| + 2 + |value|`: the index term key |

`|value|` is the payload without the tag. **Encoding:** the four fields in this order, each
`u64` BE, 32 bytes, stored as a `bytes32` field. It is under the state root, so it is
normative.

- `create` writes it; `patch` writes it only when a count changes; `delete` removes it.
  `#meta` and the receipt's details come from the same computed effects.
- **Reading:** a full `get` returns `#meta`; so does `RecordOp::get(key).only(["#meta"])`.
  `Record::meta()` decodes it, returning `None` if the read did not include it.
- A user record without `#meta`, or a count that would fall below zero, is reported as
  corrupt state (`Internal`), never repaired.
- **Completeness.** A client with a trusted root that receives `cells + 2` distinct cells of
  a record (user cells, `#key`, `#meta`), each with a valid inclusion proof, has the whole
  record. This needs every mutation to keep the counts exact. It does not cover projections,
  withheld records or query results. Proofs themselves are not implemented yet.

---

## Record operations

### `RecordOp`

One builder type for the four record calls. Its type parameter names the operation and
decides which methods exist.

```rust
pub struct RecordOp<Op> { … }             // Clone + PartialEq + Eq + Debug, #[must_use]
pub mod op { pub enum Create {} pub enum Patch {} pub enum Get {} pub enum Delete {} }
```

| Constructor | Methods | Notes |
| --- | --- | --- |
| `RecordOp::create()` | `.key(k)`, `.attribute(n, v)`, `.field(n, v)` | key per key mode |
| `RecordOp::patch(k)` | `.attribute(n, v)`, `.field(n, v)`, `.remove(n)` | |
| `RecordOp::get(k)` | `.only(names)` | default: all cells |
| `RecordOp::delete(k)` | – | |
| all | `.budget(max_cost: u64)` | stored, not enforced |

Inspection, for mocks: `record_key() -> Option<RecordKey>` (always `Some` except a create
without `.key`), `max_cost() -> Option<u64>`, `value(name) -> Option<&CellValue>` (create,
patch), `removes(name) -> bool` (patch), and `validate() -> Result<(), ApiError>`.

**Compile-time rules:** only a create has `.key`; only a patch has `.remove`; only a get has
`.only`; gets and deletes have no cell writes. Violations do not compile.

**Builder steps never fail.** The first problem is kept and reported by the call as
`InvalidArgument`, before the branch is touched. `validate()` reports it early. Problems:

| Problem | Message |
| --- | --- |
| a name violating the grammar | `attribute("x y"): invalid cell name`, cause as source |
| a value that cannot convert (NaN) or cannot take the kind (`bytes` as attribute) | `field("f"): invalid cell value`, cause as source |
| a name written or removed twice in one operation | `remove("x"): the operation already writes or removes this cell` |
| `.key` given twice | `key(): the record key is given twice` |

The builder checks the name grammar only, not `#maxCellNameLen` or the value limits; the call
checks those. `.only` takes names as raw bytes (`&str` or `&[u8]`), so reserved names such as
`#meta` can be projected; duplicates collapse.

### `create`

`create(branch, RecordOp::create()…) -> Metered<RecordKey>`

Order of checks; the first failure is returned and nothing is written:

1. Builder error → `InvalidArgument`.
2. Caller-assigned key equal to a reserved key → `Reserved`.
3. Branch admission: unknown or consumed handle, or stale origin → `HandleInvalid`; sealed →
   `Sealed`.
4. Key mode → `KeyModeMismatch`.
5. Each name against `#maxCellNameLen` and each `str` / `bytes` value against its limit, in
   name order → `InvalidArgument`.
6. Allocate the ID from `#nextRecordID`; derive the key in generated mode.
7. Key already bound → `AlreadyExists` in caller-assigned mode; corrupt state in generated
   mode.

On success it writes the user cells, `#key`, `#meta`, the binding and the advanced allocator
as one atomic branch operation, and returns the key.

### `get`

`get(target, RecordOp::get(key)…) -> Metered<Record>`

```rust
pub enum ReadTarget { Head, Branch(BranchId), Commit(CommitId) }
```

- `Head`: resolves the head and reads the record in one store snapshot.
- `Branch(id)`: work in progress, including every successful uncommitted mutation. A sealed
  branch stays readable and shows exactly the sealed state, except that `#roots` does not show
  the new root until commit.
- `Commit(c)`: only the current head is supported. Any other commit is
  `CommitUnavailable { requested, head }`.

A full read returns every cell, `#key` and `#meta` included. With `.only(names)`, missing
cells are omitted, and an empty list returns no cells but still checks that the record exists.
A missing record is `NotFound`. For committed reads, the binding and `#key` must agree, or the
read reports corrupt state.

### `patch`

`patch(branch, RecordOp::patch(key)…) -> Metered<()>`

Order of checks:

1. Builder error → `InvalidArgument`.
2. Reserved key → `Reserved`.
3. Branch admission → `HandleInvalid` / `Sealed`.
4. Key not bound → `NotFound`.
5. Names and values against the limits → `InvalidArgument`.

Then, per named cell, in name order:

| Current cell | Change | Effect |
| --- | --- | --- |
| missing | set | create |
| present, equal to the new value (type, kind and payload) | set | **no-op**: nothing written or counted |
| present, different | set | update |
| present | remove | delete |
| missing | remove | no-op |

An empty patch is valid on an existing record. Removing the last user cell leaves an empty
record. `#meta` is rewritten only if its counts change. The whole patch is one atomic branch
operation.

### `delete`

`delete(branch, RecordOp::delete(key)) -> Metered<()>`

Order of checks: reserved key → `Reserved`; branch admission; key not bound → `NotFound`.
It then removes every live cell of the record, `#key` and `#meta` included, and the binding.
The allocator does not move back; re-creating the key gives a new ID.

---

## Receipts

Every record call returns `Metered<T>`: its result together with a receipt. The receipt is
present on success and on failure, so a host can charge failed calls.

```rust
#[must_use]
pub struct Metered<T> { pub result: Result<T, ApiError>, pub receipt: Receipt }
// Metered::new(result, receipt); Metered::unmetered(result, priced_at) for mocks;
// into_result() drops the receipt.

#[non_exhaustive]
pub struct Receipt { pub cost: u64, pub priced_at: Option<CommitId>, pub details: Details }

#[non_exhaustive]
pub struct Details {
    pub cells_created: u64, pub cells_updated: u64, pub cells_deleted: u64,
    pub index_joins: u64, pub index_leaves: u64,
    pub cell_bytes_written: u64, pub cell_bytes_deleted: u64,
    pub index_bytes_written: u64, pub index_bytes_deleted: u64,
}
```

Rust's `?` works only on `Result`, so callers write `.into_result()?` or `.result?`.

**`cost`** is always 0 until metering is implemented.

**`priced_at`** is the commit whose cost schedule prices the call:

| Call | `priced_at` |
| --- | --- |
| write, or `get` on a branch | the branch's origin commit, looked up without side effects |
| `get` on `Head` or `Commit`, success | the head of the snapshot read |
| `get` on `Commit(c)`, failure | `c`; to change to the head ([Agreed changes](#agreed-changes-not-yet-implemented)) |
| unknown or consumed handle; failed head read | `None` |

Looking up the origin for a receipt never invalidates a branch, and an origin never changes,
so receipts cannot race with commits.

**`details`** are the call's effects on the record's user cells, counted as in metering D4.
System cells (`#key`, `#meta`, the binding) are not counted:

| Effect | Counts |
| --- | --- |
| create a cell | `cells_created`; `cell_bytes_written` += its size; if an attribute, `index_joins` and `index_bytes_written` |
| update a cell | `cells_updated`; old size to `cell_bytes_deleted`, new size to `cell_bytes_written`; leave for an old attribute, join for a new one |
| delete a cell | `cells_deleted`; `cell_bytes_deleted`; if an attribute, `index_leaves` and `index_bytes_deleted` |

Sizes are as for `#meta`. Effects, not requests: a no-op counts nothing. Failed calls and
reads report all zeros. Details are always present; metering R9 allows a transport to make
them opt-in. "Index terms created" is not reported yet: it needs the index-term reads that
come with metering (metering D2 step 4).

**Budget.** `RecordOp::budget(n)` is accepted and stored but not enforced. `OutOfBudget`
exists and is never returned yet; when it is, the amount spent is the receipt's `cost`.

---

## Branches

```rust
pub type CommitId = u64;   // 0 = genesis, +1 per commit
pub type BranchId = u64;   // process-local, monotonic, never reused, never persisted
pub struct BranchInfo { pub commit_id: CommitId, pub branch_id: BranchId, pub version: u64, pub sealed: bool }
pub struct SealInfo { pub commit_id: CommitId, pub state_root: [u8; 32], pub index_root: [u8; 32] }
```

A branch is an in-memory overlay over the head it was opened on, its **origin**. Every branch
call first validates the handle against the current head, in the same store snapshot it reads
from.

| Call | Behaviour |
| --- | --- |
| `head()` | the current committed head |
| `begin()` | a new branch over the current head, with one open frame |
| `branch_info(b)` | `commit_id` = origin; `version` = number of retained undo entries, not a revision counter (rollback lowers it, an identical write adds none); `sealed` |
| `checkpoint(b)` | ends the current frame and opens a new one |
| `rollback(b)` | restores the start of the current frame, in memory, without reading the store |
| `seal(b)` | computes both roots and freezes the branch; publishes nothing |
| `commit(b)` | seals if needed, then persists atomically and advances the head; consumes the branch |
| `discard(b)` | drops the branch without replay; consumes it |

**Rollback.** A rollback restores the current frame's starting state and keeps its marker.
Another rollback without an intervening write or checkpoint steps back to the preceding frame.
Fresh empty frames are not skipped. Once the first frame has been rolled back, a further
rollback is `NoFrameToRollback`; a write or checkpoint makes the last marker usable again.
Work is proportional to the undo entries replayed.

**Seal.** Sealing reopens both roots from the origin, adds the lag-one `#roots` cell for the
origin commit, applies the cell diff, derives index postings from the before and after values,
and computes `StateRoot` and `IndexRoot`, without opening a store writer. The `commit_id` it
returns is provisional, since another branch may still win. A repeated seal returns the cached
result after checking the head again. A failed seal leaves the branch open and unchanged.

A sealed branch rejects writes, `checkpoint` and `rollback` with `Sealed`. `get`,
`branch_info`, `commit` and `discard` remain available.

**Commit.** One store writer; the head is checked again inside it before any row is written;
then all buffered rows and the new head are written atomically. On success the branch is
consumed, and every other branch over the old head becomes stale. A reader holding a committed
snapshot keeps seeing it. A storage failure after sealing keeps the sealed result for a retry
or discard. A full store is `StoreFull`; the head stays unchanged.

**Stale branches.** A branch whose origin is no longer the head is stale. The first call that
detects it removes the branch: `commit` returns `Conflict`, any other call `HandleInvalid`.
After that, every call returns `HandleInvalid`. So a stale branch reports `Conflict` only if
`commit` is its first call after losing the race.

History and change-sets are not written yet: commits persist current state and roots only.

---

## Immutable data

The interface exists; the storage does not. **Every immutable-data call returns
`ApiError::NotImplemented { operation }` immediately**, including calls with invalid handles
or unknown segments. Nothing is validated, staged, allocated, read or written.

```rust
pub type ImmutableDataOrdinal = u64;          // dense per segment; provisional until commit
pub type ImmutableDataRow = Vec<Vec<u8>>;     // one opaque byte array per column
pub struct ImmutableDataKey(pub [u8; 32]);    // optional, unique within a segment
pub enum ImmutableDataAddress { Ordinal(ImmutableDataOrdinal), Key(ImmutableDataKey) }
```

Intended contract, as documented on the trait:

- `immutable_data_append(branch, segment, key, row)` stages a row on a **sealed** branch and
  returns a provisional ordinal. A key, if given, must be unique within the segment; a
  duplicate never overwrites a row. Row and key binding become visible together at commit.
- `immutable_data_get(segment, address)` reads a committed row by ordinal or by key.
- `immutable_data_range_of(segment, commit)` returns the half-open ordinal range the commit
  appended.
- `immutable_data_rows_of(segment, commit)` returns that commit's rows in append order.

Row keys are an addition to design §11, which has ordinal addressing only. They are not priced
in metering D7. These calls return `Result<T>` today; their target signatures, with a budget
and a receipt, are in [Agreed changes](#agreed-changes-not-yet-implemented).

---

## Errors

`ApiError` is non-exhaustive. Common failures are matchable variants; invalid input and
internal failures keep their causes as sources.

| Variant | Raised when |
| --- | --- |
| `NotImplemented { operation }` | an immutable-data call |
| `NotFound` | the record key is not bound |
| `AlreadyExists` | a caller-assigned create names a bound key |
| `KeyModeMismatch` | a create's key does not match the key mode |
| `OutOfBudget` | not returned yet |
| `Reserved` | create, patch or delete addresses a reserved key |
| `InvalidArgument { message, source }` | a builder problem, or a name or value over a genesis limit |
| `HandleInvalid` | an unknown, consumed or stale branch handle (except `Conflict` below) |
| `Conflict` | `commit` of a branch whose origin lost the race |
| `Sealed` | a write, checkpoint or rollback on a sealed branch; the handle stays valid |
| `NoFrameToRollback` | rollback with every frame already rolled back |
| `CommitUnavailable { requested, head }` | `get` on a commit other than the head |
| `StoreFull` | the store reached its size cap; the commit wrote nothing. Environmental, not deterministic: it must never become part of a result other nodes see |
| `Internal { source }` | corrupt state or another lower-layer failure |

---

## Agreed changes, not yet implemented

Decided against the metering spec; the code on `feat/record-ops` does not reflect them yet.

**Explicit budgets.** Every metered call carries a budget, and there is no default:

```rust
pub enum Budget { Limited(u64), Unlimited }
```

`Unlimited` must be stated, never implied by omission; it is for host-authorized estimation
(metering D8). How a `RecordOp` receives its budget, for example as a constructor argument, is
open. Today `.budget(n)` is optional and unenforced.

**`priced_at` from the head.** A committed read prices against the head's schedule, also when
it fails: `get(Commit(c))` failing with `CommitUnavailable { requested, head }` reports
`priced_at = head`, not `c` (metering D9).

**Bad handles cost 0.** A call on an unknown, consumed, stale or sealed handle is rejected
before admission and charged nothing (metering D8). Today's receipts already report cost 0.

**No-op means same type, kind and value.** As implemented; metering D2 now says the same.

**Immutable-data target signatures.** Budgeted, and returning a receipt like the record calls
(metering D7):

```rust
fn immutable_data_append(&self, branch: BranchId, segment: &str,
    key: Option<ImmutableDataKey>, row: ImmutableDataRow, budget: Budget)
    -> Metered<ImmutableDataOrdinal>;
fn immutable_data_get(&self, segment: &str, address: ImmutableDataAddress, budget: Budget)
    -> Metered<ImmutableDataRow>;
fn immutable_data_range_of(&self, segment: &str, commit: CommitId, budget: Budget)
    -> Metered<Range<ImmutableDataOrdinal>>;
fn immutable_data_rows_of(&self, segment: &str, commit: CommitId, budget: Budget)
    -> Metered<Vec<ImmutableDataRow>>;
```

Whether row keys stay (`key`, `ImmutableDataAddress::Key`) is still open: they are not in
design §11 or metering D7.

Still under discussion: the order of checks within a call, and the shape of `OutOfBudget`
(`spent`, `required`).

---

## Not implemented

Specified elsewhere, not built:

- Metering: real costs, budget enforcement, `OutOfBudget` with spent cost, the cost ledger,
  the metering admin API.
- History and change-sets: historical `get`, `begin(at?)`, `rewind`.
- `branch_hash`, `roots(at?)`, `params()`.
- `query` and `count`.
- Proofs, including `id_of` / `key_of`.
- Immutable-data storage.
- Per-record caps (`#maxRecordCells`, `#maxRecordIndexedCells`) and the global counters
  `#liveCells` / `#indexTerms` in `#alloc` (design §4, metering D3 and D5). Both change the
  genesis identity when added.
- Transport serialization: adapters own it.

---

## Differences from v1

### Why v2 differs

Four ideas run through the differences below:

1. **The call site states its intent, and the compiler checks it.** A create cannot remove a
   cell, a get cannot write one, and every write says whether it touches the index. Mistakes
   that v1 would report at run time, or not at all, do not compile.
2. **Every outcome is ready for metering, before metering exists.** A host such as Arkiv must
   charge failed calls too: a transaction keeps its fee through `OutOfBudget`. So each record
   call returns its receipt on success and failure alike, as EVM implementations return gas
   used with every outcome. Adding real costs later changes values, not signatures.
3. **What every node must agree on is fixed in genesis.** Settings that affect state, such as
   the key mode, live in committed `#params` cells and in the genesis identity. Two nodes with
   different settings fail to open the same database instead of silently diverging.
4. **The API stays small until a use case needs more.** Capabilities come with the feature
   that needs them: record IDs with proofs, version guards with a decision on record
   versioning. What `get` already provides needs no second call.

### Differences, with their reasons

| Topic | v1 (golem-db-api.md) | v2 (implemented) | Why |
| --- | --- | --- | --- |
| Call shape | `create(branch, key?, cells, budget, debug?)` etc. | one `RecordOp<Op>` argument per call, returning `Metered<T>` | Ideas 1 and 2. An operation is also a plain value: it can be cloned to retry after `Conflict` and compared in a mock. |
| Key modes | `CallerAssigned` / `EngineAssigned`, per branch lineage | `CallerAssigned` / `Generated { seed }`, fixed in genesis, with a specified derivation | Idea 3. Generated keys are predictable, so mixing modes would let a caller claim a future generated key; one mode per database rules that out. A specified derivation makes generated keys reproducible on every node. |
| Reading `#meta` | dedicated `meta` accessor | `get`, full or `.only(["#meta"])`, decoded by `Record::meta()` | Idea 4. A completeness proof needs `#meta` in the same read as the cells, so `get` must return it anyway; a projection on `#meta` is already a fixed-size point read. |
| `id_of`, `key_of` | record accessors | not built; to come with proofs | Idea 4. Exposing record IDs makes them part of the contract. Their only consumer is proof verification, whose cell paths are derived from the ID. |
| Receipt details | opt-in (`debug?`) | always present | Idea 2. In process they are a small struct computed anyway: the same counting path maintains `#meta`. Arkiv prices storage lifetime from the bytes written and deleted. Opt-in remains for transports, where receipt size matters (metering R9). |
| Writes to a sealed branch | `HandleInvalid` | `Sealed`; the handle stays valid | A host must know that it can still commit or discard the branch. `HandleInvalid` would tell it the handle is gone. |
| Rollback | not idempotent, multi-frame | multi-frame, with `NoFrameToRollback` when exhausted | A rollback past the first frame is reported, not ignored, so a host's batch bookkeeping cannot silently drift from the branch. |
| `branch_info` | `{origin, frame_depth}` | `{commit_id, branch_id, version, sealed}` | Reports what the branch layer tracks today; `commit_id` is v1's `origin`. Not a deliberate design change; to be aligned. |
| `expected_version?` | provisional OCC guard on patch and delete | not present | Idea 4. Record versioning is undecided in v1; the guard follows that decision. |
| Immutable data | ordinal addressing | adds optional per-segment row keys; not implemented | Lets a host look up a row by its own identifier, such as a transaction hash, without maintaining a separate index in cells. A proposal: it is not in design §11 and not priced in metering D7. |
| Opening | not covered | `Genesis`, `Config`, `StoreConfig`, constructors, genesis contents and identity | Idea 3. Every node must open the same deployment reproducibly, and a misconfigured node must fail at startup, not diverge later. Store settings stay node-local and outside the identity, so nodes can size their stores independently. |
| `StoreFull` | not in the error set | present | A full disk is a fact about one node, not about the state. Naming it lets a host stop committing instead of recording a failure other nodes would never see. |
