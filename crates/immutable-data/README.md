# golemdb-immutable-data

Typed, ordinal-based physical storage using NippyJar, with separate transactional
key and commit-range metadata helpers. There is no branch state, database head,
system segment, or database publication operation in this crate.

## Interface

Construct `ImmutableDataStorage::open(config, genesis_id, directory)` for files or
`::in_memory(config, genesis_id)` for memory. The implementation is concrete;
backend dispatch is private. NippyJar is pinned to Reth v2.1.0 (`d58c6e3d`).

Segment names match `[a-z][a-z0-9_]{0,63}`: 1–64 ASCII characters, starting with
a lowercase letter, followed by lowercase letters, digits or underscores. Invalid
names are rejected without normalization in schema construction and configuration
validation, before opening files. This prevents case collisions in filenames.
Column names retain the cell user-name grammar in genesis configuration.

| Method | Behavior |
| --- | --- |
| `schema(segment)` | Borrow the named segment's validated schema. |
| `append(segment, commit, &row)` | Append a complete typed row and return its global segment ordinal. Commit selects a file span only. |
| `get(segment, ordinal, &options)` | Read a physical row with named projection. |
| `end_ordinal(segment)` | Report the next physical ordinal, including successful buffered appends; zero when empty. |
| `truncate(segment, from)` | Remove the physical tail `[from, end)`, including later files. |
| `sync()` | Synchronize pending data, offsets, configurations and directory entries. |
| `bind_key(tx, segment, key, ordinal)` | Stage a unique bidirectional binding to an existing physical row. |
| `get_by_key(tx, segment, key, &options)` | Resolve the binding from the supplied transaction and read that ordinal. |
| `record_commit_range(tx, segment, commit, range)` | Stage one nonempty range contained in that commit's file span. |
| `rows_of(tx, segment, commit, &options)` | Read the stored ordinal range, or return an empty vector when there is no entry. |

`append`, `truncate` and `sync` require mutable access. Reads can flush an active
writer through an internal mutex; this mutex does **not** provide publication or
snapshot isolation. Key/range insertion validates physical bounds without flushing
a healthy active writer. The caller owns and commits every KV transaction.

## Files and ordinals

Genesis `shard_span` counts commits. Each segment independently uses files named
`segment-{name}-{start_commit}-{end_commit}.jar`; commit numbers are unpadded decimal
values and the end is exclusive. Discovery sorts parsed `u64` starts numerically.
Both bounds are known at creation. `.conf` and `.off` companions retain the full
segment name. Filenames are never renamed on rotation.

```text
start_commit = commit / shard_span * shard_span
end_commit   = start_commit + shard_span
```

Several rows may be appended for one commit. Empty commits/spans require no files
or calls. An append into an older span than the segment's latest file is rejected;
exact commit order and publication are caller concerns. There is no special
physical genesis commit.

The custom serialized NippyJar header stores format, genesis identity, segment ID,
first commit and first ordinal. NippyJar stores row count, arity and compression.
Commit IDs and row ordinals are independent:

```text
physical range = [first_ordinal, first_ordinal + row_count)
local row      = ordinal - first_ordinal
```

Appends reuse an active writer per segment. Rotation synchronizes the previous
file and starts the new file at its ending ordinal. `sync()` is required before
publishing metadata or closing storage; dropping a writer is not a durability
operation. A physical read may flush its selected active writer, but does not
publish anything in MDBX.

When an existing file is selected for reading or appending, extra data or offsets
beyond the `.conf` row count are discarded through NippyJar's native recovery.
This handles dropping a writer after unsynced appends. Required file lengths and
boundary offsets are checked first: missing configured data is an error and is
never healed by reducing the configured row count. Recovery does not scan
interior offsets.

Reads check the active/latest file, then inspect older configurations newest-first.
Discovery sorts numeric commit starts and opens payloads only in the matching
file. No historical configuration or reader cache is retained. A commit-range
read knows its file span directly from the supplied commit. Reads validate basic
file layout, including offset-file length and first/final offsets, then let
NippyJar read selected column offsets directly through mmap. There is no separate
per-column offset scan; malformed interior offsets may panic inside NippyJar.

Rows reuse `CellValue::value()` payloads without cell tags/index flags. Schemas
supply types; reads validate selected values through cell codecs. Named projection
preserves requested order. Results own their data and support `get::<T>(name)`,
`value(name)` and `iter()`. All three compression choices (none, LZ4, Zstd without
a dictionary) are supported.

## Metadata

Both ordinary store tables are outside the commitment. IDs are assigned by genesis
segment-name order. Integer components are fixed-width big endian.

| Table | Key | Value |
| --- | --- | --- |
| `ImmutableDataKeys` | `0 || segment:u32 || key:bytes32` | `ordinal:u64` |
| `ImmutableDataKeys` | `1 || segment:u32 || ordinal:u64` | `key:bytes32` |
| `ImmutableDataCommitRanges` | `segment:u32 || commit:u64` | `first_ordinal:u64 || end_ordinal:u64` |

Only keyed rows create key mappings. Key lookups check forward/reverse agreement.
Commit ranges are nonempty and inserted once; no entry is needed for an empty
batch. The coordinator supplies exact batch boundaries; the file layer can verify
that the range belongs to a span, not that it was appended by a particular commit.
`rows_of` callers establish commit existence/visibility. A mapping to missing rows
is corruption. Abort the transaction on any metadata mutation error.

`metadata` also exposes low-level ID-based helpers. These validate metadata
encodings/uniqueness, while the storage-handle conveniences additionally validate
physical bounds. No method reads or writes `Superblock/head`.

## Caller-owned publication and rollback

The later coordinator must serialize the whole operation and exclude readers:

1. Remember affected segments' original ending ordinals.
2. Append rows with the target commit as the routing coordinate.
3. Synchronize physical storage.
4. Stage key bindings and nonempty commit ranges alongside the database changes.
5. Publish the caller's KV transaction.

On a definitely unsuccessful publication, abort the transaction, truncate affected
segments to their remembered ends and synchronize rollback before releasing access.
Resolve uncertain publication outcomes before truncating. Physical reads can see
unpublished rows: the coordinator must enforce committed visibility.

Physical `truncate` does not remove metadata. It supports no-op, partial-file,
exact-boundary and complete rollback, with ordinal reuse. Invalid bounds are
rejected before mutation. Failed I/O can leave partial work; callers must stop
publication until recovery succeeds. A failed active writer rejects further use.
File access validates required files before NippyJar's auto-healing writer is
constructed. Recovery beyond unfinished file tails, including reconciling synced
files with MDBX publication, remains iteration 5.

The directory has a single owner enforced by an OS lock. Readers do not escape
individual calls. Branch overlays, publication coordination, database crash
recovery, retention/key expiry, digests and public API facade integration are not
implemented here. Iterations 4–6 are described in the implementation plan.
