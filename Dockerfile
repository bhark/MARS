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

FROM gcr.io/distroless/cc-debian12:nonroot

# proj-sys links sqlite dynamically; distroless/cc doesn't ship it
COPY --from=builder /usr/lib/x86_64-linux-gnu/libsqlite3.so.0 \
                    /usr/lib/x86_64-linux-gnu/libsqlite3.so.0
COPY --from=builder /src/target/release/mars /usr/local/bin/mars

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/mars"]
