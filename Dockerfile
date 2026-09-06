# =========================
# Build
# =========================
FROM rust:bookworm AS builder

WORKDIR /build

COPY . .

RUN cargo build --release


# =========================
# Runtime
# =========================
FROM debian:bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/rolava /usr/local/bin/rolava
COPY --from=builder /build/web /app/web
COPY --from=builder /build/prompt /app/prompt

RUN chmod +x /usr/local/bin/rolava

ENTRYPOINT ["/usr/local/bin/rolava"]
