# syntax=docker/dockerfile:1.7

FROM rust:1.95.0-bookworm AS chef

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo install cargo-chef --locked

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

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY bin ./bin
COPY crates ./crates

# cargo's output dir is a cache mount and not part of the layer; copy the
# binary out before the RUN exits so the next stage can COPY --from=builder.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p mars \
    && cp target/release/mars /usr/local/bin/mars

FROM gcr.io/distroless/cc-debian12:nonroot

# proj-sys links sqlite dynamically; distroless/cc doesn't ship it
COPY --from=builder /usr/lib/x86_64-linux-gnu/libsqlite3.so.0 \
                    /usr/lib/x86_64-linux-gnu/libsqlite3.so.0
COPY --from=builder /usr/local/bin/mars /usr/local/bin/mars

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/mars"]

# no image-level HEALTHCHECK: only runtime binds 8080, and orchestrators
# (compose, k8s) configure their own probes against /readyz or /healthz.
# the `mars healthcheck` subcommand stays available for those consumers.
