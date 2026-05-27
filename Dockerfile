FROM rust:1.79-slim AS builder
WORKDIR /app

# Copy manifest files first for dependency layer caching.
COPY Cargo.toml Cargo.lock* ./

# Build a dummy binary to cache compiled dependencies.
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Copy actual source code and rebuild.
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ── Runtime stage ───────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/hls2dash /usr/local/bin/hls2dash

EXPOSE 3000

ENV PORT=3000 \
    LOG_LEVEL=info \
    PROXY_BASE="" \
    CACHE_MAX_CAPACITY=500 \
    UPSTREAM_TIMEOUT_SECS=15

ENTRYPOINT ["hls2dash"]
