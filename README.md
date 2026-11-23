# mekugi-mcp

Python **FastMCP** server exposing Mekugi's `functions.exec` (V8 code-mode via prebuilt `codex-code-mode-host`) and `wait` inside a persistent Docker devbox, secured with **Google Cloud OAuth** and exposed over **Cloudflare Tunnel** for **Gemini Web**.

## Architecture

```
Gemini Web ──HTTPS──▶ cloudflared ──HTTP──▶ mcp (FastMCP :8000) ──HTTP──▶ devbox (:8080)
                                             ├─ GoogleProvider (OAuth Proxy)  ├─ codex-code-mode-host (V8 gRPC)
                                             ├─ ALLOWED_EMAILS check          ├─ exec_command / write_stdin / apply_patch
                                             ├─ Encrypted FileTreeStore       └─ Persistent /workspace volume
                                             └─ Gemini Web schema sanitizer
```

- All MCP sessions share one persistent `devbox` container (`/workspace`).
- Tools are annotated with `readOnlyHint=True` by default (`NO_CONFIRMATION_GATE=true`) so Gemini Web does not prompt for approval on every call.
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

To use Plane in the devbox, install a container-compatible `plane` binary at
`~/.local/bin/plane` and the full skill at
`~/.skills-mgr/skills/plane/SKILL.md` on the Docker host. Add the Compose override
to mount both read-only without including them in the image. The skill is
available inside the devbox at `~/.agents/skills/plane/SKILL.md`:

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

- **`exec(code, yield_time_ms=10000, max_output_tokens=10000)`**: Runs raw JavaScript in Mekugi's V8 isolate (`codex-code-mode-host`), with nested `tools.exec_command`, `tools.write_stdin`, and `tools.apply_patch`, plus `// @exec: {...}` pragmas and `text()`/`store()`/`load()`/`exit()`/`yield_control()`.
- **`wait(cell_id, yield_time_ms=10000, max_output_tokens=10000, terminate=false)`**: Polls or terminates a yielded `exec` cell.

## Local Development & Tests

```bash
python3 -m venv .venv
.venv/bin/pip install -e ".[dev]"
.venv/bin/pytest
```
