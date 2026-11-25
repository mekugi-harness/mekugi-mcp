FROM python:3.14-slim AS wheel-builder

ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    git \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

ENV PATH="/root/.cargo/bin:${PATH}"
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

WORKDIR /build
COPY Cargo.toml Cargo.lock pyproject.toml README.md ./
COPY proto ./proto
COPY crates ./crates
COPY src ./src

RUN --mount=type=cache,id=cargo-registry,target=/root/.cargo/registry \
    --mount=type=cache,id=cargo-git,target=/root/.cargo/git \
    --mount=type=cache,id=mcp-target-${TARGETARCH},target=/build/target \
    pip install --no-cache-dir "maturin>=1.7,<2.0" \
    && maturin build --release --locked --out /wheels

FROM python:3.14-slim

ENV PYTHONUNBUFFERED=1 \
    MCP_HOST=0.0.0.0 \
    MCP_PORT=8000 \
    OAUTH_STORAGE_DIR=/data/oauth

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=wheel-builder /wheels/*.whl /tmp/wheels/

RUN pip install --no-cache-dir /tmp/wheels/*.whl \
    && rm -rf /tmp/wheels \
    && mkdir -p /data/oauth

EXPOSE 8000

HEALTHCHECK --interval=15s --timeout=5s --start-period=10s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8000/health >/dev/null || exit 1

CMD ["mekugi-mcp"]
