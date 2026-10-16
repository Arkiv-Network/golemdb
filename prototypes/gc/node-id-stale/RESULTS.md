# Node IDs and stale-list GC

Native base: #60 at `6ac18eb2e9845e8d199267c90c78cd02daf6761e`. Both variants use the same authenticated-node cache, batched trie updates, APPEND publication and durable stale-list collector. The collector processes all eligible chunks in each measured publication. ID allocation is transactional and the counter is published once with the sealed write set; allocation reads/updates that counter in the in-memory seal buffer for each node. This experiment does not include the original sweep prototype or reference counts.

Host: gf-prov-1/testnet1-hetzner, Ryzen 9 7950X3D, CPUs 28–31, 24 GiB limit, durable MDBX, RAM-fitting data, pinned image `sha256:a3d97ff15bc1da1ce8ec29c92ed91295df6a9fa579023d25c9fad6de33dc4d25`, Rust 1.97.1, release build. Three paired rounds use fresh databases and alternating order. W counts old retained versions; head is retained in addition.

Build 20,000 records in the active writer pool; warm up with 65 commits of mixed traffic; measure 60 commits of 1,000 operations. Configured weights: 35% create, 30% patch, 35% delete. Missing or reused targets fall back to creates. Measured batches contain 21,688 creates, 17,728 patches and 20,584 deletes. Cycle timing includes begin, operations, seal, GC and durable commit. Generation, root logging, memory sampling and final validation are outside timing. Values below are medians of per-run metrics, not pooled quantiles.

| W | Path ops/s | ID ops/s | ID change | Seal p50 path / ID, ms | Commit p50 path / ID, ms | Cycle p99 path / ID, ms | Peak RSS path / ID, MiB |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 6452 | 6463 | 0.2% | 82.91 / 79.96 | 67.29 / 69.98 | 161.35 / 160.47 | 281.0 / 316.0 |
| 8 | 6376 | 6386 | 0.2% | 83.52 / 80.53 | 68.41 / 71.36 | 163.16 / 166.69 | 326.0 / 360.2 |
| 64 | 6551 | 6501 | -0.8% | 83.77 / 81.52 | 64.47 / 67.66 | 161.24 / 159.75 | 638.5 / 661.9 |

These runs show no repeatable throughput gain from IDs. Seal is slightly faster,
but commit is slower. The ID implementation is useful as a comparison prototype;
these results do not support replacing version/path addressing with it.

| W | Merkle key bytes path / ID, MiB | Merkle key + value bytes path / ID, MiB | Stale key + value bytes path / ID, MiB | MDBX Merkle table pages path / ID, MiB | Allocated file path / ID, MiB |
|---:|---:|---:|---:|---:|---:|
| 0 | 3.08 / 1.05 | 23.22 / 21.19 | 0.00 / 0.00 | 74.50 / 107.78 | 210 / 245 |
| 8 | 4.56 / 1.88 | 62.55 / 59.88 | 1.70 / 1.05 | 118.52 / 151.05 | 255 / 288 |
| 64 | 14.74 / 7.64 | 333.77 / 326.67 | 13.39 / 8.32 | 419.07 / 447.01 | 569 / 591 |

File sizes come from the separate-process table scan after closing and reopening
the workload database. They can differ from the harness's size at the end of
measurement. At W=0, ID keys reduce BitmapContainer pages from 14.93 to 4.93 MiB.
However, CellTrie pages grow from 47.45 to 81.39 MiB and IndexTrie pages grow from
12.12 to 21.45 MiB. The larger physical tables are measured; their allocation and
packing causes have not been profiled. A future ID assignment order could change
this result.

The path layout already has short branch keys: at W=0, CellTrie keys average
11.2 bytes and IndexTrie keys average 11.0 bytes. IDs reduce those keys to 8
bytes. Average branch values remain about 219 and 239 bytes, respectively.
This is a much smaller reduction per branch than replacing a 32-byte hash key.
The largest key reduction is in bitmap containers, whose keys fall from 46
bytes to 8. The measurements isolate neither allocator CPU cost nor individual
MDBX page splits, so they do not prove which mechanism causes the extra commit
time.

In this implementation, IDs follow bottom-up construction order: children
receive IDs before their parents. Version/path branch keys sort by route depth
and packed path within each commit and namespace. Assigning IDs after the batch,
in that same depth/path order, is an unmeasured follow-up. It would also allow
reserving and publishing the allocation counter once per batch instead of
updating it for each node in the seal buffer.

Paired throughput changes in rounds 1 / 2 / 3:

- W=0: -0.1% / 0.2% / 0.2%.
- W=8: 0.1% / 0.0% / 0.4%.
- W=64: -0.3% / -0.8% / -0.1%.

All 18 runs reopened MDBX in another process. All roots and final cell/posting digests match across layouts, windows and rounds. Both layouts store the same number of nodes and the same value bytes. The logical size change therefore comes from shorter node keys and stale-list entries. MDBX page usage and allocated file size also depend on page packing, allocation order and reusable pages; smaller keys do not imply a smaller file. Memory is process VmHWM before final scans and includes mapped pages, setup and warmup. It does not isolate heap use by the cache.

The host runs other workloads. OS page caches are not dropped. Each p99 comes from only 60 measured commits and is effectively the maximum sample. These measurements do not include consensus or networking and do not establish production capacity. Source manifests, build logs, raw cycles, root traces, table counts and digests are retained under `/home/ubuntu/node-id-stale-bench` and the local `work/node-id-stale-bench` directory. The harness lockfile was resolved offline against #60 before measurement and is retained.
