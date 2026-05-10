FROM rust:1.95.0-bookworm AS build

WORKDIR /src

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

COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY bin ./bin
COPY crates ./crates

RUN cargo build --release --locked -p mars

FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=build /src/target/release/mars /usr/local/bin/mars

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/mars"]
