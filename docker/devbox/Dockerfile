FROM ubuntu:26.04

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG TARGETARCH
ARG MEKUGI_VERSION=v0.7.0
ARG MEKUGI_REPO=mekugi-harness/mekugi
ARG MEKUGI_ASSET_REPO=yusing/mekugi
ARG DOTFILES_TOOLS=agent,go,rust,browser

ENV DEBIAN_FRONTEND=noninteractive \
    PYTHONUNBUFFERED=1 \
    DEVBOX_WORKDIR=/workspace \
    DEVBOX_HOST=0.0.0.0 \
    DEVBOX_PORT=8080 \
    CODE_MODE_HOST_BIN=/usr/local/bin/codex-code-mode-host \
    CODE_MODE_HOST_TARGET=127.0.0.1:50051 \
    CODE_MODE_DOCS_PATH=/usr/local/share/mekugi/code-mode-docs.json

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    nodejs \
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
    url="https://github.com/${MEKUGI_ASSET_REPO}/releases/download/${MEKUGI_VERSION}/mekugi-native_linux_${TARGETARCH}.tar.gz"; \
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

RUN --mount=type=bind,source=dotfiles-repo.tar.gz,target=/tmp/dotfiles-repo.tar.gz \
    --mount=type=secret,id=GITHUB_TOKEN \
    set -eu; \
    if [ -s /run/secrets/GITHUB_TOKEN ]; then export GITHUB_TOKEN="$(cat /run/secrets/GITHUB_TOKEN)"; fi; \
    repo="$(mktemp -d)"; \
    trap 'rm -rf "$repo"' EXIT; \
    tar -xzf /tmp/dotfiles-repo.tar.gz -C "$repo"; \
    git_bin=/usr/bin/git; \
    if [ ! -x "$git_bin" ]; then sudo apt-get update && sudo apt-get install -y --no-install-recommends git; fi; \
    "$git_bin" -C "$repo" checkout -q -- .; \
    sed -i "s|^REPO_URL=.*|REPO_URL=\"$repo\"|" "$repo/setup.sh"; \
    bash "$repo/setup.sh" --tools="${DOTFILES_TOOLS}"; \
    "$git_bin" remote set-url origin https://github.com/yusing/agentic-dotfiles.git; \
    sudo rm -rf /var/lib/apt/lists/*

USER root
WORKDIR /opt/mekugi-mcp
COPY pyproject.toml README.md ./
COPY src ./src

RUN /usr/bin/python3 -m venv /opt/mekugi-mcp/.venv \
    && /opt/mekugi-mcp/.venv/bin/pip install --no-cache-dir . \
    && /opt/mekugi-mcp/.venv/bin/python -m mekugi_mcp.code_mode_docs \
        --repo "${MEKUGI_REPO}" \
        --ref-file /usr/local/share/mekugi/SOURCE_REVISION \
        --output /usr/local/share/mekugi/code-mode-docs.json

USER ubuntu
WORKDIR /workspace

EXPOSE 8080

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8080/health >/dev/null || exit 1

CMD ["/opt/mekugi-mcp/.venv/bin/mekugi-devbox"]
