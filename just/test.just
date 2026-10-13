
# Run all tests
[group('test')]
test test_name="":
  cargo test {{test_name}} --workspace --all-features --doc 


# Run all tests
[group('test')]
nextest test_name="":
  cargo nextest run {{test_name}} --workspace --all-features 


# Run all benches
[group('test')]
bench bench_name="":
  cargo bench --workspace --all-features --no-run