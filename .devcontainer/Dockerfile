# A light dev container for golemdb: the pinned Rust toolchain, lld and
# cargo-nextest. See devcontainer.json.
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# lld is the linker on aarch64; devcontainer.json explains why.
RUN apt-get update \
    && apt-get install -y --no-install-recommends clang libclang-dev lld \
    && rm -rf /var/lib/apt/lists/*

# Install the toolchain rust-toolchain.toml pins (channel and components) into
# the image, so the first `cargo` call in the container does not download it.
# rust-src is added on top: rust-analyzer needs the standard library's source
# to resolve std, and CI, which shares the toolchain file, has no use for it.
# Installing as root leaves the new toolchain in group root; hand it to the
# rustlang group the vscode user is in, so rustup works without sudo later.
COPY rust-toolchain.toml /tmp/toolchain/rust-toolchain.toml
RUN cd /tmp/toolchain \
    && rustup toolchain install \
    && rustup component add rust-src \
    && rm -rf /tmp/toolchain \
    && chgrp -R rustlang "$RUSTUP_HOME" "$CARGO_HOME" \
    && chmod -R g+w "$RUSTUP_HOME" "$CARGO_HOME"

# cargo-nextest, the test runner CI uses. The prebuilt binary, since building
# it from source adds minutes to every image build.
RUN case "$(dpkg --print-architecture)" in \
        amd64) platform=linux ;; \
        arm64) platform=linux-arm ;; \
        *) echo "unsupported architecture" >&2; exit 1 ;; \
    esac \
    && curl -LsSf "https://get.nexte.st/latest/$platform" | tar zxf - -C /usr/local/bin

RUN cargo install cargo-hack --locked
