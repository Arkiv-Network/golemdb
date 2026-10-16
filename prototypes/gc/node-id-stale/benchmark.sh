#!/usr/bin/env bash
set -euo pipefail
REPO=$(git rev-parse --show-toplevel)
cd "$REPO"
HERE=prototypes/gc/node-id-stale
test -f crates/merkle/src/physical.rs || { echo 'Apply node-id-stale.patch first.' >&2; exit 1; }
if [[ ! -e "$HERE/harness/golem-db" ]]; then ln -s ../../../.. "$HERE/harness/golem-db"; fi
test "$(cd "$HERE/harness/golem-db" && pwd -P)" = "$REPO"
RESULTS=${BENCH_RESULTS:-target/node-id-stale-$(date +%Y%m%d-%H%M%S)}
[[ "$RESULTS" != /* ]] || { echo 'BENCH_RESULTS must be relative to the checkout.' >&2; exit 1; }
IMAGE=${BENCH_IMAGE:-golemdb-bench:1.97}
CPUS=${BENCH_CPUS:-28-31}
MEMORY=${BENCH_MEMORY:-24g}
CACHE=${BENCH_CARGO_HOME:-$HOME/.cargo}
mkdir -p "$RESULTS" "$CACHE"
TOOLCHAIN=$(docker run --rm "$IMAGE" rustup default | cut -d' ' -f1)
docker run --rm --cpuset-cpus "$CPUS" --memory "$MEMORY" --user "$(id -u):$(id -g)" \
    -v "$REPO:/work" -v "$CACHE:/cargo" -e CARGO_HOME=/cargo \
    -e RUSTUP_TOOLCHAIN="$TOOLCHAIN" -e CARGO_TARGET_DIR="/work/$RESULTS/target" \
    -w "/work/$HERE/harness" "$IMAGE" cargo build --locked --release --bin golemdb-harness
for round in 1 2 3; do
    for window in 0 8 64; do
        order=(version-path node-id)
        if [[ "$round" == 2 ]]; then order=(node-id version-path); fi
        for layout in "${order[@]}"; do
            dest="$RESULTS/out/w$window-r$round-$layout"
            test ! -e "$dest"
            mkdir -p "$dest"
            nice -n 10 docker run --rm --cpuset-cpus "$CPUS" --memory "$MEMORY" --user "$(id -u):$(id -g)" \
                -v "$REPO:/work" -v "$REPO/$dest:/work/$HERE/harness/results" \
                -e GOLEMDB_DB_DIR="/work/$RESULTS/db/$layout-$window" \
                -e GOLEMDB_LAYOUT="$layout" -e GOLEMDB_GC=stale -e GOLEMDB_GC_W="$window" \
                -w "/work/$HERE/harness" "$IMAGE" \
                bash -ec '"$1" "$2"; "$1" --reopen "$2"; "$1" --table-stats "$GOLEMDB_DB_DIR" > results/tables.csv' \
                bench "/work/$RESULTS/target/release/golemdb-harness" scenarios/steady-mixed-1000.toml \
                > "$dest/stdout.log" 2>&1
        done
    done
done
touch "$RESULTS/COMPLETE"
echo "Results: $RESULTS"
