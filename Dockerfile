# syntax=docker/dockerfile:1.6
FROM rust:1.94 AS builder
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*

# Cache cargo dependencies separately from source so a source change does
# not re-fetch + recompile the entire dep tree (saves ~5-10 min per rebuild).
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ ./proto/
COPY migrations/ ./migrations/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    mkdir src && echo "" > src/lib.rs \
    && cargo build --release --lib \
    && rm -rf src

# Now copy the real source and rebuild — only the crate recompiles.
COPY src/ ./src/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    touch src/main.rs src/lib.rs && cargo build --release \
    && cp target/release/ledger-accounting /ledger-accounting

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends wget ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /ledger-accounting /server
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD wget -qO- http://localhost:8080/healthz || exit 1
ENTRYPOINT ["/server"]