# Build from the workspace root:
#   docker build -t rustversebot:local .
FROM rust:1.85-bookworm AS builder

WORKDIR /workspace

COPY . .

RUN cargo build \
    --locked \
    --release \
    --package rustversebot

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 rustversebot \
    && useradd --system --uid 10001 --gid rustversebot \
        --home-dir /var/lib/rustversebot --shell /usr/sbin/nologin rustversebot \
    && install -d -o rustversebot -g rustversebot -m 0700 /var/lib/rustversebot \
    && install -d -o rustversebot -g rustversebot -m 0700 /var/lib/rustversebot/image

COPY --from=builder /workspace/target/release/rustversebot /usr/local/bin/rustversebot
COPY --chown=root:root config.toml /etc/rustversebot/config.toml

USER rustversebot
WORKDIR /var/lib/rustversebot

ENV BOT_CONFIG_PATH=/etc/rustversebot/config.toml \
    BOT_WEB_BIND=0.0.0.0:8080 \
    IMAGE_CACHE_DIR=/var/lib/rustversebot/image

EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--show-error", "http://127.0.0.1:8080/healthz"]

STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/rustversebot"]
