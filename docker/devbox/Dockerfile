FROM ubuntu:26.04 AS wheel-builder

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    pkg-config \
    python3 \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

ENV PATH="/root/.cargo/bin:${PATH}"
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

WORKDIR /build
COPY Cargo.toml Cargo.lock pyproject.toml README.md ./
COPY proto ./proto
COPY crates ./crates
COPY src ./src

RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/target \
    python3 -m venv /tmp/build-venv \
    && /tmp/build-venv/bin/pip install --no-cache-dir "maturin>=1.7,<2.0" \
    && /tmp/build-venv/bin/maturin build --release --locked -i /usr/bin/python3 --out /wheels

FROM ubuntu:26.04

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG TARGETARCH
ARG MEKUGI_VERSION=v0.7.0
ARG MEKUGI_REPO=mekugi-harness/mekugi
ARG DOTFILES_SETUP_URL=https://raw.githubusercontent.com/yusing/agentic-dotfiles/main/setup.sh
ARG DOTFILES_TOOLS=agent,go,rust,browser

ENV DEBIAN_FRONTEND=noninteractive \
    PYTHONUNBUFFERED=1 \
    DEVBOX_WORKDIR=/workspace \
    DEVBOX_HOST=0.0.0.0 \
    DEVBOX_PORT=8080 \
    DEVBOX_GRPC_ADDR=0.0.0.0:50052 \
    CODE_MODE_HOST_BIN=/usr/local/bin/codex-code-mode-host \
    CODE_MODE_HOST_TARGET=127.0.0.1:50051 \
    CODE_MODE_DOCS_PATH=/usr/local/share/mekugi/code-mode-docs.json

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    python3-venv \
    sudo \
    && echo 'ubuntu ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/ubuntu \
    && chmod 0440 /etc/sudoers.d/ubuntu \
    && mkdir -p /workspace \
    && chown ubuntu:ubuntu /workspace \
    && rm -rf /var/lib/apt/lists/*

# Pull prebuilt codex-code-mode-host and its SOURCE_REVISION from Mekugi GitHub Release.
RUN set -eu; \
    mkdir -p /usr/local/share/mekugi; \
    url="https://github.com/${MEKUGI_REPO}/releases/download/${MEKUGI_VERSION}/mekugi-native_linux_${TARGETARCH}.tar.gz"; \
    curl -fsSL "$url" | tar -xz -C /usr/local/share/mekugi codex-code-mode-host SOURCE_REVISION; \
    mv /usr/local/share/mekugi/codex-code-mode-host /usr/local/bin/codex-code-mode-host; \
    chmod +x /usr/local/bin/codex-code-mode-host; \
    /usr/local/bin/codex-code-mode-host --help >/dev/null

# Bootstrap user environment and Linuxbrew as non-root user ubuntu.
USER ubuntu
ENV HOME=/home/ubuntu \
    AGENT_BROWSER_ENGINE=chrome \
    PATH="/home/ubuntu/.local/bin:/home/ubuntu/.local/share/mise/shims:/home/ubuntu/go/bin:/home/ubuntu/.bun/bin:/home/linuxbrew/.linuxbrew/bin:/home/linuxbrew/.linuxbrew/sbin:${PATH}"
WORKDIR /home/ubuntu

RUN --mount=type=secret,id=GITHUB_TOKEN \
    if [ -s /run/secrets/GITHUB_TOKEN ]; then export GITHUB_TOKEN="$(cat /run/secrets/GITHUB_TOKEN)"; fi \
    && curl -fsSL "${DOTFILES_SETUP_URL}" | bash -s -- --tools="${DOTFILES_TOOLS}" \
    && sudo rm -rf /var/lib/apt/lists/*

USER root
WORKDIR /opt/mekugi-mcp
COPY --from=wheel-builder /wheels/*.whl /tmp/wheels/

RUN /usr/bin/python3 -m venv /opt/mekugi-mcp/.venv \
    && /opt/mekugi-mcp/.venv/bin/pip install --no-cache-dir /tmp/wheels/*.whl \
    && rm -rf /tmp/wheels \
    && /opt/mekugi-mcp/.venv/bin/python -m mekugi_mcp.code_mode_docs \
        --repo "${MEKUGI_REPO}" \
        --ref-file /usr/local/share/mekugi/SOURCE_REVISION \
        --output /usr/local/share/mekugi/code-mode-docs.json

USER ubuntu
WORKDIR /workspace

EXPOSE 8080 50052

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8080/health >/dev/null || exit 1

CMD ["/opt/mekugi-mcp/.venv/bin/mekugi-devbox"]
