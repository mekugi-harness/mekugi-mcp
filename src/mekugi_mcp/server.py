"""FastMCP server exposing Mekugi's functions.exec and wait over Streamable HTTP."""

from __future__ import annotations

import logging
import os
from typing import Annotated

import httpx
import uvicorn
from fastmcp import FastMCP
from pydantic import Field
from starlette.requests import Request
from starlette.responses import JSONResponse

from mekugi_mcp.auth import (
    build_google_auth_provider,
    enforce_allowed_email,
    parse_allowed_emails,
)
from mekugi_mcp.code_mode_client import DEFAULT_CELL_YIELD_TIME_MS, MAX_CELL_YIELD_TIME_MS
from mekugi_mcp.schema_sanitizer import GeminiSchemaSanitizerMiddleware
from mekugi_mcp.tool_catalog import EXEC_TOOL_DESCRIPTION, WAIT_TOOL_DESCRIPTION

logger = logging.getLogger(__name__)

SERVER_INSTRUCTIONS = """You are connected to a single persistent Linux devbox running Mekugi's V8 code-mode runtime (`codex-code-mode-host`).
Use `exec` to run JavaScript that invokes `tools.exec_command`, `tools.write_stdin`, and `tools.apply_patch` inside `/workspace`.
Always pass values you want returned to `text(...)`. If a script yields with a `cell_id`, poll or terminate it with `wait`."""


def create_mcp_server(
    *,
    devbox_url: str | None = None,
    auth_provider: object | None = None,
    use_env_auth: bool = True,
    allowed_emails: frozenset[str] | None = None,
    no_confirmation_gate: bool | None = None,
) -> FastMCP:
    resolved_devbox_url = (devbox_url or os.environ.get("DEVBOX_URL", "http://devbox:8080")).rstrip("/")
    resolved_auth = build_google_auth_provider() if (use_env_auth and auth_provider is None) else auth_provider
    resolved_emails = (
        allowed_emails
        if allowed_emails is not None
        else parse_allowed_emails(os.environ.get("ALLOWED_EMAILS"))
    )
    if no_confirmation_gate is None:
        no_confirmation_gate = (
            os.environ.get("NO_CONFIRMATION_GATE", "true").strip().lower() not in ("0", "false", "no")
        )

    tool_annotations = (
        {
            "readOnlyHint": True,
            "destructiveHint": False,
            "openWorldHint": False,
        }
        if no_confirmation_gate
        else {
            "readOnlyHint": False,
            "destructiveHint": True,
            "openWorldHint": True,
        }
    )

    mcp = FastMCP(
        name="mekugi-devbox",
        instructions=SERVER_INSTRUCTIONS,
        auth=resolved_auth,  # type: ignore[arg-type]
        middleware=[GeminiSchemaSanitizerMiddleware()],
    )

    @mcp.custom_route("/health", methods=["GET"])
    async def health(_request: Request) -> JSONResponse:
        try:
            async with httpx.AsyncClient(timeout=5.0) as http:
                resp = await http.get(f"{resolved_devbox_url}/health")
                resp.raise_for_status()
                return JSONResponse({"status": "ok", "devbox": resp.json()})
        except Exception as exc:
            return JSONResponse({"status": "degraded", "error": str(exc)}, status_code=503)

    @mcp.tool(
        name="exec",
        description=EXEC_TOOL_DESCRIPTION,
        annotations=tool_annotations,
        output_schema=None,
    )
    async def exec_tool(
        code: Annotated[
            str,
            Field(
                description="Raw JavaScript source to run in Mekugi's V8 isolate (optional first-line `// @exec: {...}` pragma)."
            ),
        ],
        yield_time_ms: Annotated[
            int,
            Field(
                description="Wait time in ms before yielding a running cell_id (default 10000, max 45000).",
                ge=0,
                le=MAX_CELL_YIELD_TIME_MS,
            ),
        ] = DEFAULT_CELL_YIELD_TIME_MS,
        max_output_tokens: Annotated[
            int,
            Field(
                description="Output token budget for this call (default 10000).",
                ge=100,
                le=100_000,
            ),
        ] = 10_000,
    ) -> str:
        enforce_allowed_email(resolved_emails)
        clamped_yield = max(0, min(int(yield_time_ms), MAX_CELL_YIELD_TIME_MS))
        timeout_s = max(15.0, (clamped_yield / 1000.0) + 12.0)
        async with httpx.AsyncClient(timeout=timeout_s) as http:
            resp = await http.post(
                f"{resolved_devbox_url}/exec",
                json={
                    "code": code,
                    "yield_time_ms": clamped_yield,
                    "max_output_tokens": int(max_output_tokens),
                },
            )
            data = resp.json()
            if resp.status_code >= 400:
                raise RuntimeError(data.get("error", f"Devbox error ({resp.status_code})"))
            return str(data["text"])

    @mcp.tool(
        name="wait",
        description=WAIT_TOOL_DESCRIPTION,
        annotations=tool_annotations,
        output_schema=None,
    )
    async def wait_tool(
        cell_id: Annotated[str, Field(description="Identifier of the exec cell to wait on or terminate.")],
        yield_time_ms: Annotated[
            int,
            Field(
                description="Wait time in ms before yielding more output (default 10000, max 45000).",
                ge=0,
                le=MAX_CELL_YIELD_TIME_MS,
            ),
        ] = DEFAULT_CELL_YIELD_TIME_MS,
        max_output_tokens: Annotated[
            int,
            Field(
                description="Output token budget for this wait call (default 10000).",
                ge=100,
                le=100_000,
            ),
        ] = 10_000,
        terminate: Annotated[
            bool,
            Field(description="Set true to stop the running exec cell."),
        ] = False,
    ) -> str:
        enforce_allowed_email(resolved_emails)
        clamped_yield = max(0, min(int(yield_time_ms), MAX_CELL_YIELD_TIME_MS))
        timeout_s = max(15.0, (clamped_yield / 1000.0) + 12.0)
        async with httpx.AsyncClient(timeout=timeout_s) as http:
            resp = await http.post(
                f"{resolved_devbox_url}/wait",
                json={
                    "cell_id": cell_id,
                    "yield_time_ms": clamped_yield,
                    "max_output_tokens": int(max_output_tokens),
                    "terminate": bool(terminate),
                },
            )
            data = resp.json()
            if resp.status_code >= 400:
                raise RuntimeError(data.get("error", f"Devbox error ({resp.status_code})"))
            return str(data["text"])

    return mcp


def main() -> None:
    logging.basicConfig(
        level=os.environ.get("LOG_LEVEL", "INFO").upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    host = os.environ.get("MCP_HOST", "0.0.0.0")
    port = int(os.environ.get("MCP_PORT", "8000"))
    mcp = create_mcp_server()
    app = mcp.http_app(path="/mcp", transport="streamable-http")
    uvicorn.run(app, host=host, port=port, log_level="info")


if __name__ == "__main__":
    main()
