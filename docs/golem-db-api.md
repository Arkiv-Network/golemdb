# Golem DB — API

The Rust interface Golem DB presents to the layer above it (crate `golemdb-api`): **what a caller
can say and what comes back.** Its shape was worked out in an experimental implementation on
branch `matthiaszimmermann/feat/record-ops` (revision `cb323d8`), and the sections up to
[Errors](#errors) are precise enough to reproduce that experiment: signatures, rules, check order,
encodings and errors. [Specified, not yet implemented](#specified-not-yet-implemented) covers what
neither implementation builds yet, such as queries, proofs and the metering admin API.

The implementation that counts is `feature/golem-db-api`. It matches this spec in its lower layers
but not yet in the record-call surface and the stored format; [api-priorities.md](api-priorities.md)
lists the gaps and what to close first.

Status: draft, 2026-10-10. It replaces the earlier, language-neutral API spec (in git history
before this revision); [Changes from the previous spec](#changes-from-the-previous-spec) says what
changed and why.

Related documents:

- [golem-db-design.md](golem-db-design.md): data model, reserved records, commitment, branches.
  Cited as _design §n_.
- [golem-db-metering.md](golem-db-metering.md): costs, budgets, receipts. Cited as _metering Dn_.
  Metering is not implemented: every cost is 0 and budgets are not enforced.

## Contents

- [Glossary](#glossary) — [Words](#words) · [Names in the API](#names-in-the-api)
- [Shape of the API](#shape-of-the-api)
- [Opening a database](#opening-a-database) — [Genesis](#genesis) · [Config and open modes](#config-and-open-modes) · [Stores](#stores) · [Constructors](#constructors) · [What genesis writes](#what-genesis-writes) · [Reopening](#reopening) · [Opening errors](#opening-errors)
- [Data model](#data-model) — [Records](#records) · [Record keys](#record-keys) · [Cell names](#cell-names) · [Cell types and kinds](#cell-types-and-kinds) · [Rust values](#rust-values) · [Reserved records](#reserved-records) · [`#meta`](#meta)
- [Record operations](#record-operations) — [`RecordOp`](#recordop) · [Order of checks](#order-of-checks) · [`create`](#create) · [`get`](#get) · [`patch`](#patch) · [`delete`](#delete)
- [Receipts](#receipts)
- [Branches](#branches)
- [Immutable data](#immutable-data)
- [Errors](#errors)
- [Agreed changes, not yet implemented](#agreed-changes-not-yet-implemented)
- [Specified, not yet implemented](#specified-not-yet-implemented) — [Branch operations](#further-branch-operations) · [Query](#query) · [Cost guarantees and estimation](#cost-guarantees-and-estimation) · [Proofs](#proofs) · [Introspection](#introspection) · [Administration — the metering API](#administration--the-metering-api) · [Cell types: index classes and custom types](#cell-types-index-classes-and-custom-types) · [Further errors](#further-errors) · [Open items](#open-items)
- [Changes from the previous spec](#changes-from-the-previous-spec)

---

## Glossary

The names below were agreed during the implementation review (its decisions N1–N21) and are
applied in the experiment; `feature/golem-db-api` follows most of them, except `OpenConfig`
(for `Config`), `open_with_options`, and "backend" in places. Two ideas guide them:

- **One word, one meaning.** "Database" used to name both what a caller opens and the
  key/value layer underneath it, and "engine" meant five different things. Each concept now
  has its own word.
- **Follow established Rust and database conventions,** so a reader who knows sled, redb,
  RocksDB or reth finds familiar names. The crate path supplies context (`golemdb_api::Config`),
  so type names do not repeat the product name.

### Words

| Word | Means | Why this word |
| --- | --- | --- |
| **GolemDB** | the product, in prose | One word, like RocksDB, FoundationDB, SurrealDB. Pending product sign-off; this document still writes "Golem DB" until then. |
| **`golemdb`** | the product as an identifier: packages (`golemdb-api`, used as `golemdb_api`), domain tags (`"golemdb/genesis/v1"`), and the repo after its rename | Rust package names are lowercase; a one-word product gets a one-word prefix. |
| **database** | what a caller opens and talks to: the `Database` handle and everything behind it | It is what users of any embedded database call the thing they open. Reserving it for that frees "store" for the layer below. |
| **store** | the transactional key/value layer underneath: the `Store` trait | Matches the package `golemdb-storage`. Previously this layer was also called `Database`, so two different "databases" were public. |
| **store implementation** | one implementation of `Store`: memory (`MemoryStore`) or MDBX (`MdbxStore`) | Replaces "backend", which meant both the store a caller passes in and an implementation of the trait. |
| **reserved** | records 0–63 and the `#` / `@` names, as opposed to user records and names | Covers both system (`#`) and admin (`@`) records; "system" alone would exclude the admin records. Replaces "engine records" and "engine names". |
| **internal** | stored rows that are neither user cells nor reserved records: trie nodes, metadata | Names what the 16 KiB internal-value floor of opening is for. Replaces "engine rows". |
| **trusted library code** | code that bypasses the checks, such as a store taken from `OpenedStore::into_store()` | The design's own term (§4). Makes clear that bypassing checks is a privilege of code linked into the process, not of a caller. |
| **genesis file** | the deployment's YAML: identical on every node, never changes | Ethereum clients use the same word for the same thing. |
| **store file** | one node's YAML: store and tuning, may change between restarts | Kept apart from the genesis file because the two change on different timescales and only one is consensus-relevant. |

**"Engine" is not used** in this document or in the code. It had meant the database as callers
see it, reserved names, internal rows, trusted code, and MDBX itself; each now has a word above.

### Names in the API

| Name | What it is | Why this name |
| --- | --- | --- |
| `Database` | the cloneable handle a caller opens; implements `Api` | Precedents `redb::Database`, `sled::Db`: the type does not repeat the product name. Was `GolemDb`. |
| `Api` | the trait with record and branch operations | Lets consumers depend on the contract (`Arc<dyn Api + Send + Sync>`) and mock it, not on `Database`. |
| `Inner` (private) | the shared state behind a `Database`'s clones | The idiomatic Rust name for state behind a cheap-to-clone handle (`std::thread::Thread`, `Arc`'s `ArcInner`), already used by `Branches` and `MemoryStore`. Was `Engine`, which overstated a thin forwarding adapter. |
| `Genesis` | the deployment's settings: hash, cell limits, key mode | Follows Ethereum clients' type for the parsed genesis file (`alloy_genesis::Genesis`). Was `GenesisConfig`. |
| `Genesis::DEV` | the development preset | A named preset, as reth names `MAINNET` and `DEV`. Deliberately no `Default`: genesis is consensus-relevant, and a default that changed between releases would split nodes silently. |
| `Config` | what opening needs on any store: `genesis` and `mode` | The crate path gives the context (`golemdb_api::Config`, like `sled::Config`). Was `OpenConfig`. |
| `OpenMode` | `CreateIfMissing`, `ExistingOnly`, `CreateNew` | Not just `Mode`: the spec also has a key mode and a paging mode. Non-exhaustive so a read-only mode can be added. |
| `StoreConfig` | which built-in store to open, with its node-local options | Names its scope: everything about the store, nothing about the deployment. |
| `Database::open`, `open_memory`, `from_store` | the three constructors | One standard constructor taking anything that converts into a `StoreConfig` (a path means MDBX); `open_memory` takes only a genesis, since a fresh store makes the mode meaningless; `from_store` says that the caller supplies the store. Replaced `open_database`, `open_with_options` and an alias. |
| `internals` feature: `open_store`, `OpenedStore` | the lower opening layer | Public only on request, so ordinary consumers see one way to open a database. |
| `RecordKeys`: `CallerAssigned`, `Generated { seed }` | the key mode, in `Genesis::record_keys` | Says who makes the key. `Generated` replaces the spec's `EngineAssigned` and matches what the README already called generated keys. |
| `RecordOp<op::Create \| Patch \| Get \| Delete>` | one operation on one record | One builder for all four calls; the operation marker decides which methods exist. The markers live in `op` so that generic names like `Get` stay out of the crate root. Replaced `RecordInput`, `PatchInput` and `Projection`. |
| `attribute(name, value)`, `field(name, value)` | write an indexed or a stored-only cell | The design's two cell kinds: a call site shows which writes touch the index. A patch's `set(name, value)` keeps the stored cell's kind instead, and fails with `CellNotFound` if the cell does not exist ([Agreed changes](#agreed-changes-not-yet-implemented)). |
| `Metered<T>`, `Receipt`, `Details` | a record call's outcome with its cost, pricing commit and effects | "Metered" says the outcome carries its metering; "receipt" is the term metering uses; "details" are metering D4's receipt details. |
| `RecordMeta`, `Record::meta()` | the decoded `#meta` counts | Mirrors the cell name `#meta`. |
| `ApiError::StoreFull` | the store reached its size cap | Names the store, not MDBX: it is a property of one node's store, and it must never become a result other nodes see. |
| `OpenError::StoreYaml` | an invalid store file | Pairs with `OpenError::Yaml` for the genesis file. |
| `ApiError::KeyModeMismatch` | a create whose key does not match the key mode | Says which rule was broken, rather than a generic `InvalidArgument`. |
| `StorageError::Implementation` | a failure inside a store implementation | Replaces `StorageError::Backend`, following "store implementation" above. |
| `CellNameRef::parse_user`, `parse_reserved`, `raw` | the three ways to make a cell name | Named after the namespace each accepts. `parse_reserved` replaces `parse_engine`. |

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
| `RecordOp::patch(k)` | `.attribute(n, v)`, `.field(n, v)`, `.remove(n)`; agreed: `.set(n, v)` | `set` keeps the stored kind ([Agreed changes](#agreed-changes-not-yet-implemented)) |
| `RecordOp::get(k)` | `.only(names)` | default: all cells |
| `RecordOp::delete(k)` | – | |
| all | `.budget(max_cost: u64)` | stored, not enforced |

Inspection, for mocks, adapters and policy wrappers that receive an operation:

| Accessor | On | Returns |
| --- | --- | --- |
| `record_key() -> Option<RecordKey>` | all | the key; `None` only for a create without `.key` |
| `max_cost() -> Option<u64>` | all | the budget given, if any |
| `validate() -> Result<(), ApiError>` | all | the first builder error, if any |
| `value(name) -> Option<&CellValue>` | create, patch | the value this operation writes to `name` |
| `removes(name) -> bool` | patch | whether this patch removes `name` |
| `cells()` | create | every cell to create, as `(&CellName, &CellValue)` in name order |
| `changes()` | patch | every change, as `(&CellName, Change)` in name order |
| `names() -> Option<&[CellName]>` | get | the projection: `None` for the full record, `Some([])` for an existence check |

```rust
pub enum Change<'a> {
    Write(&'a CellValue),   // attribute or field: the kind is the value's
    Set(&'a CellValue),     // set: keep the stored cell's kind (agreed, see Agreed changes)
    Remove,
}
```

`cells()` and `changes()` return an `ExactSizeIterator`. An operation holding a builder error
still lists its valid steps; `validate()` says whether the call will fail. The three listing
accessors come from `feature/golem-db-api`, where `changes()` yields `Option<&CellValue>`;
`Change` replaces that, since `Some` could not tell `set` from an explicit write.

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

### Order of checks

Every call checks in three stages, as metering D2 specifies; the first failure is returned:

| Stage | Checks | Cost |
| --- | --- | --- |
| 1. Handle | writes and branch reads: the branch handle is valid and, for writes, not sealed. Reads of `Head` or a commit select their snapshot instead | 0 |
| 2. Admission | builder errors, reserved keys, key mode, genesis limits; no record state is read | admission |
| 3. Record state | the binding, then `#meta`, cells and index terms; caps, once added | state reads |

So a bad operation on a dead branch reports `HandleInvalid` and costs nothing. Both the experiment
and `feature/golem-db-api` deviate today: they check reserved keys before the handle, and
`patch` checks the limits after `NotFound` ([Agreed changes](#agreed-changes-not-yet-implemented)).

### `create`

`create(branch, RecordOp::create()…) -> Metered<RecordKey>`

Order of checks ([three stages](#order-of-checks)); the first failure is returned and nothing
is written:

1. Handle: unknown or consumed handle, or stale origin → `HandleInvalid`; sealed → `Sealed`.
2. Admission:
   1. builder error → `InvalidArgument`;
   2. caller-assigned key equal to a reserved key → `Reserved`;
   3. key mode → `KeyModeMismatch`;
   4. each name against `#maxCellNameLen` and each `str` / `bytes` value against its limit, in
      name order → `InvalidArgument`.
3. Record state:
   1. allocate the ID from `#nextRecordID`; derive the key in generated mode;
   2. key already bound → `AlreadyExists` in caller-assigned mode; corrupt state in generated
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

Order of checks: the target first, at no cost (an invalid branch handle → `HandleInvalid`; an
unavailable commit → `CommitUnavailable`); then the binding (`NotFound`); then the cells.

A full read returns every cell, `#key` and `#meta` included. With `.only(names)`, missing
cells are omitted, and an empty list returns no cells but still checks that the record exists.
A missing record is `NotFound`. For committed reads, the binding and `#key` must agree, or the
read reports corrupt state.

### `patch`

`patch(branch, RecordOp::patch(key)…) -> Metered<()>`

Order of checks ([three stages](#order-of-checks)):

1. Handle → `HandleInvalid` / `Sealed`.
2. Admission: builder error → `InvalidArgument`; reserved key → `Reserved`; names and values
   against the limits → `InvalidArgument`.
3. Record state: key not bound → `NotFound`; then `#meta` and each named cell, in name order,
   as below.

Per named cell, in name order:

| Current cell | Change | Effect |
| --- | --- | --- |
| missing | `attribute` / `field` | create |
| present, equal to the new value (type, kind and payload) | `attribute` / `field` | **no-op**: nothing written or counted |
| present, different | `attribute` / `field` | update |
| present | remove | delete |
| missing | remove | no-op |
| present | `set` (agreed) | the new value with the stored cell's kind; a no-op if type and payload are also unchanged, an update otherwise |
| missing | `set` (agreed) | **`CellNotFound { name }`**; the whole patch fails and nothing is written |

An empty patch is valid on an existing record. Removing the last user cell leaves an empty
record. `#meta` is rewritten only if its counts change. The whole patch is one atomic branch
operation.

### `delete`

`delete(branch, RecordOp::delete(key)) -> Metered<()>`

Order of checks: handle → `HandleInvalid` / `Sealed`; reserved key → `Reserved`; key not bound
→ `NotFound`.
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
| `get` on `Head` | the head of the snapshot read |
| `get` on `Commit(c)`, success or failure | the head at admission, not `c`: reads of old data use current prices |
| unknown or consumed handle; failed head read | `None`: no snapshot was captured, and the call costs 0 |

The pricing schedule follows the head, never the data being read. A read of commit 20 admitted
at head 100 is priced by the schedule of head 100 and reports `priced_at: 100`, so a weight
change applies to reads of historical data from the next head on (metering D9, "Historical data
does not select historical prices").

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
exists and is never returned yet; when it is, the amount spent is the receipt's `cost`, and the
error carries `required` ([Agreed changes](#agreed-changes-not-yet-implemented)).

---

## Branches

```rust
pub struct CommitId(u64);  // 0 = genesis (CommitId::GENESIS), +1 per commit
pub struct BranchId(u64);  // process-local, monotonic, never reused, never persisted
// Both: Copy, Eq, Ord, Hash, Display; new(u64), get() -> u64, From<u64>, Into<u64>.
pub struct BranchInfo { pub commit_id: CommitId, pub branch_id: BranchId, pub version: u64, pub sealed: bool }
pub struct SealInfo { pub commit_id: CommitId, pub state_root: [u8; 32], pub index_root: [u8; 32] }
```

`CommitId` and `BranchId` are distinct types over a `u64`, so one cannot be passed where the
other is expected; the compiler rejects it. On a transport both are plain `u64` values.

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
  returns a provisional ordinal. A key, if given, enters the segment's key index at commit,
  together with the row. Keeping keys unique is the host's job: a repeated key points the
  index at the newest row, older rows stay readable by ordinal, and an append never fails
  because of its key (design §11, [Row keys](golem-db-design.md#row-keys)).
- `immutable_data_get(segment, address)` reads a committed row by ordinal or by key.
- `immutable_data_range_of(segment, commit)` returns the half-open ordinal range the commit
  appended.
- `immutable_data_rows_of(segment, commit)` returns that commit's rows in append order.

The key index is uncommitted and pruned with its shards; a lookup of a pruned row by key
returns `NotFound`. These calls return `Result<T>` today; their target signatures, with a budget
and a receipt, are in [Agreed changes](#agreed-changes-not-yet-implemented).

---

## Errors

`ApiError` is non-exhaustive. Common failures are matchable variants; invalid input and
internal failures keep their causes as sources.

| Variant | Raised when |
| --- | --- |
| `NotImplemented { operation }` | an immutable-data call |
| `NotFound` | the record key is not bound |
| `CellNotFound { name }` (agreed) | a patch's `set` names a cell the record does not have |
| `AlreadyExists` | a caller-assigned create names a bound key |
| `KeyModeMismatch` | a create's key does not match the key mode |
| `OutOfBudget { required: Option<u64> }` (agreed shape) | the call's budget cannot cover its next step or its total; not returned yet |
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

Decided after the experiment, against the metering spec. Neither the experiment nor
`feature/golem-db-api` implements them yet.

**Explicit budgets.** Every metered call carries a budget, and there is no default:

```rust
pub enum Budget { Limited(u64), Unlimited }
```

`Unlimited` must be stated, never implied by omission; it is for host-authorized estimation
(metering D8). How a `RecordOp` receives its budget, for example as a constructor argument, is
open. Today `.budget(n)` is optional and unenforced.

**`priced_at` from the head.** A committed read prices against the head's schedule, also when
it fails ([Receipts](#receipts)). The experiment reports `priced_at = c` when `get(Commit(c))`
fails with `CommitUnavailable { requested, head }`; the fix is to use `head`, which the error
already carries. `feature/golem-db-api` has no receipts yet, so it gets this right when they are
added.

**Bad handles cost 0.** A call on an unknown, consumed, stale or sealed handle is rejected
before admission and charged nothing (metering D8). Today's receipts already report cost 0.

**No-op means same type, kind and value.** As in both implementations; metering D2 now says the same.

**`set` keeps the stored kind; no `insert`.** A patch's `set(name, value)` writes the new value
with the kind the stored cell already has, so changing a value never changes whether it is
indexed. If the record has no such cell, the patch fails with the new error
`CellNotFound { name }`, distinct from the record-level `NotFound`, and writes nothing. The
stored kind is known from the cell read that patch planning performs anyway (metering D2
step 4), so `set` costs no extra read. `create` has only `attribute` and `field`: a new record
has no stored kind to keep. `feature/golem-db-api` today has `insert` and `set` that keep the
*value's* kind, which the plain `CellValue` constructors set to field; those are replaced.

**Order of checks.** Handle first, then admission, then record state
([Order of checks](#order-of-checks), metering D2). Both implementations check reserved keys
before the handle, and `patch` validates names and values after resolving the key; both move.

**Builder steps never fail.** As described in [`RecordOp`](#recordop): the first invalid step is
kept and reported by the call, with a receipt, so input errors are charged as admission
(metering D2). `feature/golem-db-api` today returns `Result<Self>` from every step; that is
replaced.

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

Row keys stay, with newest-wins lookup and no uniqueness check (design §11,
[Row keys](golem-db-design.md#row-keys)). The doc comment on `feature/golem-db-api`'s
`immutable_data_append` says "unique segment-local key"; it should say that uniqueness is the
host's job.

**`OutOfBudget { required: Option<u64> }`.** The spent cost is in the receipt, as for every
outcome; the error adds only what a retry would need (metering D8):

| Outcome | `receipt.cost` | `required` |
| --- | --- | --- |
| stopped at a planning checkpoint, before the total is known | completed steps | `None` |
| planning completed, total above the budget | planning work done | `Some(total)` |
| cost arithmetic overflows `u64` | completed steps | `None` |
| a read runs out of budget | work done so far | `None`: reads stop as they go |

`required` holds for the inspected state only: a retry succeeds with it only if nothing changed in
between. Other failures, such as `InvalidArgument` or `NotFound`, carry no `required`; their
receipt states what they cost. Today `OutOfBudget` is a variant without fields.

---

## Specified, not yet implemented

Neither the experiment nor `feature/golem-db-api` builds the following yet. The subsections below
carry over the previous spec's contracts for them; their Rust signatures are not pinned.

- Metering: real costs, budget enforcement, the cost ledger, the metering admin API.
- History and change-sets: historical `get`, `begin(at?)`, `rewind`.
- `branch_hash`, `roots(at?)`, `params()`.
- `query` and `count`.
- Proofs, including `id_of` / `key_of`.
- Immutable-data storage, including the row-key index.
- Per-record caps (`#maxRecordCells`, `#maxRecordIndexedCells`) and the global counters
  `#liveCells` / `#indexTerms` in `#alloc` (design §4, metering D3 and D5).
- The remaining `#params` cells of design §4: `#minRetention`, `#shardSpan`,
  `#immutableDataSegments`.
- Transport serialization: adapters own it.

Every new genesis cell above changes the genesis identity when added.

### Further branch operations

| op | signature | semantics |
| --- | --- | --- |
| `begin` | `(at?: CommitId) → BranchId` | opens a branch at `at`, within retention; absent = head, as today |
| `branch_hash` | `(b) → B256` | digest of the branch's current state, computed on demand from the overlay |
| `rewind` | `(to: CommitId)` | **not in release 1.** Reserved for a host-restricted reorg within the retention window (design D02) |

- **The lineage never forks.** Competing candidates are branches until one commits. Without
  `rewind`, a commit cannot be undone: a host commits only blocks it will not reorg. Fork
  _choice_ is the host's job.
- **Reads on a branch are point reads only.** `query` and `count` target committed state; a
  branch cannot query its own uncommitted writes.
- **A commit is homogeneous:** either data operations or admin operations, never both.
- **`branch_hash`** answers the block-validation case: a node re-executing a proposed block needs
  the resulting root before it accepts the header. It is **not incremental and not O(1)**: the
  database merkleizes once at commit, so the call runs one pass over the branch's net touched
  set. `commit` may reuse the result. Whether repeated calls are metered is
  [open](#open-items). → _design [§10](golem-db-design.md#the-in-memory-overlay)_

### Query

Queries run against **committed state only**, never against a branch.

#### `query`: filtered, sorted, paged read

| Input | Meaning |
| --- | --- |
| `at?` | `CommitId`; absent = read the head. The choice also fixes page stability ([Paging](#paging)) |
| `query` | the query structure, below |
| `cursor?` | an opaque cursor from a previous page |
| `budget` | `Limited(u64)` or host-authorized `Unlimited` |

```
query = {
    projection? : [cell name]
    filter      : ordered DNF: OR of AND-groups, negated literals allowed
    sort?       : [ (name, type, direction) ]      ordered list of terms
    page        : { limit, offset? }
}
```

**Output:** the records (full or projected), `total_matched`, the commit the page was evaluated
at, a cursor for the next page, and a receipt.

#### Filtering

The `filter` is an **ordered** DNF, and the database never reorders it:

- **AND-order:** predicates intersect in submitted order, carrying a running intermediate; a group
  stops early when that intermediate is empty. This is the caller's main optimization lever.
- **OR-order:** groups evaluate in submitted order and union. The order is pinned so that the
  abort point, and so `OutOfBudget`, is deterministic.
- **Early exit applies to unsorted queries only.** A sorted query costs O(N) whatever page was
  asked for. An unsorted query may stop once `offset + limit` records are collected, unless
  `total_matched` is required.
- A record matching several groups is returned **once**.
- **Predicates are typed:** a cell of another type under the same name is treated as absent.
- `LimitExceeded` caps the group count, predicates per group and nesting.

**The database performs no statistics-based planning**: work is a pure function of
`(state, ordered filter, page)`, and query optimization is the caller's job. So the _outcome_
(results or `OutOfBudget`) depends on the order while the result set does not: ordering affects
_whether_ you get results, never _which_.

#### Sorting

Sorting orders the set the filter selected; it never changes which records are returned.
Multi-key ordering is native: `sort` is an ordered list of `(name, type, direction)` terms.
Creation order is a well-defined deterministic order. → _design [§12](golem-db-design.md#12-sorting)_

#### Paging

**`total_matched` is always available and always free.** On an unsorted query, requesting it
forfeits early exit.

A page begins where the caller says: a **cursor** walks, carrying the position of the record last
emitted; an **offset** jumps, indexing the sorted sequence directly.

- **A cursor's position is a value, not a count:** the last emitted record's sort values plus its
  record ID. Resuming takes the first element strictly greater than that key. The key is
  compared, never dereferenced, so a resume is well defined even if that record was since
  deleted or changed.
- **Exhaustion:** a cursor is exhausted when nothing is greater than its key, which is exact. An
  offset is exhausted when a page returns fewer than `limit`, which against a live query can
  fire early.

**Pinned and live**, chosen when the first page is requested; the cursor carries it through the
iteration:

| mode | request | behaviour |
| --- | --- | --- |
| **pinned** | names `at` | every page evaluated at that commit; one unchanging state |
| **live** | names no commit | each page resolves against the head at the time; the sequence drifts |

Either way, the response reports the commit it evaluated at.

| Mutation between pages | offset | cursor |
| --- | --- | --- |
| insert **before** the position | everything shifts right; one record returned twice | **clean**: the position is a value, not a count |
| delete **before** the position | everything shifts left; one record skipped | **clean** |
| a record's **sort value changes** across the boundary | duplicate or skip | duplicate or skip |
| any mutation **after** the position | none yet | none yet |

No page is ever internally wrong under either mechanism; only the sequence can be inconsistent.
**Sorting on an immutable cell makes live paging anomaly-free.** Pinning adds one read per
resolved value, roughly doubling a query's read count, and does not grow with age. Each page is
a new call for pricing: pinning the data does not pin the prices (metering D9).
→ _design [§13](golem-db-design.md#pinned-and-live)_

**The cursor** is opaque; the query is resubmitted alongside it.

```
cursor = { commit?, sortKey, recordId, fingerprint, nodeId? }
```

| Field | Contract |
| --- | --- |
| `commit` | present exactly when the iteration is pinned |
| `sortKey` | the last emitted record's sort values, one per term, with **absent** represented explicitly |
| `recordId` | the tie-break component of the position |
| `fingerprint` | attributes the cursor to a sequence; a mismatch is `InvalidQuery`, never a degraded mode. Covers at least everything that determines membership and order; whether it covers more is [open](#open-items) |
| `nodeId` | opaque database-assigned routing hint, carrying no durable machine identity; design D11 may remove it |

Two normative rules for implementations that keep a warm sequence:

- **Cache validity.** A held sequence may serve a request **if and only if the commit it was
  built at equals the commit the request resolves to.**
- **Cost is the canonical execution, warm or cold.** The receipt reports filter evaluation,
  sorting and materialization regardless of what was short-circuited.

→ _design [§13](golem-db-design.md#the-cursor-and-the-warm-node)_

#### `count`: count matches without materializing records

| Input | Meaning |
| --- | --- |
| `at?` | `CommitId`; absent = the current head; must lie within retention |
| `filter` | ordered DNF, as in `query` |
| `budget` | `Limited(u64)` or host-authorized `Unlimited` |

**Output:** a `u64` count and a receipt.

### Cost guarantees and estimation

Cost is **consensus-visible**: it decides `OutOfBudget`, which decides whether a call returns
results. [Metering](golem-db-metering.md) defines the cost model; a caller may rely on these
guarantees:

1. **Deterministic:** identical on every implementation, machine and version for the same
   operation against the same state.
2. **Operation-local and additive:** attributable to one call, summable across calls.
3. **Nothing physical is priced:** not pages, not disk bytes, not wall-clock time.
4. **Cost is the canonical execution:** caching, warm nodes and held sequences never reduce a
   charge.
5. **Monotone:** there are no refunds anywhere in the model.

**Estimation.** The same read-only planner serves execution and estimation (metering D2). An
estimate returns the full planned cost without applying the mutations, consuming IDs or changing
counters. A host-authorized `Unlimited` estimate still validates and uses checked arithmetic. An
estimate is valid only for the inspected state and schedule; a multi-operation simulation that
needs earlier writes visible to later ones runs in a disposable branch. Host resource controls
are in [metering D8](golem-db-metering.md#shared-planner-and-arkiv-usage-contexts).

**Deletion is not prepaid at creation.** A host whose records expire, where expiry rather than a
caller triggers the delete, prepays deletion in its own pricing, using the bound of metering D5.

### Proofs

Every committed state has a single 32-byte root; any cell or index term can be proved against it
to a party holding nothing but the root.

- **Any past commit is provable, not just the head:** a light client holding the current root can
  verify the root at a past commit, from `#roots`, and descend from it.
- **Proofs attest typed values,** since the type tag is inside the hashed value.
- **Whole-record completeness** follows from a proof of `#meta` ([`#meta`](#meta)).
- **`id_of` and `key_of`** come with proofs: a cell's trie path is derived from the record ID, so a
  verifier needs it. They are fixed-size point reads (metering D6).

The call's signature is not pinned: it needs a target commit and the item to prove (record key
and cell name, or an index term), and returns the node path plus the root it verifies against.
Non-inclusion proofs follow the same shape. → _design [§8](golem-db-design.md#8-state-commitment-and-global-root)_

### Introspection

Unmetered.

| op | signature | meaning |
| --- | --- | --- |
| `roots` | `(at?: CommitId) → {state_root, index_root}` | the committed roots as of a commit; absent = head |
| `params` | `() → map<name, value>` | the deployment's genesis parameters; equivalent to `get` on `#params` |

### Administration — the metering API

The **model** is code, identified by a `modelVersion`: the op-class taxonomy, counting rules,
byte-term definition and expected weight names. It changes with a new release. The **weights**
are data, one `u64` per named weight per model version, stored in the admin records
`@meteringModel` and `@modelWeight`, and are what this API writes.

| op | signature | meaning |
| --- | --- | --- |
| `install_model` | `(version, activation: CommitId, weights: map) → ()` | installs a future model version and its complete weight set, in one admin commit |
| `set_weight` | `(version, name, weight) → ()` | patches one weight |
| `metering_model` | `(at?: CommitId) → {active_version, pending?}` | the active model and any pending activation |
| `model_weights` | `(version?) → map<name, u64>` | the weights of a model version; absent = active |

Lifecycle rules; a violation is `InvalidArgument`:

1. **Install, then validate at activation.** `install_model` requires `version` > current,
   `activation` > head, and parseable cells. **Completeness is checked at the activation
   commit**, not at install, which keeps the upgrade window between the two.
2. **The active model takes immediate patches only.** `set_weight` on it takes effect at the
   next committed head, after the admin commit succeeds, never mid-branch. There is no
   scheduling for the current model; future work is staged under the pending version.
3. **Capture pricing once, never re-price in flight** ([Receipts](#receipts), metering D9).
4. **At most one pending model** at a time.

**Surface separation.** Opening returns a data handle and a separate admin handle. Admin
operations take no branch: each forms its own single-purpose commit, which makes commit
homogeneity structural. **Authorization is the host's:** the database validates the lifecycle
rules, not who may call the API.

> **Consensus note.** A weight change moves the boundary between success and `OutOfBudget`, so it
> affects outcomes in blockchain mode. Model activations must be coordinated by the chain
> specification.

→ _design [§4](golem-db-design.md#meteringmodel-recordid-32), [metering D9](golem-db-metering.md#d9-cost-schedules)_

### Cell types: index classes and custom types

- Each type has an **index class**: `none`, `eq`, `eq+range` or `eq+prefix`. A query predicate the
  class does not support is `InvalidQuery`. The classes are part of the type grid of
  _design [§3](golem-db-design.md#the-type-grid)_.
- Type IDs 1–63 are defined by this spec, append-only: a new core type is a spec release. IDs
  64–127 are **custom types**, registered as deployment configuration, never at run time. A
  registration specifies `id`, `name`, `width`, index class, codec and encoding, plus conformance
  vectors an implementation must reproduce byte for byte.

### Further errors

| Error | Raised when |
| --- | --- |
| `InvalidQuery` | a predicate the type's index class does not support, or a cursor whose fingerprint does not match |
| `LimitExceeded` | a filter exceeds the caps: group count, predicates per group, nesting |
| `Pruned` | an immutable-data ordinal existed but is beyond the retention window; a pruned key gives `NotFound` |

### Open items

| # | Item | Status |
| --- | --- | --- |
| 1 | **Record versioning and optimistic concurrency:** whether an `expected_version` guard on patch and delete exists | undecided |
| 2 | **Proof call signature:** target, item addressing, returned path shape, non-inclusion | not pinned |
| 3 | **Repeated `branch_hash` calls:** metered or not | open |
| 4 | **Cursor fingerprint:** whether it covers more than membership and order | open |

---

## Changes from the previous spec

### Why it changed

Four ideas run through the differences below:

1. **The call site states its intent, and the compiler checks it.** A create cannot remove a
   cell, a get cannot write one, and every write says whether it touches the index. Mistakes
   that the previous spec would report at run time, or not at all, do not compile.
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

| Topic | Previous spec | This spec | Why |
| --- | --- | --- | --- |
| Call shape | `create(branch, key?, cells, budget, debug?)` etc. | one `RecordOp<Op>` argument per call, returning `Metered<T>` | Ideas 1 and 2. An operation is also a plain value: it can be cloned to retry after `Conflict` and compared in a mock. |
| Key modes | `CallerAssigned` / `EngineAssigned`, per branch lineage | `CallerAssigned` / `Generated { seed }`, fixed in genesis, with a specified derivation | Idea 3. Generated keys are predictable, so mixing modes would let a caller claim a future generated key; one mode per database rules that out. A specified derivation makes generated keys reproducible on every node. |
| Reading `#meta` | dedicated `meta` accessor | `get`, full or `.only(["#meta"])`, decoded by `Record::meta()` | Idea 4. A completeness proof needs `#meta` in the same read as the cells, so `get` must return it anyway; a projection on `#meta` is already a fixed-size point read. |
| `id_of`, `key_of` | record accessors | not built; to come with proofs | Idea 4. Exposing record IDs makes them part of the contract. Their only consumer is proof verification, whose cell paths are derived from the ID. |
| Receipt details | opt-in (`debug?`) | always present | Idea 2. In process they are a small struct computed anyway: the same counting path maintains `#meta`. Arkiv prices storage lifetime from the bytes written and deleted. Opt-in remains for transports, where receipt size matters (metering R9). |
| Writes to a sealed branch | `HandleInvalid` | `Sealed`; the handle stays valid | A host must know that it can still commit or discard the branch. `HandleInvalid` would tell it the handle is gone. |
| Kind on write | per write, any setter | `attribute` / `field` declare it; patch's `set` keeps the stored kind and fails with `CellNotFound` on a missing cell; no `insert` | A value change should not silently change indexing; a missing cell under `set` is most likely a typo. |
| Commit and branch IDs | `u64` values | distinct newtypes over `u64` | Mixing up a commit number and a branch handle is an easy mistake with two `u64`s; separate types make it a compile error. Taken from `feature/golem-db-api`. |
| Operation inspection | not covered | `cells()`, `changes()`, `names()` besides the per-name accessors | Code that receives an operation, such as a mock, an adapter or a policy wrapper, must be able to list its contents, not only ask about a known name. Taken from `feature/golem-db-api`. |
| Rollback | not idempotent, multi-frame | multi-frame, with `NoFrameToRollback` when exhausted | A rollback past the first frame is reported, not ignored, so a host's batch bookkeeping cannot silently drift from the branch. |
| `branch_info` | `{origin, frame_depth}` | `{commit_id, branch_id, version, sealed}` | Reports what the branch layer tracks today; `commit_id` is the previous spec's `origin`. Not a deliberate design change; to be aligned. |
| `expected_version?` | provisional OCC guard on patch and delete | not present | Idea 4. Record versioning is undecided ([Open items](#open-items)); the guard follows that decision. |
| Immutable data | ordinal addressing | adds optional per-segment row keys, newest wins; not implemented | Lets a host such as Arkiv serve lookups by its own identifier, such as a transaction hash, without a separate index in cells. Uniqueness is left to the host, so appends never fail on keys and no outcome depends on retention. Now in design §11. |
| Opening | not covered | `Genesis`, `Config`, `StoreConfig`, constructors, genesis contents and identity | Idea 3. Every node must open the same deployment reproducibly, and a misconfigured node must fail at startup, not diverge later. Store settings stay node-local and outside the identity, so nodes can size their stores independently. |
| `StoreFull` | not in the error set | present | A full disk is a fact about one node, not about the state. Naming it lets a host stop committing instead of recording a failure other nodes would never see. |
