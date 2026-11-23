# Multi-stage Dockerfile for Google Cloud Run deployment
FROM rust:1.85-slim-bookworm AS builder

# HiGHS (MILP / simplex LP backends) is built from source: CMake, a C++ compiler and libclang.
# For a pure-Rust image (Clarabel only) drop this block and build with --no-default-features.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy manifest and build dependencies first for layer caching
COPY Cargo.toml ./
COPY src ./src

RUN cargo build --release --bin scm-lp-server --bin scm-lp-cli

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/scm-lp-server /usr/local/bin/scm-lp-server
COPY --from=builder /app/target/release/scm-lp-cli /usr/local/bin/scm-lp-cli

ENV PORT=8080
EXPOSE 8080

CMD ["scm-lp-server"]
