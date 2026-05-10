FROM rust:1.95.0-bookworm AS chef

RUN cargo install cargo-chef --locked

WORKDIR /src

FROM chef AS planner

COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY bin ./bin
COPY crates ./crates

RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      clang \
      cmake \
      libsqlite3-dev \
      pkg-config \
      sqlite3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=planner /src/recipe.json recipe.json

RUN cargo chef cook --release --recipe-path recipe.json

COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY bin ./bin
COPY crates ./crates

RUN cargo build --release --locked -p mars

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/mars /usr/local/bin/mars
COPY docker/entrypoint.sh /usr/local/bin/mars-entrypoint
RUN chmod +x /usr/local/bin/mars-entrypoint

# entrypoint starts as root, chowns MARS_DATA_DIRS, drops to 65532 via
# setpriv. do NOT set USER here - that would defeat the bootstrap.
ENTRYPOINT ["/usr/local/bin/mars-entrypoint"]
