FROM ubuntu:24.04

ARG TARGETARCH
ARG MEKUGI_VERSION=v0.7.0
ARG MEKUGI_REPO=yusing/mekugi
ARG CODEX_REPO=yusing/codex

ENV DEBIAN_FRONTEND=noninteractive \
    PYTHONUNBUFFERED=1 \
    DEVBOX_WORKDIR=/workspace \
    DEVBOX_HOST=0.0.0.0 \
    DEVBOX_PORT=8080 \
    CODE_MODE_HOST_BIN=/usr/local/bin/codex-code-mode-host \
    CODE_MODE_HOST_TARGET=127.0.0.1:50051 \
    CODE_MODE_DOCS_PATH=/usr/local/share/mekugi/code-mode-docs.json

RUN apt-get update && apt-get install -y --no-install-recommends \
    bash \
    build-essential \
    ca-certificates \
    curl \
    fd-find \
    git \
    jq \
    openssh-client \
    procps \
    python3 \
    python3-pip \
    python3-venv \
    ripgrep \
    && rm -rf /var/lib/apt/lists/*

# Pull prebuilt codex-code-mode-host and its SOURCE_REVISION from Mekugi GitHub Release.
RUN set -eu; \
    mkdir -p /usr/local/share/mekugi; \
    url="https://github.com/${MEKUGI_REPO}/releases/download/${MEKUGI_VERSION}/mekugi-native_linux_${TARGETARCH}.tar.gz"; \
    curl -fsSL "$url" | tar -xz -C /usr/local/share/mekugi codex-code-mode-host SOURCE_REVISION; \
    mv /usr/local/share/mekugi/codex-code-mode-host /usr/local/bin/codex-code-mode-host; \
    chmod +x /usr/local/bin/codex-code-mode-host; \
    /usr/local/bin/codex-code-mode-host --help >/dev/null

WORKDIR /opt/mekugi-mcp
COPY pyproject.toml README.md ./
COPY src ./src

RUN python3 -m venv /opt/mekugi-mcp/.venv \
    && /opt/mekugi-mcp/.venv/bin/pip install --no-cache-dir . \
    && /opt/mekugi-mcp/.venv/bin/python -m mekugi_mcp.code_mode_docs \
        --repo "${CODEX_REPO}" \
        --ref-file /usr/local/share/mekugi/SOURCE_REVISION \
        --output /usr/local/share/mekugi/code-mode-docs.json

RUN mkdir -p /workspace
WORKDIR /workspace

EXPOSE 8080

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8080/health >/dev/null || exit 1

CMD ["/opt/mekugi-mcp/.venv/bin/mekugi-devbox"]
