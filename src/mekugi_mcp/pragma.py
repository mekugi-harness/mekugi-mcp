"""Parser for Mekugi / Codex code-mode `// @exec: {...}` first-line pragmas."""

from __future__ import annotations

import json
import re
from dataclasses import dataclass

PRAGMA_RE = re.compile(r"^[ \t]*//\s*@exec:(.*)$")


@dataclass(frozen=True)
class ParsedExecSource:
    code: str
    yield_time_ms: int | None = None
    max_output_tokens: int | None = None


def parse_exec_source(raw: str) -> ParsedExecSource:
    """Extract an optional `// @exec: {...}` first-line pragma from JS source."""
    if not raw:
        return ParsedExecSource(code="")

    first_nl = raw.find("\n")
    first_line = raw if first_nl == -1 else raw[:first_nl]
    rest = "" if first_nl == -1 else raw[first_nl + 1 :]

    match = PRAGMA_RE.match(first_line.rstrip("\r"))
    if not match:
        return ParsedExecSource(code=raw)

    payload_str = match.group(1).strip()
    if not payload_str:
        return ParsedExecSource(code=rest)

    try:
        payload = json.loads(payload_str)
    except json.JSONDecodeError as exc:
        raise ValueError(f"Invalid // @exec pragma JSON: {exc}") from exc

    if not isinstance(payload, dict):
        raise ValueError("// @exec pragma must be a JSON object")

    yield_time_ms: int | None = None
    max_output_tokens: int | None = None

    if "yield_time_ms" in payload and payload["yield_time_ms"] is not None:
        val = payload["yield_time_ms"]
        if isinstance(val, bool) or not isinstance(val, (int, float)) or val < 0:
            raise ValueError("// @exec yield_time_ms must be a non-negative number")
        yield_time_ms = int(val)

    if "max_output_tokens" in payload and payload["max_output_tokens"] is not None:
        val = payload["max_output_tokens"]
        if isinstance(val, bool) or not isinstance(val, (int, float)) or val <= 0:
            raise ValueError("// @exec max_output_tokens must be a positive number")
        max_output_tokens = int(val)

    return ParsedExecSource(
        code=rest,
        yield_time_ms=yield_time_ms,
        max_output_tokens=max_output_tokens,
    )
