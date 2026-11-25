"""Runtime renderer and fallback tool descriptions for Mekugi's code-mode MCP tools."""

from __future__ import annotations

import json
import logging
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

DEFAULT_CELL_YIELD_TIME_MS = 10_000
MAX_CELL_YIELD_TIME_MS = 45_000

NESTED_TOOLS_SECTION = """Nested tools available on `tools`:
- `await tools.exec_command({ cmd: string, workdir?: string, shell?: string, login?: boolean, tty?: boolean, yield_time_ms?: number, max_output_tokens?: number }): Promise<string>`
- `await tools.write_stdin({ session_id: number, chars?: string, yield_time_ms?: number, max_output_tokens?: number }): Promise<string>`
- `await tools.apply_patch(patch: string): Promise<string>` (Codex `*** Begin Patch` ... `*** End Patch` format)"""

MCP_BRIDGE_NOTES = (
    "MCP bridge notes:\n"
    "- `yield_time_ms` is capped at 45000 ms to stay within Gemini Web's tool timeout; "
    "if still running, continue with `wait` using the returned `cell_id`.\n"
    "- This MCP server returns text-only tool results: use `text(...)` for all output. "
    "`image()`, `audio()`, and `generatedImage()` are returned as truncated text placeholders "
    "rather than binary media blocks."
)

WAIT_MCP_ADDENDUM = (
    "- Set `terminate: true` to stop the running cell.\n"
    "- Unread output from exited shell sessions is retained for 10 minutes."
)

EXEC_TOOL_DESCRIPTION = """Run JavaScript code in Mekugi's V8 code-mode isolate to orchestrate devbox tool calls.
- Pass raw JavaScript in `code` (no Markdown fences). Each call runs as an async module in a fresh V8 isolate without Node, fs, network, or console.
- Call nested tools on the global `tools` object:
  - `await tools.exec_command({ cmd: string, workdir?: string, shell?: string, login?: boolean, tty?: boolean, yield_time_ms?: number, max_output_tokens?: number }): Promise<string>`
  - `await tools.write_stdin({ session_id: number, chars?: string, yield_time_ms?: number, max_output_tokens?: number }): Promise<string>`
  - `await tools.apply_patch(patch: string): Promise<string>` (Codex `*** Begin Patch` ... `*** End Patch` format)
- Optional first-line pragma: `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 10000}`.
- Global helpers: `text(value)`, `exit()`, `store(key, value)`, `load(key)`, `notify(value)`, `yield_control()`, `setTimeout`, `clearTimeout`, `ALL_TOOLS`.
- Always print results you want returned with `text(...)`.
- If the script is still running after `yield_time_ms` (capped at 45000 ms), the response includes a `cell_id`; continue with `wait`."""

WAIT_TOOL_DESCRIPTION = """Wait on a running `exec` cell, read its next output page, or terminate it.
- Pass `cell_id` from a previous `exec` or `wait` response.
- Set `terminate: true` to stop the running cell.
- For a running shell `session_id` inside a completed cell, poll `tools.write_stdin` inside `exec` instead (unread output after process exit is retained for 10 minutes)."""


@dataclass(frozen=True)
class ExtractedCodeModeDocs:
    source_repo: str
    source_ref: str
    exec_description_template: str
    wait_description_template: str
    image_helper_description: str

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ExtractedCodeModeDocs:
        return cls(
            source_repo=str(data["source_repo"]),
            source_ref=str(data["source_ref"]),
            exec_description_template=str(data["exec_description_template"]),
            wait_description_template=str(data["wait_description_template"]),
            image_helper_description=str(data["image_helper_description"]),
        )


def render_tool_descriptions(
    docs: ExtractedCodeModeDocs,
    *,
    default_exec_yield_time_ms: int = DEFAULT_CELL_YIELD_TIME_MS,
) -> tuple[str, str]:
    """Render Mekugi's extracted templates into MCP exec and wait tool descriptions."""
    exec_body = (
        docs.exec_description_template.replace(
            "{{ default_exec_yield_time_ms }}", str(default_exec_yield_time_ms)
        )
        .replace("{{ image_helper }}", docs.image_helper_description)
        .strip()
    )
    exec_description = f"{exec_body}\n\n{NESTED_TOOLS_SECTION}\n\n{MCP_BRIDGE_NOTES}"
    wait_description = f"{docs.wait_description_template.strip()}\n{WAIT_MCP_ADDENDUM}"
    return exec_description, wait_description


def load_extracted_docs_file(path: str | Path) -> ExtractedCodeModeDocs | None:
    p = Path(path)
    if not p.is_file():
        return None
    data = json.loads(p.read_text(encoding="utf-8"))
    return ExtractedCodeModeDocs.from_dict(data)


def resolve_mcp_tool_descriptions(
    *,
    docs_path: str | None = None,
    default_exec_yield_time_ms: int = DEFAULT_CELL_YIELD_TIME_MS,
) -> tuple[str, str, str | None]:
    """Resolve (exec_description, wait_description, source_ref) from static file or fallback."""
    resolved_path = docs_path or os.environ.get("CODE_MODE_DOCS_PATH", "").strip()
    if resolved_path:
        try:
            docs = load_extracted_docs_file(resolved_path)
            if docs is not None:
                exec_desc, wait_desc = render_tool_descriptions(
                    docs, default_exec_yield_time_ms=default_exec_yield_time_ms
                )
                return exec_desc, wait_desc, docs.source_ref
        except Exception as exc:
            logger.warning("Failed to load code-mode docs from %s: %s", resolved_path, exc)
    return EXEC_TOOL_DESCRIPTION, WAIT_TOOL_DESCRIPTION, None


async def fetch_devbox_tool_descriptions(
    *,
    cluster_transport: Any,
    default_exec_yield_time_ms: int = DEFAULT_CELL_YIELD_TIME_MS,
    timeout_s: float = 2.0,
) -> tuple[str, str, str] | None:
    """Asynchronously fetch and render live code-mode docs from the default devbox."""
    try:
        raw = await cluster_transport.fetch_code_mode_docs(
            None, int(max(0.2, timeout_s) * 1000)
        )
        if raw is None:
            return None
        docs = ExtractedCodeModeDocs.from_dict(raw)
        exec_desc, wait_desc = render_tool_descriptions(
            docs, default_exec_yield_time_ms=default_exec_yield_time_ms
        )
        return exec_desc, wait_desc, docs.source_ref
    except Exception as exc:
        logger.debug("Could not refresh code-mode docs via cluster transport: %s", exc)
        return None
