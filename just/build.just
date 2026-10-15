
# Builds all artifacts
[group('build')]
build:
  cargo build --workspace --all-targets --all-features


# Build benches
[group('build')]
build_bench:
  cargo bench --workspace --all-features --no-run
