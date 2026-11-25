# Runs integration benchmark suites (write, query, index)
[group('bench')]
[arg("args", long)]
bench suites="write query" args="":
  cargo bench -p golemdb-integration-tests {{prepend("--bench ", suites)}} {{args}}
