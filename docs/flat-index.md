# Flat Index

This document describes the flat index layout: what changes from the two-level index on `main`,
why, what it costs, and how it answers the concerns that led to the two-level design.
`golemdb-design.md` still describes the two-level layout.

## Summary

The per-term posting-list tries (`BitmapTrie`) and their container table (`BitmapContainer`) are
removed. Each Roaring container becomes a live row of `Index`, keyed by the term in value order,
and a single `IndexTrie` commits to every container. The index then follows the same model as
cells: live rows hold the data, and the trie is only a commitment.

Containers stay bounded (16-bit Roaring chunks, at most 65,536 record IDs each). The flat design
is not a single list of record IDs per term.

## Layout

### `main`

| Table | Key | Value |
|---|---|---|
| `Index` | term | locator: `bitmapHash ‖ commit [‖ hi48]` |
| `IndexTrie` | `commit ‖ route` | branch over terms, leaf = term's bitmap root |
| `BitmapTrie` | `commit ‖ H(term) ‖ route` | branch of the term's posting-list trie |
| `BitmapContainer` | `commit ‖ H(term) ‖ hi48` | Roaring container |

`hi48` is a container's name: the top 48 bits of the record IDs it holds, as 6 big-endian bytes.
The container's Roaring bitmap holds the low 16 bits of each ID, so at most 65,536 IDs.

Reading a term follows `Index` → `BitmapTrie` walk (skipped for a single-container term, whose
locator holds `hi48` and the commit) → each `BitmapContainer` row. Every hop after `Index` is a
separate read keyed by `commit ‖ H(term) ‖ …`, so it lands at a scattered place in the file.

### Flat

| Table | Key | Value |
|---|---|---|
| `Index` | `termKey ‖ hi48` | Roaring container (canonical bytes) |
| `IndexTrie` | `commit ‖ route` | branch over all containers |

- **One row per container, not per term.** A term with postings in N containers has N adjacent
  rows. A term exists exactly while it has at least one container row.
- **Logical identity is unchanged:** a term is still its canonical bytes
  (`name ‖ 0x00 ‖ tag ‖ value`), and routing still uses `H(term)`.
- **`termKey`** is the term with an end marker, so `hi48` can follow it. On `main`, the
  term was the whole `Index` key and needed none.
  - Fixed-width values already have a known length and are written as is.
  - Variable-width values end with `0x00 0x01`. Since such a value can itself contain `0x00`, each
    `0x00` inside it is escaped as `0x00 0xff`. This keeps byte order equal to value order.

  Nothing hashes `termKey`. Changing its encoding changes the on-disk order, not the commitment.
- **Trie path:** 38 bytes, `H(term) ‖ hi48` (32 + 6). The top levels branch on the term
  hash, as `IndexTrie` does on `main`. Below a term's prefix, the levels branch on the container,
  as that term's `BitmapTrie` does on `main`.
- **Leaves are virtual:** `H(0x04 ‖ path ‖ roaring)`, recomputed from the `Index` row. The trie
  stores only branches.

### Key changes

A row's physical key is where MDBX stores it. Its logical key is its trie path, which is what
`IndexRoot` commits to.

#### `Index`

| | `main` | Flat | Why |
|---|---|---|---|
| Row | one per term | one per container | A term's posting list sits under its own key, with no separate table to jump to. Containers stay bounded at 65,536 IDs, so a write still touches one small row. |
| Physical key | term (`name ‖ 0x00 ‖ tag ‖ value`) | `termKey ‖ hi48` | `hi48` after the term keeps a term's containers together, in record-ID order. `termKey` ends the term unambiguously and keeps value order. No `commit`: rows are live and overwritten in place, like `Cell`, so each container has exactly one current row. |
| Logical key | `H(term)`: the term's leaf | `H(term) ‖ hi48`: the container's leaf | The commitment moves down from terms to containers, since each container row is what the trie now commits to. |
| Value | locator: `bitmapHash ‖ commit [‖ hi48]` | the container's Roaring bytes | A read finds the containers by scanning the term's key, not by following a pointer into versioned rows. |

#### `IndexTrie`

| | `main` | Flat | Why |
|---|---|---|---|
| Physical key (branch rows) | `commit ‖ route` | `commit ‖ route`, unchanged | Branches are rewritten along every changed path in every commit. With `commit` first, each commit's branch rows are appended together. Queries don't read branches, so their order doesn't affect reads. |
| Logical key (leaf paths) | `H(term)`, 32 bytes | `H(term) ‖ hi48`, 38 bytes | One trie replaces the term trie plus a `BitmapTrie` per term: a term's containers are the subtree under `H(term)`. `H(term)` stays first because trie paths must be fixed-width and well spread to keep the trie balanced. |
| Leaf | a term, hashed over its bitmap root | a container: `H(0x04 ‖ path ‖ roaring)` | The term's leaf existed only to point at its bitmap trie, which is now part of this one. The full path ties each container to its term and position. |
| Leaf stored in | a term-leaf hash, with the per-term `BitmapTrie` below it | nowhere: recomputed from the `Index` row | The `Index` row already holds everything the leaf hash needs. |

### Code

The main changes:

- `crates/index`:
  - `tables.rs`: `Index` and `IndexTrie` only;
  - `index_trie.rs`: trie paths, leaf hashing, checked batch updates;
  - `index/writer.rs`, `index/reader.rs`: rewritten for container rows. `TermScan` reads a range
    in one pass, grouping adjacent rows by term;
  - `term.rs`: `termKey` encoding (`key_bytes`), splitting row keys back into terms;
  - `term_range.rs`: a range's bounds on container keys.
- `crates/merkle`:
  - `trie.rs`: `apply_checked` (`CheckedEdit`, `LeafMismatch`) replaces the separate check walk;
  - `node.rs`: `MAX_PATH_BYTES` raised to 127 for 38-byte paths; `LeafStorage` removed;
  - `table.rs`: `TrieTable` is no longer generic.
- `crates/api/src/config.rs`: open-time check that the worst-case `termKey ‖ hi48` fits
  the backend's key size.
- `crates/query/src/posting_source.rs`: reads a range's terms and postings in one pass.
- Removed: `crates/index/src/bitmap_trie.rs`, `crates/merkle/src/payload_trie.rs`.

## Why

### Reading a posting list on `main`

On `main`, reading a term's postings is a chain of separate lookups: the `Index` locator, then the
term's `BitmapTrie` branches, then each `BitmapContainer` row. All but the first are keyed by
`commit ‖ H(term) ‖ …`, so:

- a term's containers are spread across the commits that wrote them;
- neighbouring terms (for example, consecutive values of the same attribute) are spread by
  `H(term)`;
- every hop is a separate, scattered read.

Queries that read one small term pay this a few times. Queries that read many containers, or many
neighbouring terms, pay it once per container, and once the database no longer fits in memory each
of those is a random disk read.

### This isn't caused by versioned keys

- **Before versioned keys,** container and branch rows were keyed by hash, so a term's
  containers were at random positions. Only `Index` was in value order.
- **Versioned keys** (`commit ‖ H(term) ‖ …`) made each commit's writes contiguous, which is
  where the write throughput came from. But neighbouring terms are written across many commits,
  and `H(term)` scrambles value order within a commit, so reads stayed scattered.

Both schemes scatter the posting lists because each term is reached through its own structure.
Only keying containers by term order puts them next to each other.

### The fix: co-locality, in three parts

1. **Key order (physical):** keying container rows by `termKey ‖ hi48` puts a term's
   containers together, and neighbouring terms next to each other. This is what makes reads
   contiguous.
2. **Live rows (storage):** container rows are overwritten in place, so there is exactly one
   current row per container: nothing to skip, and no locator to follow before reading.
3. **One trie (logical):** with live rows, the per-term tries no longer serve reads, so a single
   trie over `H(term) ‖ hi48` replaces them. This is what changes `IndexRoot` and the
   proof shape. Locality doesn't require it; it's a simplification that live rows make possible.

Versioned keys still apply where they help: `IndexTrie` remains keyed by `commit ‖ route` and
keeps append-only writes. `Index` moves to live, value-ordered rows, as `Cell` already is.

### Alternatives considered

#### Re-key `main` only

Keep the two levels and change only physical keys:

| Table | `main` | Re-keyed |
|---|---|---|
| `BitmapContainer` | `commit ‖ H(term) ‖ hi48` | `termKey ‖ hi48 ‖ commit` |
| `BitmapTrie` | `commit ‖ H(term) ‖ route` | `termKey ‖ route ‖ commit` |

This keeps the commitment unchanged and gets most of the locality, but:

- old versions sit next to live rows until GC removes them, and reads skip over them;
- each term still needs its locator and a (now local) trie walk to find the current versions;
- these tables lose versioned-key write locality;
- namespaces become variable-width;
- all the two-level machinery stays.

#### Live rows under the two-level commitment

Store containers as live `Index` rows keyed `termKey ‖ hi48`, as in the flat design, but
keep the per-term `BitmapTrie`s and the `IndexTrie` over terms, hashing container leaves from the
live rows. Roots and proofs stay the same, but:

- each term still needs its bitmap root stored, as a locator row;
- every write still updates two tries: the term's bitmap trie, then its leaf in `IndexTrie`;
- the namespaces, term leaves and their hash domains stay.

The flat design gets the same locality with one trie, and removes complexity instead of adding to
it.

## Bounded costs

The two-level design was chosen over "a list of record IDs per term", for reasons recorded in
`golemdb-design.md` ("Why the Posting List Gets a Second Tier"). The flat design keeps the bounded
containers, so each reason still holds.

| Concern | Per-term list | Flat design |
|---|---|---|
| Cost of adding one ID to a big term (`status=active`, `$all`) | grows with the list | one container, at most 65,536 IDs (at most 8 KiB), as on `main` |
| Hashing per write | grows with the list | one container leaf plus one trie path |
| Trie depth | — | about log16(terms) + log16(containers in the term): what `main` pays across its two tries |
| Bad actors inflating a term | unbounded cost per write | bounded: the term gains containers, each write still touches one |
| History per change (change sets, as designed) | snapshots the whole list | one container's pre-image |
| Metering | depends on list length | flat cost per container touched, unchanged |

Metering formulas don't change. `main` prices a join at one trie depth (`INITIAL_DEPTHS.index_trie`
= 6) while actually updating two tries. In the flat design a join updates one path, so the model
matches the work. The depth becomes about log16(all containers): for example, 1M terms with about
7 containers each gives about 5.7, so 6 still fits.

## Queries that improve

Reads follow the key order, so queries that touch many containers read contiguous rows instead of
one scattered row per container:

- **Many neighbouring terms of one attribute:** the terms, and all their containers, are one
  contiguous stretch of `Index`, read by a single scan.
- **Big terms:** a term's containers are adjacent rows under its key, read without a trie walk.
- **Small terms:** one row, read directly, instead of a locator and then a container read.

The gain is largest when the database doesn't fit in memory, where scattered reads go to disk.
The work still grows with the number of containers read; the flat layout makes each one cheaper,
not fewer of them.

## Trade-offs

### Pros

- Some queries improve: see [Queries that improve](#queries-that-improve).
- One model for cells and the index: live rows plus a commitment trie. History, proofs and GC
  each need one approach.
- Less code: namespaces, `PayloadTrie`, leaf rows with commits, root locators, the
  single-container special case and two hash domains are removed. With GC, the two bitmap tables
  also leave the `Stale` table codes.
- Writes do less work: a change writes one container row and one trie path, instead of a new
  container version, a bitmap-trie path, an index-trie path and a locator. That means less hashing
  and fewer dead rows for GC. Write cost stays bounded as before.
- Metering stays per effect, and its depth model now matches the work done.

### Cons

- **New commitment format:** `IndexRoot`, leaf hashes and proofs change.
- **Longer keys for strings:** escaping can double a string value's bytes, and the terminator and
  `hi48` add 8, so the open-time key-length check allows a smaller `max_str_len` for a given
  backend key size.
- **Larger branch rows:** a branch stores each leaf child's full trie path, now 38 bytes. That's
  6 more than a term leaf on `main`, and 32 more than a container leaf in a `BitmapTrie`.
- **Reads skip the container hash check:** on `main`, reading a term walked its bitmap trie and
  checked each container against its leaf hash along the way. Flat reads decode `Index` rows
  directly, as cell reads do. Checking them would mean walking `IndexTrie` branches, which are
  keyed by commit and scattered, giving back the locality.

## Appendix: benchmarks

`main` is the flat branch's merge base (`d910d14`). Neither tree runs GC.

### Queries

Linux, 15 GB RAM, NVMe. 1.5M records shaped like Arkiv's: 50 projects (`app-0` holds about 22%),
10 entity types, 4 statuses, 10k refs, 10k owners (the busiest holds about 10%), unique `id` and
`created`, and `$expiresAt` over 150k blocks with about 10 records each, plus a 0.5–2 KB unindexed
payload. Records were created 1,000 per commit. Both databases end at 38 GB.

Every query returns the first 100 records. Cold is the first run after evicting the page cache.
For lookups (`:owner`, `:ref`, …), each of 10 runs draws a new value: cold is the first run and warm
is the median of the 10. For the other queries, warm is the median of runs 2 and 3.

| Query | `main` cold | Flat cold | Speedup | `main` warm | Flat warm | Speedup |
|---|---|---|---|---|---|---|
| **Expiry** | | | | | | |
| `$expiresAt` > top 1% (~15k matches) | 1,530 ms | 48 ms | 32× | 43.5 ms | 10.5 ms | 4.1× |
| `$expiresAt` > top 10% (~150k) | 9,812 ms | 413 ms | 24× | 539 ms | 187 ms | 2.9× |
| `$expiresAt` > top 50% (~750k) | 22,881 ms | 1,484 ms | 15× | 2,103 ms | 443 ms | 4.7× |
| `project = :p AND $expiresAt` 10% | 9,760 ms | 420 ms | 23× | 572 ms | 204 ms | 2.8× |
| **Lookups** | | | | | | |
| `$owner = :owner` (~135) | 30.1 ms | 23.7 ms | 1.27× | 23.9 ms | 20.5 ms | 1.17× |
| `ref = :ref` | 29.7 ms | 23.8 ms | 1.25× | 23.5 ms | 20.1 ms | 1.17× |
| `id = :id` | 0.89 ms | 0.59 ms | 1.51× | 0.79 ms | 0.55 ms | 1.44× |
| `project AND status` | 42.3 ms | 33.7 ms | 1.25× | 23.5 ms | 21.5 ms | 1.09× |
| `project AND entityType` | 41.5 ms | 33.6 ms | 1.24× | 28.6 ms | 23.8 ms | 1.20× |
| **Common values** | | | | | | |
| `$owner` = busiest (~150k) | 19.5 ms | 12.5 ms | 1.56× | 6.5 ms | 6.2 ms | 1.05× |
| `status = 'active'` (~375k) | 21.8 ms | 15.7 ms | 1.39× | 9.5 ms | 9.3 ms | 1.02× |
| `status = 'active' LIMIT 10` | 18.8 ms | 12.4 ms | 1.52× | 7.0 ms | 6.9 ms | 1.02× |
| `project = 'app-0'` (~330k) | 23.4 ms | 16.7 ms | 1.40× | 5.5 ms | 4.9 ms | 1.13× |
| `project = 'app-0' AND status = 'active'` | 40.3 ms | 30.5 ms | 1.32× | 10.9 ms | 10.3 ms | 1.06× |
| `project = 'app-1' AND status = 'active'` | 42.8 ms | 33.9 ms | 1.26× | 8.7 ms | 8.7 ms | 1.00× |

Loading the 1.5M records took 1,330 s on `main` and 1,271 s on flat.

### Writes

Two conformance-harness scenarios, 1,000 operations per block:

- `churn`: Arkiv-like records (6 indexed attributes, 1 KB payload). 100 blocks of creates, then
  200 blocks of 35% creates, 30% patches and 35% deletes;
- `s300k`: smaller records (3 indexed attributes, 256-byte payload). 300 blocks of creates.

Seal plus commit per block, averaged over blocks 200 onwards and two rounds; disk at the end.

| Scenario | Machine | `main` | Flat | Change | Disk, `main` → flat |
|---|---|---|---|---|---|
| `churn` | Linux | 378 ms | 346 ms | −8% | 3,232 → 3,040 MiB |
| `churn` | macOS | 252 ms | 227 ms | −10% | 3,232 → 3,040 MiB |
| `s300k` | Linux | 330 ms | 322 ms | −2% | 2,768 → 2,704 MiB |
| `s300k` | macOS | 185 ms | 176 ms | −5% | 2,768 → 2,704 MiB |
