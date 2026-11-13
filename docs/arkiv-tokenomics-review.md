# Arkiv Tokenomics Review

Two findings remain open, and both are settled by the same compatibility spike. The first can
still change the fee design's structure and must be resolved before implementation. Reviewed:
[`arkiv-tokenomics.md`](arkiv-tokenomics.md) (draft as of 2026-10-15) and
[`golemdb-metering.md`](golemdb-metering.md) (draft, 2026-10-09).

Example numbers assume: token $0.12, 2 s blocks, the 1 KiB entity of the tokenomics worked example (1,556 storage units),
`storage_price` 650,000 wei per unit per block, `min_base_fee` 1 gwei.

## Findings at a Glance

| ID | Finding | Priority | Status |
| --- | --- | --- | --- |
| F1 | Hybrid storage gas is unverified on reth and wallets | P0 blocks implementation | Spike defined, not run |
| F2 | Header and receipt `gasUsed` disagree (O7) | P2 clean-up | Decided in step 6 of the F1 spike |

## P0: Blocks Implementation

A finding here changes a data structure, a formula or a consensus rule. 

### F1. Hybrid storage gas is unverified on reth and wallets

The hybrid's main argument is "wallets work unchanged". At the floor, storage gas is large:

- One 1 KiB entity is 3.9M storage gas for 3 months and 7.9M for 6 months.
- A batch of four 6-month entities exceeds a 30M compute block `gasLimit`.

Much of the stack is expected to assume `tx.gasLimit ≤ block.gasLimit`. Each of these needs
checking or patching; none is verified yet (**TODO (verify)**, by the spike):

- reth's transaction pool validator is expected to reject such transactions.
- The `eth_estimateGas` binary search and the `eth_call` gas cap are expected to stop at the
  block limit.
- Wallets, MetaMask especially, may warn about such a limit or cap it.

`min_base_fee` is the only lever on the size: a 10 gwei floor shrinks storage gas 10× but
raises the compute floor 10×.

**Fix:** a spike before committing, timeboxed to 2–3 days. Each step records pass or fail and
the patch it would need:

1. **reth transaction pool.** Submit a transaction whose `gasLimit` exceeds the block
   `gasLimit`. Find where it is rejected and what patching that takes.
2. **reth payload builder.** Its loop is expected to skip a transaction when the cumulative gas
   plus its `gasLimit` exceeds the block `gasLimit`. Replace that with the compute pre-check and actual
   compute.
3. **RPC.** Check the `eth_estimateGas` binary search and the `--rpc.gascap` limit, and how
   Arkiv's estimate replaces them.
4. **MetaMask.** Send transactions with a `gasLimit` of 20M (below a 30M block `gasLimit`) and
   60M (above it). Record warnings, caps and refusals, and whether MetaMask's own buffer on the
   estimate pushes the limit past the block's.
5. **viem.** Check `estimateGas` and `sendTransaction` with large limits, and a receipt formatter
   for the new fields.
6. **Header and receipt gas fields** (F2). Check reth's validation that the header's `gasUsed`
   equals the last receipt's `cumulativeGasUsed`, and how its RPC derives a receipt's `gasUsed`.
   Pick the cheapest of F2's options.

The result decides between three options: keep the hybrid and raise the floor, keep the hybrid
and patch reth, or switch to the EIP-4844-style split, which is cheaper to do now than later.

## P2: Clean-up Before Spec Freeze

### F2. Header and receipt `gasUsed` disagree (O7)

Header `gasUsed` counts compute only, while receipt `gasUsed` is the total. This is more than a
tooling question:

- **Consensus rule.** In Ethereum, the header's `gasUsed` must equal the last receipt's
  `cumulativeGasUsed`, and `cumulativeGasUsed` is part of the receipts root. **TODO (verify):**
  where reth checks it.
- **RPC derivation.** The RPC derives each receipt's `gasUsed` as the difference between
  consecutive `cumulativeGasUsed` values. **TODO (verify):** reth's RPC code path.

| Option | Header `gasUsed` | Receipts' `cumulativeGasUsed` | Patching needed |
| --- | --- | --- | --- |
| a | compute | cumulative compute | RPC: receipt `gasUsed` = compute + `storageGas` + `purgeGas` |
| b | total | cumulative total | base fee update and block limit read a new header field `computeGasUsed`; `eth_feeHistory` |
| c | compute, plus `storageGasUsed` and `purgeGasUsed` in the header | cumulative compute | as (a); totals reconcile for indexers |

Which is cheapest depends on reth internals, so the choice is step 6 of the F1 spike and closes
with it.
