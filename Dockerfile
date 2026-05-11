# syntax=docker/dockerfile:1.7

# BIN selects which workspace binary this image ships. Defaults to the
# `mars` service binary; override with `--build-arg BIN=mars-operator` to
# produce the operator image. The Dockerfile is otherwise identical so
# both images go through the same build/cache plan.
ARG BIN=mars

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
ARG BIN

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
    cargo build --release --locked -p "${BIN}" \
    && cp "target/release/${BIN}" /usr/local/bin/app

FROM gcr.io/distroless/cc-debian12:nonroot

# proj-sys links sqlite dynamically; distroless/cc doesn't ship it
COPY --from=builder /usr/lib/x86_64-linux-gnu/libsqlite3.so.0 \
                    /usr/lib/x86_64-linux-gnu/libsqlite3.so.0
# Final stage uses a single fixed entrypoint path so ENTRYPOINT (JSON exec
# form, no shell) does not need to resolve a build-arg at runtime. The
# binary inside is whichever `-p ${BIN}` was built above.
COPY --from=builder /usr/local/bin/app /usr/local/bin/app

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/app"]

# no image-level HEALTHCHECK: only runtime binds 8080, and orchestrators
# (compose, k8s) configure their own probes against /readyz or /healthz.
# the `mars healthcheck` subcommand stays available for those consumers.
