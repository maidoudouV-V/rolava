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
    && apt-get install -y --no-install-recommends ca-certificates python3 python3-venv \
    && python3 -m venv /opt/yt-dlp \
    && /opt/yt-dlp/bin/pip install --no-cache-dir yt-dlp==2026.08.19 \
    && ln -s /opt/yt-dlp/bin/yt-dlp /usr/local/bin/yt-dlp \
    && yt-dlp --version \
    && rm -rf /var/lib/apt/lists/*

ENV PATH="/opt/yt-dlp/bin:${PATH}"

WORKDIR /app

# 安装到项目根的 node_modules，/app/skills 下的脚本可直接加载 axios。
RUN npm install --no-save --package-lock=false --omit=dev --no-audit --no-fund axios@1.20.0 \
    && npm cache clean --force

COPY --from=builder /build/target/release/rolava /usr/local/bin/rolava
COPY --from=builder /build/web /app/web
COPY --from=builder /build/prompt /app/prompt
COPY --from=builder /build/skills /app/skills

RUN chmod +x /usr/local/bin/rolava

ENTRYPOINT ["/usr/local/bin/rolava"]
