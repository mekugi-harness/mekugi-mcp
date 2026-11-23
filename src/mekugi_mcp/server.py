"""FastMCP server exposing Mekugi's functions.exec and wait over Streamable HTTP."""

from __future__ import annotations

import logging
import os
from collections.abc import Sequence
from typing import Annotated, Any

import httpx
import uvicorn
from fastmcp import FastMCP
from fastmcp.server.middleware import CallNext, Middleware, MiddlewareContext
from fastmcp.tools.base import Tool
from pydantic import Field
from starlette.requests import Request
from starlette.responses import JSONResponse

from mekugi_mcp.auth import (
    build_google_auth_provider,
    enforce_allowed_email,
    parse_allowed_emails,
)
from mekugi_mcp.code_mode_client import DEFAULT_CELL_YIELD_TIME_MS, MAX_CELL_YIELD_TIME_MS
from mekugi_mcp.code_mode_docs import (
    fetch_devbox_tool_descriptions,
    resolve_mcp_tool_descriptions,
)
from mekugi_mcp.schema_sanitizer import GeminiSchemaSanitizerMiddleware

logger = logging.getLogger(__name__)

SERVER_INSTRUCTIONS = """You are connected to a single persistent Linux devbox running Mekugi's V8 code-mode runtime (`codex-code-mode-host`).
Use `exec` to run JavaScript that invokes `tools.exec_command`, `tools.write_stdin`, and `tools.apply_patch` inside `/workspace`.
Always pass values you want returned to `text(...)`. If a script yields with a `cell_id`, poll or terminate it with `wait`."""


def create_mcp_server(
    *,
    devbox_url: str | None = None,
    devbox_token: str | None = None,
    auth_provider: object | None = None,
    use_env_auth: bool = True,
    allowed_emails: frozenset[str] | None = None,
    allow_all_users: bool | None = None,
    no_confirmation_gate: bool | None = None,
    docs_path: str | None = None,
) -> FastMCP:
    resolved_devbox_url = (devbox_url or os.environ.get("DEVBOX_URL", "http://devbox:8080")).rstrip("/")
    resolved_devbox_token = (
        devbox_token
        if devbox_token is not None
        else os.environ.get("DEVBOX_INTERNAL_TOKEN", "").strip()
    )
    resolved_auth = build_google_auth_provider() if (use_env_auth and auth_provider is None) else auth_provider
    auth_enabled = resolved_auth is not None
    resolved_emails = (
        allowed_emails
        if allowed_emails is not None
        else parse_allowed_emails(os.environ.get("ALLOWED_EMAILS"))
    )
    if allow_all_users is None:
        allow_all_users = os.environ.get("ALLOW_ALL_GOOGLE_USERS", "").strip().lower() in (
            "1",
            "true",
            "yes",
        )
    if no_confirmation_gate is None:
        no_confirmation_gate = (
            os.environ.get("NO_CONFIRMATION_GATE", "true").strip().lower() not in ("0", "false", "no")
        )
    devbox_headers = (
        {"Authorization": f"Bearer {resolved_devbox_token}"} if resolved_devbox_token else {}
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

    static_docs_path = docs_path or os.environ.get("CODE_MODE_DOCS_PATH", "").strip() or None
    exec_description, wait_description, docs_source_ref = resolve_mcp_tool_descriptions(
        docs_path=static_docs_path,
        devbox_url=resolved_devbox_url,
        devbox_headers=devbox_headers,
        default_exec_yield_time_ms=DEFAULT_CELL_YIELD_TIME_MS,
    )
    docs_state: dict[str, str | None] = {
        "exec": exec_description,
        "wait": wait_description,
        "ref": docs_source_ref,
    }

    class _LiveCodeModeDocsMiddleware(Middleware):
        """Refreshes exec/wait descriptions from devbox on tools/list after recovery or upgrade."""

        async def on_list_tools(
            self,
            context: MiddlewareContext[Any],
            call_next: CallNext[Any, Sequence[Tool]],
        ) -> Sequence[Tool]:
            if static_docs_path is None:
                refreshed = await fetch_devbox_tool_descriptions(
                    devbox_url=resolved_devbox_url,
                    devbox_headers=devbox_headers,
                    default_exec_yield_time_ms=DEFAULT_CELL_YIELD_TIME_MS,
                )
                if refreshed is not None:
                    docs_state["exec"], docs_state["wait"], docs_state["ref"] = refreshed
            tools = await call_next(context)
            updated: list[Tool] = []
            for tool in tools:
                if tool.name == "exec" and docs_state["exec"]:
                    tool = tool.model_copy(update={"description": docs_state["exec"]})
                elif tool.name == "wait" and docs_state["wait"]:
                    tool = tool.model_copy(update={"description": docs_state["wait"]})
                updated.append(tool)
            return updated

    mcp = FastMCP(
        name="mekugi-devbox",
        instructions=SERVER_INSTRUCTIONS,
        auth=resolved_auth,  # type: ignore[arg-type]
        middleware=[_LiveCodeModeDocsMiddleware(), GeminiSchemaSanitizerMiddleware()],
    )

    @mcp.custom_route("/health", methods=["GET"])
    async def health(_request: Request) -> JSONResponse:
        try:
            async with httpx.AsyncClient(timeout=5.0) as http:
                resp = await http.get(f"{resolved_devbox_url}/health")
                resp.raise_for_status()
                return JSONResponse(
                    {
                        "status": "ok",
                        "code_mode_docs_ref": docs_state["ref"],
                        "devbox": resp.json(),
                    }
                )
        except Exception as exc:
            return JSONResponse({"status": "degraded", "error": str(exc)}, status_code=503)

    @mcp.tool(
        name="exec",
        description=exec_description,
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
        enforce_allowed_email(
            resolved_emails,
            auth_enabled=auth_enabled,
            allow_all_users=allow_all_users,
        )
        clamped_yield = max(0, min(int(yield_time_ms), MAX_CELL_YIELD_TIME_MS))
        timeout_s = max(15.0, (clamped_yield / 1000.0) + 12.0)
        async with httpx.AsyncClient(timeout=timeout_s) as http:
            resp = await http.post(
                f"{resolved_devbox_url}/exec",
                headers=devbox_headers,
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
        description=wait_description,
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
        enforce_allowed_email(
            resolved_emails,
            auth_enabled=auth_enabled,
            allow_all_users=allow_all_users,
        )
        clamped_yield = max(0, min(int(yield_time_ms), MAX_CELL_YIELD_TIME_MS))
        timeout_s = max(15.0, (clamped_yield / 1000.0) + 12.0)
        async with httpx.AsyncClient(timeout=timeout_s) as http:
            resp = await http.post(
                f"{resolved_devbox_url}/wait",
                headers=devbox_headers,
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
