# GolemDB technical context

GolemDB is an embedded Rust database with typed record cells, reversible write
branches, an ordered attribute index, and deterministic Merkle commitments.
The storage schema and canonical encodings are part of the commitment format.
Application protocols, fee collection, and transport serialization belong to hosts
and adapters.

## Core architecture

### Crate boundaries

| Crate | Responsibility |
| --- | --- |
| `api` | Public `Api` trait, `Database` handle, typed CRUD requests, opening, genesis initialization and validation. |
| `record` | Record identity, key bindings, allocation, admission checks, and atomic CRUD over branch cell views. |
| `branch` | Branch registry, overlays, undo frames, guarded reads and writes, sealing, and atomic publication. |
| `cells` | Cell names, keys, types, canonical values, deployment limits, reserved-record constants, `Cell` storage and its commitment. |
| `index` | Ordered terms, bitmap containers, posting updates, index queries, and the two index commitment tiers. |
| `merkle` | Generic compressed hexary Patricia tries, branch codecs, traversal, immutable branch mutation, and hash providers. |
| `storage` | Transactional ordered KV traits, cursors, shared scans, and `MemoryStore`. |
| `storage-mdbx` | `MdbxStore`, native transactions and cursors, durability, and physical capacity settings. |
| `integration-tests` | Contracts spanning crates, backend persistence, fault injection, and integration benchmarks. |

`Database` delegates to an internal implementation parameterized by `S: Store`
and `H: HashProvider`. Record and branch operations share the same branch manager.
Cells and index use Merkle tries through caller-supplied storage transactions.
Merkle handles leaf references and branches; leaf payloads and hash domains belong
to cells and index. Storage interprets neither cell types nor trie nodes.

### Records and cells

- `RecordKey` is a caller-supplied 32-byte public identity. Internal record IDs are
  `u64`; user allocation starts at 64. IDs below 64 belong to the engine.
- A record's non-indexed `#key` cell contains its public key. The `#recordKeys`
  record maps raw 32-byte keys to internal IDs through non-indexed `u64` cells.
- Create stages the user cells, identity, binding, and allocator increment as one
  atomic branch operation. Delete removes every live cell and the binding, without
  reclaiming the ID. Recreating a deleted key allocates a new ID.
- Zero user cells is a valid record. Removing its last user cell preserves its
  identity. An empty patch and removing an absent cell are valid on an existing
  record. Identical changes do not advance its branch's undo-entry count.
- `CellKey` encodes `record_id:u64_be || cell_name`. `CellName` owns the name bytes;
  `CellNameRef` borrows them. Reserved records can use raw binary names.
- User names match `\$?[A-Za-z][A-Za-z0-9_.:-]*` and are case-sensitive. User
  mutations cannot write `#` or `@` names. Record CRUD rejects exact reserved
  record keys; a key's prefix alone does not make it reserved.
- A field is not indexed. An attribute is indexed. Patching a cell may change its
  type and kind. `CellValue` owns the canonical bytes; `CellValueRef` borrows them.
- `CellLimits` separates deployment admission policy from representation validity:
  maximum name, string, and variable-byte lengths come from genesis parameters.

### Reserved records

Reserved public keys are the ASCII names below, padded on the right to 32 bytes
with zeroes. Every listed record has its own `#key` and a binding in `#recordKeys`.

| ID | Record | Contents and role |
| --- | --- | --- |
| 0 | `#params` | `u32` fields `#maxCellNameLen`, `#maxStrLen`, `#maxBytesLen`. |
| 1 | `#alloc` | `u64` field `#nextRecordID`, initially 64. |
| 2 | `#roots` | Previous-commit roots, addressed by raw big-endian commit IDs. |
| 3 | `#recordKeys` | Public-key-to-record-ID bindings. |
| 4 | `#rootIndex` | Reserved identity for index-root-related schema. |
| 32 | `@meteringModel` | Reserved identity for metering models. |
| 33 | `@modelWeight` | Reserved identity for pricing weights. |

Genesis initializes the catalogue, identities, bindings, limits, and allocator.
The root and metering records start with identity cells only. Reserved cells are
fields and do not produce attribute index terms.

### Canonical cell and term encoding

```text
cell_value = type_tag:u8 || payload
type_tag   = (attribute ? 0x80 : 0x00) | type_id
index_term = cell_name || 0x00 || attribute_type_tag || payload
```

One storage row contains one complete cell. Fixed-width types require their exact
width. Strings and variable bytes consume the remaining row; no cell payload has
a length prefix. Type ID zero is an absent marker, not a cell type. Overlay
tombstones use `None` rather than a persisted zero-tagged cell.

| Value family | Canonical payload |
| --- | --- |
| Boolean | One byte, `00` or `01`. |
| String | Raw UTF-8, including valid embedded NUL characters. |
| Fixed bytes | Exact-width bytes; supported sizes include 4, 8, 16, 20, and 32. |
| Unsigned integer | Fixed-width big-endian bytes, widths 32 through 256 bits. |
| Signed integer, decimal, date, timestamp | Big-endian signed representation with the first byte XOR `0x80`. |
| Float | Sortable IEEE encoding: complement negative input bits, otherwise flip the sign bit. NaNs are rejected; construction normalizes negative zero to positive zero. |
| Variable bytes | Raw bytes; field kind only. |

Stored cells and index terms share the same ordered payload. Parsing stored bytes
is strict, including rejecting noncanonical float zero. `CellValue::from_*`
constructors accept natural values and perform encoding; they create fields.
Decimal constructors accept unscaled integers. Scales in `Width::decimal_scale`
are 4, 6, 18, and 18 for 32-, 64-, 128-, and 256-bit decimals. Dates use days and
timestamps microseconds since the Unix epoch.

`IndexTerm::from_cell` copies the already-encoded attribute payload without another
order transform; fields produce no term. `IndexTerm::new` instead accepts native
value bytes and applies the transform. Unsupported attribute types are errors.
Ordered comparisons and scans are meaningful within a single name/type prefix.

### Physical storage

| Table | Key | Value |
| --- | --- | --- |
| `Superblock` | Metadata name | Format identifiers, genesis identity, or head. |
| `Cell` | Encoded `CellKey` | Tagged canonical cell value. |
| `CellTrie` | Branch hash | Compact branch bytes for 32-byte paths. |
| `Index` | Ordered index term | Nonempty bitmap root hash. |
| `IndexTrie` | Branch hash | Compact branch bytes for 32-byte paths. |
| `BitmapTrie` | Branch hash | Compact branch bytes for 6-byte paths. |
| `BitmapContainer` | Container leaf hash | Six-byte path followed by canonical portable Roaring bytes. |

`Superblock/head` is `commit_id:u64_be || state_root:[u8;32] || index_root:[u8;32]`.
Other metadata keys are `format` (`u32_be`, value 1), `hash_fn` (`u16_be`, 1 for
Keccak-256 or 2 for BLAKE3), `roaring` (`u16_be`, format ID 1), and `genesis_id`
(32 bytes).

The `Store` contract supplies snapshot readers and one serialized writer. All
tables publish atomically. Writers read their own changes; dropping an uncommitted
writer aborts. Independent reader snapshots and cursors remain valid across writes
and commits. Nested writers on the same store are not supported.

Tables are created transactionally by their first `put` or `insert`. An absent
table reads as empty; reads and deletes do not create it. Table names are nonempty
and NUL-free. Keys use unsigned lexicographic byte ordering. `put` replaces;
`insert` preserves an existing row and returns `AlreadyExists`.

A cursor starts at a supplied key. Its first `next` returns the smallest key at
least that key; its first `prev` returns the largest key at most that key.
Subsequent movement is strict. Reversing after reaching an end returns the boundary
row. Shared bounded and prefix scans are lazy and end after their first error.

`MemoryStore` retains committed snapshots through shared state and copies state
for a writer. `MdbxStore` uses native named tables, snapshot-aware catalogue checks,
lazy cursors, durable synchronization, and copy-on-write pages. Clone an open MDBX
handle to share its environment; close all handles before independently reopening
the same path. Defaults are 128 table handles, a 1 GiB map ceiling, and 16 MiB growth.
Physical limits come from the backend. Map exhaustion maps to `StorageError::Full`
and `ApiError::StoreFull`.

### Hashing and Merkle commitments

`Hash` is 32 bytes. Genesis selects Keccak-256 (not SHA3-256) or standard unkeyed
BLAKE3. The API chooses a concrete provider once; inner hash calls use static
dispatch. The same provider supplies routing, leaf, and branch hashes.
`hash_parts` concatenates its arguments without implicit framing.

```text
cell_path        = H(encoded_cell_key)
cell_leaf        = H(00 || cell_path || tagged_cell_value)
cell_branch      = H(01 || branch_payload)
term_path        = H(encoded_index_term)
index_leaf       = H(02 || term_path || bitmap_root)
index_branch     = H(03 || branch_payload)
bitmap_leaf      = H(04 || hi48_be || canonical_roaring)
bitmap_branch    = H(05 || branch_payload)
```

Domains are single bytes written above in hexadecimal. Tries are compressed
16-way Patricia trees, visiting each byte's high nibble first. Cell and index paths
are 32 bytes; bitmap paths are 6 bytes. The generic trie supports 1–32-byte paths.

```text
branch_payload = prefix_len:u8
              || packed_relative_prefix[ceil(prefix_len / 2)]
              || state_mask:u16_be
              || tree_mask:u16_be
              || child_hashes[popcount(state_mask)][32]

stored_branch = branch_payload
             || full_leaf_paths[popcount(state_mask & ~tree_mask)][path_width]
```

Masks identify child slots; `tree_mask` is a subset of `state_mask`. Hashes and leaf
paths appear in ascending occupied slot order. Prefix length counts nibbles; odd
prefixes have zero low-nibble padding. Canonical branches have at least two children
and leave room for a child-selection nibble. Unary branches collapse.

Stored full leaf paths are routing metadata and are excluded from the branch
hash. Leaf hashes bind their complete paths. Validation checks canonical bytes,
hashes, depths, and routes on visited nodes; authenticating leaf payloads belongs
to the owning layer.

Empty roots are `H(empty bytes)`, singleton roots are their leaf hashes, and larger
roots are branch hashes. `RootRef` preserves this distinction and singleton paths.
A missing branch row does not establish a singleton. Cell and index singleton
reopening verifies the sole matching flat row; a bitmap singleton verifies its
container. Corrupt or missing referenced data is an error, not absence.

Branch and container rows are immutable. Hash-key reuse requires equality of the
complete stored bytes, including stored-only paths. Mutations rewrite affected
branches and retain old rows. Reading an old root requires its retained metadata
and payloads; supplying an old root with unrelated current flat state is invalid.

### Attribute index and bitmap containers

The flat `Index` table supports ordered term scans. `IndexTrie` commits to the
term-to-bitmap mapping using hashed term paths. Each `BitmapTrie` commits to one
posting list of internal record IDs.

```text
record_id:u64 = hi48:48 bits || offset:16 bits
container_bytes = hi48:6-byte big-endian || portable_roaring
```

A container holds offsets in `0..=65535`. `Bitmap` assembles path-ordered containers
into a `RoaringTreemap` for query set operations, rejecting duplicate or out-of-order
paths and omitting empty chunks.

Canonical serialization strips run compression, then optimizes, then serializes.
For cardinality `N`, the base form is an array when `N <= 4096`, otherwise an
8192-byte bitmap. A run form is selected only when `2 + 4 * run_count` is strictly
smaller than the base payload cost. Ties retain the base form. Roaring's internal
portable fields are little-endian; the outer path is big-endian.

`roaring = "=0.11.5"` is pinned because its representation and serialization rules
affect root hashes. Persisted decoding checks bounds, full consumption, nonempty
contents, and byte equality with canonical re-encoding. Empty working containers
remove their leaves; an empty posting list removes its flat term and index leaf.

`Index::apply` coalesces posting changes by term, container, and offset; the last
input change to a term/record pair wins. Each affected container is loaded and
finalized once, and each term receives one final root update. No-op changes produce
no writes. The result contains the new root and changed terms' before/after roots.
Queries materialize bitmaps or scan ordered terms by bounds/prefix without loading
their bitmaps. Index operations use the caller's transaction and never commit it.

### Branch lifecycle and atomic publication

`BranchId` is a volatile handle scoped to its issuing manager and clones.
`CommitId` is a distinct numeric type; commit zero is genesis. Branches start at
head, and each guarded operation checks that the origin still matches head using
the same snapshot as its reads. Operations serialize per branch.

The overlay contains cell puts and tombstones over committed state. Each write
callback is atomic: errors restore its prior overlay, undo entries, and frames.
Unwinding callback panics restore branch state before propagating. Checkpoints
delimit undo frames; rollback undoes the latest frame. `BranchInfo::version` counts
undo entries and can decrease on rollback; it is not an OCC revision token.

Seal computes a net cell diff and derives index posting changes from actual old
and new attribute values. It applies both to buffered storage, computes the roots,
and freezes the branch without opening a durable writer. Repeated seal reuses the
result after checking head. Sealed branches allow reads but reject writes,
checkpoints, and rollback.

Sealing commit `n + 1` also adds `#roots[n] = state_root(n) || index_root(n)` as a
non-indexed bytes cell. Its name is `n:u64_be`. This avoids committing a root to
itself. The seal-added cell becomes visible through record reads after publication.

Commit seals if necessary, opens one storage writer, rechecks head, replays the
buffered rows, and updates head atomically. Success consumes the branch. A live
stale branch's commit returns `Conflict` and consumes its handle; unknown or
invalidated handles return `HandleInvalid`. A storage publication failure preserves
the sealed branch for retry or discard while leaving committed state unchanged.

Low-level cell/index/trie mutations may stage partial work before returning an
error; their enclosing transaction must be aborted. Record mutations use the
branch journal for operation-level rollback. Direct cell writes are trusted engine
operations and can bypass record invariants.

### Public API and opening

One synchronous, dyn-compatible `Api` trait covers CRUD, branch lifecycle, and
immutable-data signatures. Consumers can use `&dyn Api` or
`Arc<dyn Api + Send + Sync>`. `Database` clones share the engine and branch registry.
Results own their data and expose no storage transactions or cell writers.

| Call | Request | Result |
| --- | --- | --- |
| `create(branch, operation)` | `RecordOp<op::Create>` | `RecordKey` |
| `get(target, operation)` | `RecordOp<op::Get>` | `Record` |
| `patch(branch, operation)` | `RecordOp<op::Patch>` | `()` |
| `delete(branch, operation)` | `RecordOp<op::Delete>` | `()` |

Each constructor requires a key: `RecordOp::create(key)`, `get(key)`, `patch(key)`,
or `delete(key)`. Write helpers accept explicit `CellValue`s and immediately return
`Result<Self>` on invalid names, incompatible kinds, or duplicate names.
`.attribute()` and `.field()` select kind; create's `.insert()` and patch's `.set()`
preserve the supplied value's kind. Patch also has `.remove()`. Operations implement
`Clone`, `PartialEq`, and `Eq` and provide inspection methods for mocks and adapters.
Raw record maps and patch enums are not re-exported by the API.

Get reads all cells, including `#key`, unless `.only(names)` selects a projection.
Names may be text or raw bytes; selections are deduplicated and sorted. Missing
cells are omitted. An empty selection still verifies record existence and identity.
`ReadTarget::Head` reads one committed snapshot; `Branch(id)` reads working state;
`Commit(id)` requires that ID to match the snapshot's head and rejects other commits.

Branch methods are `head`, `begin`, `branch_info`, `checkpoint`, `rollback`, `seal`,
`commit`, and `discard`. `SealInfo` exposes the candidate commit ID and both roots.
`ApiError` separates domain failures, capacity exhaustion, invalid input, and internal
errors. Builder messages identify the method and cell; typed diagnostic causes remain
available through `Error::source()`.

`Database::open(path, &OpenConfig)` opens MDBX directly. `open_with_options` accepts
`MdbxOptions`; `open_memory(&Genesis)` creates a memory-backed database;
`from_store(store, &OpenConfig)` accepts a supplied store. `OpenConfig` contains
`Genesis` and `OpenMode::{CreateIfMissing, ExistingOnly, CreateNew}`.

`Genesis` contains the hash algorithm and cell limits. YAML loading uses an explicit
path and rejects missing, duplicate, or unknown fields. Backend capacity and paths
are separate from genesis identity. The opener checks physical key/value ceilings,
including index keys and a 16 KiB engine-value requirement.

Opening runs detection and initialization/validation under one writer. Pristine
storage has no tables or rows; a pre-existing empty directory is allowed. Tables
without head are corrupt/incomplete state. Creation atomically persists reserved
cells, their state trie, format metadata, genesis identity, and commit-zero head;
the genesis index root is empty. Reopening validates metadata, required cells and
their commitments, allocator, roots, and the previous-commit roots entry without
rewriting genesis. It does not audit every user row or index descendant.

Genesis identity hashes a versioned domain, format/hash/Roaring identifiers, and
sorted length-framed genesis key/value pairs. YAML formatting and field order do not
affect it. Changed startup values cause `GenesisMismatch`. `Database::info()` is an
opening snapshot; `Api::head()` returns the live head.

## Extension contracts

These sections describe extension boundaries and design requirements. Exact public
interfaces and persistent encodings require verification against the owning code
and detailed specifications before implementation.

### Metering and record metadata

Metering associates a record operation with a budget, deterministic pricing context,
and a receipt for successful or failed work. Counts distinguish work performed from
effects applied: user cells, index joins/leaves, and cell/index bytes. Failed-call
charges and pricing snapshots belong to the operation's contract, including cases
where writes are rolled back. Budget enforcement and overflow behavior must agree
with receipt accounting.

`#meta` describes record shape through user-cell count, user-cell bytes, indexed-cell
count, and index bytes. Its encoding affects state roots. Updates belong in the same
atomic mutation and undo journal as the cells and bindings they describe. System
cells are separate from user-cell counters. The metering records provide schema
locations for models and weights.

Operation options fit on `RecordOp`; receipt/result wrappers, precise counts, and
metadata encoding require a single consistent contract across API, record, and
branch layers. Detailed formulas and schedule semantics belong in
`docs/golem-db-metering.md`.

### Generated record keys and typed input conversion

Deterministic key generation requires a specified hash domain, derivation inputs,
and allocation relationship. Genesis key policies and seeds affect deployment
identity; caller-assigned versus generated modes require explicit compatibility
rules. Public keys remain distinct from internal record IDs.

Native Rust input conversion belongs in cells. A conversion trait can encode values
such as `50_i32` while API helpers choose attribute or field kind. Explicit
`CellValue` inputs remain useful for encoded values and copying cells. Transport
parsing belongs to CLI, HTTP, and TCP adapters rather than record logic.

### Immutable data

Immutable data is organized into named segments of rows, with each row represented
by one opaque byte array per column. Every row has a dense segment-local ordinal;
an optional 32-byte `ImmutableDataKey` adds segment-local unique addressing.
Record keys and immutable-data keys are distinct types.

`immutable_data_append` targets a sealed branch and returns a provisional ordinal.
The publication contract makes row data and key bindings visible together at commit;
discarded or losing branches publish neither. Duplicate keys are errors.
`immutable_data_get` addresses a committed row by ordinal or key.
`immutable_data_range_of` and `immutable_data_rows_of` identify a commit's appended
range and rows. The `Database` entry points return `ApiError::NotImplemented`
without validation, I/O, or state changes. Segment declarations, column arity,
persistence, pruning, and rewind must preserve ordinal/key consistency.

### History, proofs, and storage evolution

Historical record/index queries, forks, and rewind require matching historical
flat state and root metadata. Retaining immutable branch/container rows alone does
not reconstruct old record payloads or term tables. Changesets, reconstruction,
retention, and garbage collection must preserve the roots and leaf data needed by
readers and proofs.

Proofs authenticate leaf preimages and complete paths against domain-separated
roots; stored routing metadata is not a substitute for authenticated payloads.
Proof wire formats and completeness guarantees depend on the owning cell/index
schema and record metadata.

Alternative storage backends use `Store`; backend selection and configuration belong
at opening. A new bitmap format, changed hash algorithm, branching factor, domain,
or canonical encoding changes commitments and requires format compatibility rules.
Zero-copy reads, caching, parallel hashing, and physical-layout changes must preserve
snapshot isolation, canonical bytes, and atomic publication.

### Queries, concurrency checks, and administration

A record query layer combines attribute predicates through ordered term scans and
bitmap set operations, then materializes records with projection, sorting, and
paging. Query and count contracts must define snapshot selection, ordering,
pagination continuity, and metering together. The index crate's scans and bitmap
operations are building blocks; they are not record-query methods on `Api`.

Record versions for optimistic concurrency control require an explicit version
identity and an atomic expected-version check with the mutation. Branch undo-entry
counts do not supply this guarantee. Version creation, updates, deletion/recreation,
rollback, and persistence need consistent semantics.

Administration covers deployment and metering-model management through a separate
contract from ordinary record CRUD. Introspection and administrative changes need
defined visibility and publication rules. Adding required methods to `Api` changes
the contract for every concrete implementation and mock; operation-level additions
can use typed request options where appropriate.

## Source map and verification

- Public usage: `crates/api/README.md`; interface: `crates/api/src/lib.rs`.
- Cell wire format and system catalogue: `crates/cells/src/{lib,types,value,system}.rs`.
- Persistent initialization: `crates/api/src/genesis.rs`; head: `crates/branch/src/head.rs`.
- Trie encoding: `crates/merkle/src/node.rs`; term/container codecs: `crates/index/src/`.
- Domain design references: `docs/golem-db-design.md`, `docs/golem-db-api.md`,
  `docs/golem-db-metering.md`. Exact callable signatures and stored codecs are defined
  by their owning crates; broader design contracts must not be inferred as runtime
  behavior where the interface explicitly rejects an operation.

Rust edition is 2024; `rust-toolchain.toml` selects Rust 1.97 with rustfmt and Clippy.
MDBX's pinned bindings are `libmdbx = 0.6.6` and `mdbx-sys = 13.11.0`; native builds
require a C compiler and libclang. Unit tests live under crate `src/tests` trees;
cross-component tests and benchmarks live in `crates/integration-tests`.

```sh
cargo build --workspace --all-targets --all-features
cargo nextest run --workspace --all-features
cargo test --workspace --all-features --doc
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo bench --workspace --all-features --no-run
```

Canonical byte/hash vectors protect the commitment format. Randomized tests compare
incremental state with independent set/map and fresh-trie models. Integration tests
exercise memory/MDBX behavior, snapshots, root reopening, atomic failure, rollback,
and process restart. Storage-work tests check that small edits touch only affected
containers and trie branches.
