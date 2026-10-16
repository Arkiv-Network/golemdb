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
| `reader()` | Clone an independent committed reader, without access to active writers. |
| `append(segment, commit, &row)` | Append a complete typed row and return its global segment ordinal. Commit selects a file span only. |
| `get(segment, ordinal, &options)` | Read a physical row with named projection. |
| `end_ordinal(segment)` | Report the next physical ordinal, including successful buffered appends; zero when empty. |
| `truncate(segment, from)` | Remove the physical tail `[from, end)`, including later files. |
| `sync()` | Synchronize pending data, offsets, configurations and directory entries. |
| `recover(tx, commit)` | Restore physical tails to the supplied durable commit-range snapshot, without changing metadata. |
| `bind_key(tx, segment, key, ordinal)` | Stage a unique bidirectional binding to an existing physical row. |
| `get_by_key(tx, segment, key, &options)` | Resolve the binding from the supplied transaction and read that ordinal. |
| `record_commit_range(tx, segment, commit, range)` | Stage one nonempty range contained in that commit's file span. |
| `rows_of(tx, segment, commit, &options)` | Read the stored ordinal range, or return an empty vector when there is no entry. |

`append`, `truncate`, `sync` and `recover` require mutable access and borrow the
writer without locking. Physical read conveniences on this handle can flush an
active writer through an internal mutex; use the independent reader below for
committed reads. Key/range insertion validates physical bounds without flushing
a healthy active writer. The caller owns and commits every KV transaction.

## Committed reader

`ImmutableDataReader` is cloneable and exposes `schema`,
`get(tx, published_commit, segment, address, options)` and
`rows_of(tx, published_commit, segment, commit, options)`. Supply one published KV
snapshot and the commit number read from that same snapshot. It never reads head
itself, accepts no branch handle, and never locks or flushes the append writer.

Keys, ranges and ordinal bounds come from the snapshot. Future commits and
unpublished ordinals are rejected. File discovery is bounded by the segment's
latest published batch, so incomplete future files are ignored even after empty
commit gaps. Configurations may describe additional rows; only the requested
prefix must exist. Reads tolerate an extra partial data/offset tail.

The pinned NippyJar cursor uses physical EOF for its last column. Committed reads
instead use NippyJar's mapped offset reader and codecs, ending every selected
column at its stored terminal offset. No extra per-column validation scan is added.
Readers hold shared cleanup access until their local mappings drop; only physical
truncate/recovery takes exclusive access. Append and sync proceed independently.
Memory readers use a short collection lock per operation. Cloned reader handles
retain directory ownership, so close all of them before reopening the directory.

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

When an existing file is selected for a physical read or append, extra data or offsets
beyond the `.conf` row count are discarded through NippyJar's native recovery.
This handles dropping a writer after unsynced appends. Required file lengths and
boundary offsets are checked first: missing configured data is an error and is
never healed by reducing the configured row count. Recovery does not scan
interior offsets. Tail cleanup waits for independent readers to release their
mappings; committed reads only inspect their published prefix and never heal files.

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
batch. The publisher supplies exact batch boundaries; the file layer can verify
that the range belongs to a span, not that it was appended by a particular commit.
`rows_of` callers establish commit existence/visibility. A mapping to missing rows
is corruption. Abort the transaction on any metadata mutation error.

`metadata` also exposes low-level ID-based helpers. These validate metadata
encodings/uniqueness, while the storage-handle conveniences additionally validate
physical bounds. No method reads or writes `Superblock/head`.

`metadata::end_ordinal_at(tx, segment_id, commit)` reads the latest range at or
before the supplied commit with one reverse cursor lookup. It returns zero when
no range exists for that segment. Branch staging uses this published boundary for
provisional ordinals; physical file tails do not determine allocation. Retention
must preserve this boundary when pruning is designed.

## Caller-owned publication and rollback

The branch manager serializes publishers through the whole operation. Its internal
`commit.rs` implements the procedure; independent committed readers keep running:

1. Open a KV writer, recheck head and stage sealed cell/index changes.
2. Verify staged ordinals against published/physical ends, then append rows with the target commit as the routing coordinate.
3. Stage key bindings and nonempty commit ranges in the same transaction.
4. Synchronize physical storage.
5. Publish the caller's KV transaction.

On failure, reacquire a writer and resolve the publication outcome before trimming
files. A published outcome is success; a known abort restores physical storage to
the durable ranges before releasing access. An unresolved outcome or failed recovery
blocks further publication until reopen and recovery. Committed readers can still
read intact prefixes using authoritative snapshots. Raw physical reads can see
unpublished rows; the independent committed reader enforces snapshot visibility.

Physical `truncate` does not remove metadata. It supports no-op, partial-file,
exact-boundary and complete rollback, with ordinal reuse. Invalid bounds are
rejected before mutation. Failed I/O can leave partial work; callers must stop
publication until recovery succeeds. A failed active writer rejects further use
until recovery drops it and restores the published prefix.

## Recovery

`open` acquires directory ownership; it cannot recover without a published KV
snapshot. The branch constructor calls `recover(tx, published_commit)` under a
store writer before allowing access. Direct callers must exclude other publishers
and supply an authoritative snapshot; recovery waits for file readers internally.
Recovery does not inspect or
write `Superblock/head` and never changes key/range metadata.

Recovery scans ranges once per segment, checks contiguous ordinals and rejects
future entries, then groups them by file span. Required file identities, row counts
and prefix boundaries must cover all published rows. Missing committed data is an
error. Buffered writers are dropped before any trimming. Recovery trims offsets
and syncs them first, then trims and syncs data, and finally lets NippyJar reduce
the configuration's row count to the validated prefix. Wholly unpublished files
and incomplete creation companions are removed; directory changes are synced.

The pinned NippyJar version syncs data and offsets before atomically replacing its
configuration through a synced temporary file and rename. Thus an interrupted
append retains a configuration covering at least the published prefix. Recovery
needs no additional journal and is repeatable after interruption. This assumes
durable KV commits and filesystem sync/atomic-rename guarantees; it is not a full
payload corruption audit. Retention will need to preserve recovery boundaries.

The directory has a single owner enforced by an OS lock shared by the writer and
reader handles. Mappings do not escape individual calls. Independent branch staging is implemented in `golemdb-branch`.
Publication coordination and startup recovery live in `golemdb-branch`. The
`golemdb-api` facade opens storage once, retains its independent reader and a
shared store handle, and transfers storage to the branch manager. Committed reads
use the API's store snapshot directly. The facade exposes typed rows and projections.
Retention/key expiry and digests remain deferred.
