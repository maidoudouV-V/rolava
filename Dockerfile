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
FROM node:24.20.0-bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates python3 \
    && ln -s /usr/bin/python3 /usr/local/bin/python \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

RUN npm install --no-save --package-lock=false --omit=dev --no-audit --no-fund \
        axios@1.20.0 protobufjs@8.8.0 qrcode@1.5.4 \
    && npm cache clean --force

COPY --from=builder /build/target/release/rolava /usr/local/bin/rolava
COPY --from=builder /build/web /app/web
COPY --from=builder /build/prompt /app/prompt
COPY --from=builder /build/skills /app/skills
COPY --from=builder /build/config/models.dev.json /app/config/models.dev.json

RUN chmod +x /usr/local/bin/rolava

ENTRYPOINT ["/usr/local/bin/rolava"]
