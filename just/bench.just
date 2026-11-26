# Runs integration benchmark suites (write, query, index)
[group('bench')]
[arg("args", long)]
bench suites="write query" args="":
  cargo bench -p golemdb-integration-tests {{prepend("--bench ", suites)}} {{args}}

# Builds the load benchmarks (crates/bench/README.md)
[group('bench')]
load_bench_build:
  cargo build --release -q -p golemdb-bench

# Runs one load benchmark scenario: a name under crates/bench/scenarios, or a path to a .toml file
[group('bench')]
load_bench scenario out=("target/bench-out/" + file_stem(scenario)) db_dir="target/bench-db": load_bench_build
  #!/bin/sh
  set -eu
  scenario={{quote(scenario)}}
  [ -f "$scenario" ] || scenario=crates/bench/scenarios/$scenario.toml
  [ -f "$scenario" ] || { echo "no such scenario: $scenario" >&2; exit 2; }
  target/release/golemdb-bench "$scenario" --out {{quote(out)}} --db-dir {{quote(db_dir)}}
