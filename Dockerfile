# Rust 1.94: turbomcp is edition 2024 and declares rust-version 1.89, so the previous 1.87 pin
# cannot build this crate at all. `git` is required now that turbomcp is a git dependency rather
# than a crates.io one — the -slim image does not ship it.
FROM rust:1.94-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev git && \
    cargo build --release && \
    strip target/release/liberado-actual-mcp

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --create-home --uid 1001 app
COPY --from=builder /build/target/release/liberado-actual-mcp /usr/local/bin/
ENV BIND_ADDR=0.0.0.0:8000
EXPOSE 8000
USER app
ENTRYPOINT ["liberado-actual-mcp"]
