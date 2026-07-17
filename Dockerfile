# Rust 1.94: turbomcp is edition 2024 and declares rust-version 1.89, so the previous 1.87 pin
# cannot build this crate at all. `git` is required now that turbomcp is a git dependency rather
# than a crates.io one — the -slim image does not ship it.
# MUST stay on -bookworm to match the debian:bookworm-slim runtime below. The bare `rust:1.94-slim`
# tag is trixie-based (glibc 2.38) and produces a binary that dies on bookworm (glibc 2.36) with
# "GLIBC_2.38 not found". Bump both stages together or not at all.
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev git && \
    cargo build --release && \
    strip target/release/liberado-actual-mcp

FROM debian:bookworm-slim
# curl is required by the compose healthcheck (`curl -s http://localhost:8000/`). Without it the
# healthcheck can never pass and the container sits permanently unhealthy.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --create-home --uid 1001 app
COPY --from=builder /build/target/release/liberado-actual-mcp /usr/local/bin/
ENV BIND_ADDR=0.0.0.0:8000
EXPOSE 8000
USER app
ENTRYPOINT ["liberado-actual-mcp"]
