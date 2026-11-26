# mekugi-mcp

Python **FastMCP** server backed by a **Rust HTTP/2 gRPC clustering transport** (`crates/mekugi-transport`, compiled via PyO3/maturin) exposing Mekugi's `functions.exec` (V8 code-mode via prebuilt `codex-code-mode-host`), `wait`, and `list_boxes` across one or more persistent devboxes, secured with **Google Cloud OAuth** and exposed over **Cloudflare Tunnel** for **Gemini Web**.

## Architecture

```
Gemini Web ──HTTPS──▶ cloudflared ──HTTP──▶ mcp (FastMCP :8000)
                                             ├─ GoogleProvider (OAuth Proxy) + ALLOWED_EMAILS
                                             ├─ Gemini Web schema sanitizer
                                             └─ Rust transport (mekugi_mcp._transport, Tokio + Tonic)
                                                  ├─ Lock-free per-box registry & stateless <box>:<cell_id> routing
                                                  ├─ Persistent HTTP/2 gRPC keep-alive channels (optional pool_size)
                                                  ├─ HTTP/2+HTTP/1.1 keep-alive fallback (reqwest)
                                                  └─ Parallel bounded health/info probes for list_boxes & /health
                                                       │
                                                       ├── gRPC/H2 (:50052) ──▶ devbox-local ──gRPC──▶ codex-code-mode-host
                                                       ├── gRPC/H2 (:50052) ──▶ devbox-gpu1  ──gRPC──▶ codex-code-mode-host
                                                       └── gRPC/H2 (:50052) ──▶ devbox-arm64 ──gRPC──▶ codex-code-mode-host
```

- **Zero-contention keep-alive transport**: Each configured devbox owns an isolated Rust `tonic` HTTP/2 channel (or `pool_size` round-robin channels) with L7 HTTP/2 keep-alive pings (`20s` interval / `5s` timeout, `keep_alive_while_idle=true`), adaptive flow control, and `TCP_NODELAY`. Concurrent `exec`, `wait`, and `list_boxes` calls multiplex on independent HTTP/2 streams so one slow command never blocks others.
- **Stateless cell routing**: Yielded `cell_id`s are namespaced as `<box>:<raw_cell_id>` by the Rust transport, so `wait` automatically routes to the right machine even across MCP container restarts.
- **Tools**: Annotated with `readOnlyHint=True` by default (`NO_CONFIRMATION_GATE=true`) so Gemini Web does not prompt for approval on every call.
- `yield_time_ms` is capped at `45000` ms to stay within Gemini Web's ~60s tool timeout; longer scripts yield a `cell_id` for `wait`.

## Setup

### 1. Google Cloud OAuth
1. In Google Cloud Console → **APIs & Services → Credentials**, create an **OAuth client ID** (**Web application**).
2. Add the authorized redirect URI:
   ```
   https://<your-domain>/auth/callback
   ```
3. Copy the Client ID and Client Secret.

### 2. Cloudflare Tunnel
1. In Cloudflare Zero Trust → **Networks → Tunnels**, create a tunnel and copy its token.
2. Add a Public Hostname routing `https://<your-domain>` to `http://mcp:8000`.

### 3. Configure and start
```bash
cp .env.example .env
# Edit .env with BASE_URL, GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET,
# JWT_SIGNING_KEY, ALLOWED_EMAILS, and CLOUDFLARE_TUNNEL_TOKEN

docker compose up -d --build
```

Verify health:
```bash
curl -fsS https://<your-domain>/health
```

### Multi-Box Cluster Configuration

By default (when neither `DEVBOXES_JSON` nor `DEVBOXES_FILE` is set), `mcp` connects to `DEVBOX_URL` (default `grpc://devbox:50052`) as `DEFAULT_BOX` (default `local`).

To connect multiple devboxes from one MCP server, set `DEVBOXES_JSON` or mount a JSON file at `DEVBOXES_FILE`:

```json
{
  "default_box": "local",
  "boxes": [
    {
      "name": "local",
      "target": "grpc://devbox:50052",
      "description": "Primary Ubuntu devbox (Docker)",
      "labels": { "arch": "amd64", "role": "general" }
    },
    {
      "name": "gpu-1",
      "target": "grpcs://gpu-1.tailnet.ts.net:50052",
      "token_env": "DEVBOX_TOKEN_GPU1",
      "pool_size": 2,
      "description": "Remote GPU box",
      "labels": { "arch": "amd64", "gpu": "rtx4090" }
    }
  ]
}
```

Supported target schemes: `grpc://`, `grpcs://` (native TLS + optional `ca_cert_path`), `http://`, and `https://`.

### Running on a VPS with systemd (On-Demand Direct Host Control)

Instead of running `mekugi-devbox` in Docker, you can install it directly on a Linux VPS as a **systemd** service (`Type=notify` with `sd_notify` readiness, watchdog heartbeat, and `KillMode=control-group` so stopping the unit cleanly terminates all agent shells).

By default the installer leaves the service **disabled at boot** so you can toggle direct agent control on and off whenever you want:

```bash
# System service (runs as root by default, or pass --run-as <user>)
sudo ./systemd/install.sh

# Or user-level systemd service (no root required)
./systemd/install.sh --user
```

The installer builds `mekugi-devbox` (or pass `--bin <path>`), downloads `codex-code-mode-host` and extracts `code-mode-docs.json`, creates a `chmod 600` env file with a generated `DEVBOX_INTERNAL_TOKEN`, and installs the unit file.

Toggle access on demand:

```bash
sudo systemctl start mekugi-devbox    # allow agent control
sudo systemctl stop mekugi-devbox     # revoke access and reap all agent shells
sudo systemctl status mekugi-devbox
sudo journalctl -u mekugi-devbox -f
```

(Pass `--enable` to `install.sh` or run `sudo systemctl enable --now mekugi-devbox` if you want it to start on boot.)

Then add the VPS to your MCP server's `DEVBOXES_JSON` (bind `DEVBOX_GRPC_ADDR` in `/etc/mekugi-devbox/env` to your Tailscale/WireGuard IP or `127.0.0.1:50052` behind an SSH tunnel — avoid exposing `0.0.0.0` on a public VPS):

```json
{
  "name": "vps",
  "target": "grpc://100.x.y.z:50052",
  "token_env": "DEVBOX_TOKEN_VPS",
  "description": "Direct VPS host (systemd on-demand)"
}
```

To remove the unit and installed binaries later, run `sudo ./systemd/install.sh --uninstall` (or add `--user`).

To use Plane in the devbox, install a container-compatible `plane` binary at `~/.local/bin/plane` and the full skill at `~/.skills-mgr/skills/plane/SKILL.md` on the Docker host, then add the Compose override:
```bash
docker compose -f docker-compose.yml -f docker/devbox/compose.plane.yml \
  up -d --build
```

### 4. Connect in Gemini Web
1. Open `gemini.google.com` → **Settings → Connected Apps → Custom apps**.
2. Add your MCP endpoint:
   ```
   https://<your-domain>/mcp
   ```
3. Complete the Google OAuth flow.

## Exposed MCP Tools

- **`exec(code, box=None, yield_time_ms=10000, max_output_tokens=10000)`**: Runs raw JavaScript in Mekugi's V8 isolate (`codex-code-mode-host`) on the target devbox (defaults to `DEFAULT_BOX`), with nested `tools.exec_command` (supporting both shell `cmd` and shell-free `argv` modes, plus optional `stdin` and `env`), `tools.write_stdin`, and `tools.apply_patch`, plus `// @exec: {...}` pragmas and `text()`/`store()`/`load()`/`exit()`/`yield_control()`.
- **`wait(cell_id, box=None, yield_time_ms=10000, max_output_tokens=10000, terminate=false)`**: Polls or terminates a yielded `exec` cell. Namespaced `<box>:<cell_id>` IDs route automatically.
- **`list_boxes()`**: Lists all configured devboxes with default flag, transport, live status/latency, host metadata (`hostname`, `os`, `arch`, `workdir`), labels, description, and active cell/shell counts.

## Local Development & Tests

```bash
# Rust transport unit + HTTP/2 multiplexing tests & clippy
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# Build Python extension and run Python unit + e2e tests
python3 -m venv .venv
.venv/bin/pip install -e ".[dev]"
.venv/bin/maturin develop
.venv/bin/ruff check src tests
.venv/bin/pytest
```
