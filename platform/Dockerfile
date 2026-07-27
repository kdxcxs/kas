# syntax=docker/dockerfile:1.7

FROM node:22-bookworm-slim AS node-runtime

FROM rust:1.88-bookworm AS builder

COPY --from=node-runtime /usr/local/ /usr/local/

RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --release \
    -p kas-api \
    -p kas-admin \
    -p kas-builtin-driver \
    -p kas-migrate

RUN KAS_PLATFORM_PROFILE=release \
    CARGO_TARGET_DIR=/src/platform/target \
    ./platform/scripts/build-packages.sh /out/packages

RUN ./platform/scripts/build-workspace-plugin.sh \
      /src/platform/frontend/dist \
      /out/plugins/workspace.zip \
    && ./platform/scripts/build-frontend-plugin.sh \
      /src/platform/plugins/registry \
      /out/plugins/registry.zip

FROM node:22-bookworm-slim AS runtime

ARG CODEX_VERSION=latest

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq tini \
    && npm install --global "@openai/codex@${CODEX_VERSION}" \
    && npm cache clean --force \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system kas \
    && useradd --system --gid kas --home-dir /var/lib/kas --shell /usr/sbin/nologin kas \
    && mkdir -p /opt/kas/bin /opt/kas/packages /opt/kas/plugins /opt/kas/scripts /var/lib/kas \
    && chown -R kas:kas /var/lib/kas

COPY --from=builder /src/target/release/kas-api /opt/kas/bin/kas-api
COPY --from=builder /src/target/release/kas-admin /opt/kas/bin/kas-admin
COPY --from=builder /src/target/release/kas-builtin-driver /opt/kas/bin/kas-builtin-driver
COPY --from=builder /src/target/release/kas-migrate /opt/kas/bin/kas-migrate
COPY --from=builder /out/packages/ /opt/kas/packages/
COPY --from=builder /out/plugins/ /opt/kas/plugins/
COPY platform/scripts/install-frontend-plugin.sh /opt/kas/scripts/install-frontend-plugin.sh
COPY platform/docker-entrypoint.sh /opt/kas/bin/docker-entrypoint.sh

RUN chmod 755 \
    /opt/kas/bin/kas-api \
    /opt/kas/bin/kas-admin \
    /opt/kas/bin/kas-builtin-driver \
    /opt/kas/bin/kas-migrate \
    /opt/kas/bin/docker-entrypoint.sh \
    /opt/kas/scripts/install-frontend-plugin.sh

ENV PATH="/opt/kas/bin:${PATH}" \
    KAS_DATA_DIR="/var/lib/kas" \
    KAS_DATABASE="/var/lib/kas/kas.db" \
    KAS_ADDRESS="0.0.0.0:3000" \
    KAS_API_URL="http://127.0.0.1:3000" \
    KAS_FILE_ADDRESS="0.0.0.0:3001" \
    KAS_FILE_API="http://127.0.0.1:3001" \
    KAS_FILE_API_URL="http://127.0.0.1:3001" \
    KAS_SKILL_ADDRESS="0.0.0.0:3002" \
    KAS_SKILL_API="http://127.0.0.1:3002" \
    KAS_SKILL_API_URL="http://127.0.0.1:3002" \
    KAS_APPROVAL_ADDRESS="0.0.0.0:3003" \
    KAS_APPROVAL_API="http://127.0.0.1:3003" \
    KAS_APPROVAL_API_URL="http://127.0.0.1:3003" \
    KAS_FRONTEND_ADDRESS="0.0.0.0:5173" \
    KAS_CODEX_HOME="/var/lib/kas/codex-home"

VOLUME ["/var/lib/kas"]
EXPOSE 3000 3001 3002 3003 5173

HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=6 \
  CMD curl --fail --silent http://127.0.0.1:5173/health >/dev/null || exit 1

USER kas
WORKDIR /var/lib/kas

ENTRYPOINT ["/usr/bin/tini", "--", "/opt/kas/bin/docker-entrypoint.sh"]
