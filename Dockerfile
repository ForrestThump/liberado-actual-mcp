FROM rust:1.87-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && \
    cargo build --release && \
    strip target/release/liberado-actual-mcp

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/liberado-actual-mcp /usr/local/bin/
ENV BIND_ADDR=0.0.0.0:8000
EXPOSE 8000
ENTRYPOINT ["liberado-actual-mcp"]
