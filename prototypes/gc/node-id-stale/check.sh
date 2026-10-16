#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
unset GOLEMDB_LAYOUT GOLEMDB_GC GOLEMDB_GC_W GOLEMDB_GC_CHUNKS
if [[ ! -f crates/merkle/src/physical.rs ]]; then
    git apply --check prototypes/gc/node-id-stale.patch
    git apply prototypes/gc/node-id-stale.patch
fi
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
if [[ ! -e prototypes/gc/node-id-stale/harness/golem-db ]]; then
    ln -s ../../../.. prototypes/gc/node-id-stale/harness/golem-db
fi
cargo check --locked --manifest-path prototypes/gc/node-id-stale/harness/Cargo.toml
cargo test --workspace --all-features --lib --tests
cargo test --workspace --all-features --doc
for layout in version-path node-id; do
    for window in 0 8 64; do
        GOLEMDB_LAYOUT="$layout" GOLEMDB_GC=stale GOLEMDB_GC_W="$window" \
            cargo test -p golemdb-integration-tests --test gcproto
    done
done
cargo bench --workspace --all-features --no-run
