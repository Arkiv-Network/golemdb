# Builds the load benchmarks (bench/README.md)
[group('bench')]
bench_build:
  cargo build --release -q -p golemdb-bench


# Runs one load benchmark scenario: a name under bench/scenarios, or a path to a .toml file
[group('bench')]
bench scenario out=("target/bench-out/" + file_stem(scenario)) db_dir="target/bench-db": bench_build
  #!/bin/sh
  set -eu
  scenario={{quote(scenario)}}
  [ -f "$scenario" ] || scenario=bench/scenarios/$scenario.toml
  [ -f "$scenario" ] || { echo "no such scenario: $scenario" >&2; exit 2; }
  target/release/golemdb-bench "$scenario" --out {{quote(out)}} --db-dir {{quote(db_dir)}}
