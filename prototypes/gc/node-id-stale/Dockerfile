FROM rust:1.97-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends clang libclang-dev && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt clippy
