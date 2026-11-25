"""FastMCP server exposing Mekugi's functions.exec, wait, and list_boxes over Streamable HTTP.

All multi-box transport, keep-alive connection pooling/multiplexing, health probing,
and stateless cell routing live in the Rust extension (mekugi_mcp._transport).
Python stays focused on FastMCP tool registration, Google OAuth, and schema sanitization.
"""

from __future__ import annotations

import json
import logging
import os
import time
from collections.abc import Sequence
from typing import Annotated, Any

import uvicorn
from fastmcp import FastMCP
from fastmcp.server.middleware import CallNext, Middleware, MiddlewareContext
from fastmcp.tools.base import Tool
from pydantic import Field
from starlette.requests import Request
from starlette.responses import JSONResponse

from mekugi_mcp._transport import ClusterTransport
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

DOCS_REFRESH_TTL_S = 30.0
DOCS_RETRY_BACKOFF_S = 10.0

SERVER_INSTRUCTIONS = """You are connected to one or more persistent Linux devboxes running Mekugi's V8 code-mode runtime (`codex-code-mode-host`).
Use `list_boxes` to inspect available devboxes, their health, labels, and default selection.
Use `exec` to run JavaScript that invokes `tools.exec_command`, `tools.write_stdin`, and `tools.apply_patch` inside `/workspace`.
Pass optional `box` to `exec` to target a specific machine (omitting `box` routes to the server's default box).
Always pass values you want returned to `text(...)`. If a script yields with a `cell_id`, poll or terminate it with `wait` (namespaced `<box>:<cell_id>` IDs route automatically)."""

LIST_BOXES_DESCRIPTION = (
    "List all configured devbox machines with their transport, default flag, live reachability/latency, "
    "host metadata (hostname, os, arch, workdir), labels, description, and active cell/shell counts."
)


def build_cluster_transport(
    *,
    devbox_url: str | None = None,
    devbox_token: str | None = None,
    default_box: str | None = None,
    boxes_json: str | None = None,
    boxes_file: str | None = None,
) -> ClusterTransport:
    """Construct the Rust ClusterTransport from explicit args or MCP ENV vars."""
    resolved_token = (
        devbox_token
        if devbox_token is not None
        else os.environ.get("DEVBOX_INTERNAL_TOKEN", "").strip()
    ) or None
    resolved_default = (
        default_box
        if default_box is not None
        else os.environ.get("DEFAULT_BOX", "").strip()
    ) or None

    if boxes_json is not None and boxes_json.strip():
        return ClusterTransport.from_json(
            boxes_json,
            default_box=resolved_default,
            fallback_token=resolved_token,
        )
    if boxes_file is not None and boxes_file.strip():
        return ClusterTransport.from_file(
            boxes_file,
            default_box=resolved_default,
            fallback_token=resolved_token,
        )

    # Explicit devbox_url overrides cluster ENV vars (preserves single-box caller/test ergonomics).
    if devbox_url is not None:
        return ClusterTransport.single_box(
            devbox_url.rstrip("/"),
            box_name=resolved_default,
            token=resolved_token,
        )

    env_json = os.environ.get("DEVBOXES_JSON", "").strip()
    if env_json:
        return ClusterTransport.from_json(
            env_json,
            default_box=resolved_default,
            fallback_token=resolved_token,
        )

    env_file = os.environ.get("DEVBOXES_FILE", "").strip()
    if env_file:
        return ClusterTransport.from_file(
            env_file,
            default_box=resolved_default,
            fallback_token=resolved_token,
        )

    fallback_url = (
        os.environ.get("DEVBOX_URL", "").strip() or "grpc://devbox:50052"
    ).rstrip("/")
    return ClusterTransport.single_box(
        fallback_url,
        box_name=resolved_default or "local",
        token=resolved_token,
    )


def create_mcp_server(
    *,
    devbox_url: str | None = None,
    devbox_token: str | None = None,
    default_box: str | None = None,
    boxes_json: str | None = None,
    boxes_file: str | None = None,
    cluster_transport: ClusterTransport | None = None,
    auth_provider: object | None = None,
    use_env_auth: bool = True,
    allowed_emails: frozenset[str] | None = None,
    allow_all_users: bool | None = None,
    no_confirmation_gate: bool | None = None,
    docs_path: str | None = None,
) -> FastMCP:
    transport = (
        cluster_transport
        if cluster_transport is not None
        else build_cluster_transport(
            devbox_url=devbox_url,
            devbox_token=devbox_token,
            default_box=default_box,
            boxes_json=boxes_json,
            boxes_file=boxes_file,
        )
    )
    resolved_auth = (
        build_google_auth_provider()
        if (use_env_auth and auth_provider is None)
        else auth_provider
    )
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
            os.environ.get("NO_CONFIRMATION_GATE", "true").strip().lower()
            not in ("0", "false", "no")
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
        default_exec_yield_time_ms=DEFAULT_CELL_YIELD_TIME_MS,
    )
    docs_state: dict[str, Any] = {
        "exec": exec_description,
        "wait": wait_description,
        "ref": docs_source_ref,
        "last_attempt_at": time.monotonic() if docs_source_ref is not None else 0.0,
    }

    class _LiveCodeModeDocsMiddleware(Middleware):
        """Refreshes exec/wait descriptions from the default devbox via Rust keep-alive transport."""

        async def on_list_tools(
            self,
            context: MiddlewareContext[Any],
            call_next: CallNext[Any, Sequence[Tool]],
        ) -> Sequence[Tool]:
            now = time.monotonic()
            elapsed = now - float(docs_state["last_attempt_at"])
            retry_interval = DOCS_REFRESH_TTL_S if docs_state["ref"] is not None else DOCS_RETRY_BACKOFF_S
            should_refresh = static_docs_path is None and (
                docs_state["last_attempt_at"] == 0.0 or elapsed >= retry_interval
            )
            if should_refresh:
                refreshed = await fetch_devbox_tool_descriptions(
                    cluster_transport=transport,
                    default_exec_yield_time_ms=DEFAULT_CELL_YIELD_TIME_MS,
                )
                docs_state["last_attempt_at"] = time.monotonic()
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
            cluster_health = await transport.health_all(probe_timeout_ms=2_000)
            status_code = 200 if cluster_health["status"] == "ok" else 503
            boxes = cluster_health["boxes"]
            healthy_boxes = sum(1 for b in boxes if b.get("status") == "ok")
            return JSONResponse(
                {
                    "status": cluster_health["status"],
                    "default_box": cluster_health["default_box"],
                    "code_mode_docs_ref": docs_state["ref"],
                    "healthy_boxes": healthy_boxes,
                    "total_boxes": len(boxes),
                },
                status_code=status_code,
            )
        except Exception as exc:
            logger.warning("Health probe failed: %s", exc)
            return JSONResponse({"status": "degraded"}, status_code=503)

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
        box: Annotated[
            str | None,
            Field(
                description=(
                    "Optional target devbox name (defaults to the server's configured default box). "
                    "Call list_boxes to see available machines."
                )
            ),
        ] = None,
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
        data = await transport.exec(
            code,
            box_name=box,
            yield_time_ms=clamped_yield,
            max_output_tokens=int(max_output_tokens),
        )
        return str(data["text"])

    @mcp.tool(
        name="wait",
        description=wait_description,
        annotations=tool_annotations,
        output_schema=None,
    )
    async def wait_tool(
        cell_id: Annotated[
            str,
            Field(
                description="Identifier of the exec cell to wait on or terminate (namespaced '<box>:<cell_id>' routes automatically)."
            ),
        ],
        box: Annotated[
            str | None,
            Field(
                description=(
                    "Optional target devbox override when passing an unprefixed cell_id "
                    "(defaults to the server's configured default box)."
                )
            ),
        ] = None,
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
        data = await transport.wait(
            cell_id,
            box_name=box,
            yield_time_ms=clamped_yield,
            max_output_tokens=int(max_output_tokens),
            terminate=bool(terminate),
        )
        return str(data["text"])

    @mcp.tool(
        name="list_boxes",
        description=LIST_BOXES_DESCRIPTION,
        annotations={
            "readOnlyHint": True,
            "destructiveHint": False,
            "openWorldHint": False,
        },
        output_schema=None,
    )
    async def list_boxes_tool() -> str:
        enforce_allowed_email(
            resolved_emails,
            auth_enabled=auth_enabled,
            allow_all_users=allow_all_users,
        )
        boxes = await transport.list_boxes(probe_timeout_ms=2_000)
        payload = {
            "default_box": transport.default_box,
            "boxes": boxes,
        }
        return json.dumps(payload, indent=2)

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
