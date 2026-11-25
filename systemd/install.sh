#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

USER_MODE=false
UNINSTALL=false
ENABLE_SERVICE=false
START_SERVICE=false
SKIP_HOST_DOWNLOAD=false
REBUILD=false
RUN_AS_USER=""
BIN_PATH=""
MEKUGI_REPO="mekugi-harness/mekugi"
MEKUGI_VERSION="v0.7.0"
GRPC_ADDR="127.0.0.1:50052"
HTTP_PORT="8080"
WORKDIR=""

usage() {
  cat <<'EOF'
Install or uninstall mekugi-devbox as a systemd service for direct VPS control.

Usage:
  sudo ./systemd/install.sh [options]       # System service (/etc/systemd/system)
  ./systemd/install.sh --user [options]     # User service (~/.config/systemd/user)

Options:
  --user                     Install as a systemd user service (no root required)
  --run-as <user>            System mode only: run the service as <user> via drop-in
  --bin <path>               Path to a prebuilt mekugi-devbox binary (skips cargo build)
  --rebuild                  Force rebuilding mekugi-devbox even if target/release exists
  --mekugi-version <tag>     Mekugi release tag for codex-code-mode-host (default: v0.7.0)
  --skip-host-download       Do not download codex-code-mode-host or extract docs
  --grpc-addr <host:port>    Initial DEVBOX_GRPC_ADDR in new env file (default: 127.0.0.1:50052)
  --http-port <port|"">      Initial DEVBOX_PORT in new env file (default: 8080; empty disables HTTP)
  --workdir <dir>            Initial DEVBOX_WORKDIR in new env file
  --enable                   Enable service on boot (default: off / on-demand only)
  --start                    Start service immediately after install
  --uninstall                Stop, disable, and remove the unit and installed binaries
  -h, --help                 Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) USER_MODE=true; shift ;;
    --run-as) RUN_AS_USER="${2:?--run-as requires a username}"; shift 2 ;;
    --bin) BIN_PATH="${2:?--bin requires a path}"; shift 2 ;;
    --rebuild) REBUILD=true; shift ;;
    --mekugi-version) MEKUGI_VERSION="${2:?--mekugi-version requires a tag}"; shift 2 ;;
    --skip-host-download) SKIP_HOST_DOWNLOAD=true; shift ;;
    --grpc-addr) GRPC_ADDR="${2:?--grpc-addr requires host:port}"; shift 2 ;;
    --http-port) HTTP_PORT="${2?--http-port requires a value}"; shift 2 ;;
    --workdir) WORKDIR="${2:?--workdir requires a directory}"; shift 2 ;;
    --enable) ENABLE_SERVICE=true; shift ;;
    --start) START_SERVICE=true; shift ;;
    --uninstall) UNINSTALL=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ "$USER_MODE" == true ]]; then
  if [[ -n "$RUN_AS_USER" ]]; then
    echo "Error: --run-as cannot be combined with --user" >&2
    exit 2
  fi
  BIN_DIR="${HOME}/.local/bin"
  SHARE_DIR="${HOME}/.local/share/mekugi"
  CONF_DIR="${HOME}/.config/mekugi-devbox"
  UNIT_DIR="${HOME}/.config/systemd/user"
  UNIT_SRC="${SCRIPT_DIR}/mekugi-devbox.user.service"
  SYSTEMCTL=(systemctl --user)
  DEFAULT_WORKDIR="${HOME}"
else
  if [[ "$(id -u)" -ne 0 ]]; then
    echo "Error: system install requires root (re-run with sudo, or pass --user)." >&2
    exit 1
  fi
  BIN_DIR="/usr/local/bin"
  SHARE_DIR="/usr/local/share/mekugi"
  CONF_DIR="/etc/mekugi-devbox"
  UNIT_DIR="/etc/systemd/system"
  UNIT_SRC="${SCRIPT_DIR}/mekugi-devbox.service"
  SYSTEMCTL=(systemctl)
  if [[ -n "$RUN_AS_USER" ]]; then
    if ! id "$RUN_AS_USER" >/dev/null 2>&1; then
      echo "Error: user '${RUN_AS_USER}' does not exist" >&2
      exit 1
    fi
    DEFAULT_WORKDIR="$(getent passwd "$RUN_AS_USER" | cut -d: -f6)"
  else
    DEFAULT_WORKDIR="/root"
  fi
fi

WORKDIR="${WORKDIR:-$DEFAULT_WORKDIR}"
ENV_FILE="${CONF_DIR}/env"
UNIT_DEST="${UNIT_DIR}/mekugi-devbox.service"
DROPIN_DIR="${UNIT_DIR}/mekugi-devbox.service.d"

if [[ "$UNINSTALL" == true ]]; then
  "${SYSTEMCTL[@]}" stop mekugi-devbox.service 2>/dev/null || true
  "${SYSTEMCTL[@]}" disable mekugi-devbox.service 2>/dev/null || true
  rm -f "$UNIT_DEST" "${DROPIN_DIR}/run-as.conf"
  rmdir "$DROPIN_DIR" 2>/dev/null || true
  "${SYSTEMCTL[@]}" daemon-reload 2>/dev/null || true
  rm -f "${BIN_DIR}/mekugi-devbox" "${BIN_DIR}/codex-code-mode-host"
  rm -f "${SHARE_DIR}/code-mode-docs.json" "${SHARE_DIR}/SOURCE_REVISION"
  echo "Uninstalled mekugi-devbox systemd unit and binaries."
  if [[ -f "$ENV_FILE" ]]; then
    echo "Preserved config file: ${ENV_FILE} (remove manually if no longer needed)."
  fi
  exit 0
fi

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo "amd64" ;;
    aarch64|arm64) echo "arm64" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
  esac
}

generate_token() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -base64 32 | tr -d '\n'
  else
    head -c 32 /dev/urandom | base64 | tr -d '\n'
  fi
}

# 1. Resolve or build mekugi-devbox binary
if [[ -z "$BIN_PATH" ]]; then
  DEFAULT_RELEASE_BIN="${REPO_ROOT}/target/release/mekugi-devbox"
  if [[ "$REBUILD" == true || ! -x "$DEFAULT_RELEASE_BIN" ]]; then
    CARGO_BIN="$(command -v cargo || true)"
    if [[ -z "$CARGO_BIN" && -n "${SUDO_USER:-}" ]]; then
      SUDO_HOME="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
      if [[ -x "${SUDO_HOME}/.cargo/bin/cargo" ]]; then
        CARGO_BIN="${SUDO_HOME}/.cargo/bin/cargo"
      fi
    fi
    if [[ -z "$CARGO_BIN" ]]; then
      echo "Error: cargo not found and --bin was not provided." >&2
      echo "Build with 'cargo build --release --locked -p mekugi-devbox' and pass --bin target/release/mekugi-devbox." >&2
      exit 1
    fi
    echo "Building mekugi-devbox release binary..."
    if [[ "$USER_MODE" == false && -n "${SUDO_USER:-}" ]]; then
      sudo -u "$SUDO_USER" "$CARGO_BIN" build --release --locked --manifest-path "${REPO_ROOT}/Cargo.toml" -p mekugi-devbox
    else
      "$CARGO_BIN" build --release --locked --manifest-path "${REPO_ROOT}/Cargo.toml" -p mekugi-devbox
    fi
  fi
  BIN_PATH="$DEFAULT_RELEASE_BIN"
fi

if [[ ! -x "$BIN_PATH" ]]; then
  echo "Error: mekugi-devbox binary is not executable: ${BIN_PATH}" >&2
  exit 1
fi

mkdir -p "$BIN_DIR" "$SHARE_DIR" "$CONF_DIR" "$UNIT_DIR"
install -m 0755 "$BIN_PATH" "${BIN_DIR}/mekugi-devbox"

# 2. Download codex-code-mode-host + extract code-mode docs (matches docker/devbox/Dockerfile)
if [[ "$SKIP_HOST_DOWNLOAD" == false ]]; then
  ARCH="$(detect_arch)"
  TAR_URL="https://github.com/${MEKUGI_REPO}/releases/download/${MEKUGI_VERSION}/mekugi-native_linux_${ARCH}.tar.gz"
  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TMP_DIR"' EXIT

  echo "Downloading codex-code-mode-host (${MEKUGI_VERSION}, linux/${ARCH})..."
  curl -fsSL "$TAR_URL" | tar -xz -C "$TMP_DIR" codex-code-mode-host SOURCE_REVISION
  install -m 0755 "${TMP_DIR}/codex-code-mode-host" "${BIN_DIR}/codex-code-mode-host"
  install -m 0644 "${TMP_DIR}/SOURCE_REVISION" "${SHARE_DIR}/SOURCE_REVISION"

  REV="$(tr -d '[:space:]' < "${SHARE_DIR}/SOURCE_REVISION")"
  DESC_URL="https://raw.githubusercontent.com/${MEKUGI_REPO}/${REV}/codex-rs/code-mode-protocol/src/description.rs"
  curl -fsSL "$DESC_URL" -o "${TMP_DIR}/description.rs"
  "${BIN_DIR}/mekugi-devbox" --extract-docs \
    --repo "$MEKUGI_REPO" \
    --ref "$REV" \
    --source-file "${TMP_DIR}/description.rs" \
    --output "${SHARE_DIR}/code-mode-docs.json"
fi

# 3. Create env file with chmod 600 if absent (never overwrite an existing config)
CREATED_ENV=false
TOKEN_VALUE=""
if [[ ! -f "$ENV_FILE" ]]; then
  CREATED_ENV=true
  TOKEN_VALUE="$(generate_token)"
  (
    umask 077
    cat > "$ENV_FILE" <<EOF
DEVBOX_INTERNAL_TOKEN=${TOKEN_VALUE}
DEVBOX_GRPC_ADDR=${GRPC_ADDR}
DEVBOX_HOST=127.0.0.1
DEVBOX_PORT=${HTTP_PORT}
DEVBOX_WORKDIR=${WORKDIR}
DEVBOX_SHELL=/bin/bash
CODE_MODE_HOST_BIN=${BIN_DIR}/codex-code-mode-host
CODE_MODE_HOST_TARGET=127.0.0.1:50051
CODE_MODE_DOCS_PATH=${SHARE_DIR}/code-mode-docs.json
MAX_CELL_YIELD_TIME_MS=45000
LOG_LEVEL=info
EOF
  )
  chmod 0600 "$ENV_FILE"
fi

# 4. Install systemd unit (and optional --run-as drop-in)
install -m 0644 "$UNIT_SRC" "$UNIT_DEST"
if [[ "$USER_MODE" == false && -n "$RUN_AS_USER" ]]; then
  RUN_AS_GROUP="$(id -gn "$RUN_AS_USER")"
  mkdir -p "$DROPIN_DIR"
  cat > "${DROPIN_DIR}/run-as.conf" <<EOF
[Service]
User=${RUN_AS_USER}
Group=${RUN_AS_GROUP}
EOF
  chown -R "${RUN_AS_USER}:${RUN_AS_GROUP}" "$CONF_DIR"
elif [[ "$USER_MODE" == false ]]; then
  rm -f "${DROPIN_DIR}/run-as.conf"
  rmdir "$DROPIN_DIR" 2>/dev/null || true
fi

"${SYSTEMCTL[@]}" daemon-reload

if [[ "$ENABLE_SERVICE" == true ]]; then
  "${SYSTEMCTL[@]}" enable mekugi-devbox.service
fi
if [[ "$START_SERVICE" == true ]]; then
  "${SYSTEMCTL[@]}" restart mekugi-devbox.service
fi

if [[ "$USER_MODE" == true ]]; then
  CTL_CMD="systemctl --user"
  JOURNAL_CMD="journalctl --user -u mekugi-devbox -f"
else
  CTL_CMD="sudo systemctl"
  JOURNAL_CMD="sudo journalctl -u mekugi-devbox -f"
fi

echo ""
echo "Installed mekugi-devbox systemd service:"
echo "  Unit:   ${UNIT_DEST}"
echo "  Config: ${ENV_FILE}"
echo ""
echo "Toggle agent access on demand:"
echo "  ${CTL_CMD} start mekugi-devbox    # allow agent control"
echo "  ${CTL_CMD} stop mekugi-devbox     # revoke access & terminate agent shells"
echo "  ${CTL_CMD} status mekugi-devbox"
echo "  ${JOURNAL_CMD}"
if [[ "$CREATED_ENV" == true ]]; then
  echo ""
  echo "Generated DEVBOX_INTERNAL_TOKEN in ${ENV_FILE} (chmod 600)."
  echo "Edit DEVBOX_GRPC_ADDR if you want to bind to a Tailscale/WireGuard IP instead of 127.0.0.1."
fi
