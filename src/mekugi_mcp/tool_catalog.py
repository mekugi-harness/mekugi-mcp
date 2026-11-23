"""Nested tool definitions for codex-code-mode-host and MCP tool descriptions."""

from __future__ import annotations

import json

from mekugi_mcp.proto import code_mode_v1_pb2 as pb

EXEC_COMMAND_SCHEMA = {
    "type": "object",
    "properties": {
        "cmd": {
            "type": "string",
            "description": "Shell command to execute inside the devbox.",
        },
        "workdir": {
            "type": "string",
            "description": "Optional working directory (defaults to /workspace).",
        },
        "shell": {
            "type": "string",
            "description": "Optional shell executable (defaults to /bin/bash).",
        },
        "login": {
            "type": "boolean",
            "description": "Run shell with -l login semantics (default true).",
        },
        "tty": {
            "type": "boolean",
            "description": "Allocate a PTY for interactive sessions (default false).",
        },
        "yield_time_ms": {
            "type": "number",
            "description": "How long to wait in ms before returning a running session ID (default 10000).",
        },
        "max_output_tokens": {
            "type": "number",
            "description": "Maximum output tokens to return (default 10000).",
        },
    },
    "required": ["cmd"],
}

WRITE_STDIN_SCHEMA = {
    "type": "object",
    "properties": {
        "session_id": {
            "type": "number",
            "description": "Identifier of the running shell session.",
        },
        "chars": {
            "type": "string",
            "description": "Bytes/characters to write to stdin (empty string polls output).",
        },
        "yield_time_ms": {
            "type": "number",
            "description": "How long to wait in ms for output (default 250).",
        },
        "max_output_tokens": {
            "type": "number",
            "description": "Maximum output tokens to return (default 10000).",
        },
    },
    "required": ["session_id"],
}


def build_nested_tool_definitions() -> list[pb.ToolDefinition]:
    """Return the nested tool definitions exposed on global `tools` in V8."""
    return [
        pb.ToolDefinition(
            name="exec_command",
            tool_name=pb.ToolName(name="exec_command"),
            description="Runs a command in a shell or PTY inside the devbox, returning output or a session ID.",
            kind=pb.TOOL_KIND_FUNCTION,
            input_schema_json=json.dumps(EXEC_COMMAND_SCHEMA).encode("utf-8"),
            pauses_cell_deadline=False,
        ),
        pb.ToolDefinition(
            name="write_stdin",
            tool_name=pb.ToolName(name="write_stdin"),
            description="Writes characters to an existing exec_command session and returns recent output.",
            kind=pb.TOOL_KIND_FUNCTION,
            input_schema_json=json.dumps(WRITE_STDIN_SCHEMA).encode("utf-8"),
            pauses_cell_deadline=False,
        ),
        pb.ToolDefinition(
            name="apply_patch",
            tool_name=pb.ToolName(name="apply_patch"),
            description="Applies a Codex patch (`*** Begin Patch` ... `*** End Patch`) to files in the devbox.",
            kind=pb.TOOL_KIND_FREEFORM,
            pauses_cell_deadline=False,
        ),
    ]


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
- For a running shell `session_id` inside a completed cell, poll `tools.write_stdin` inside `exec` instead."""
