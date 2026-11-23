"""JSON Schema sanitizer for Gemini Web Custom App MCP compatibility."""

from __future__ import annotations

from collections.abc import Sequence
from typing import Any

from fastmcp.server.middleware import CallNext, Middleware, MiddlewareContext
from fastmcp.tools.base import Tool

FORBIDDEN_SCHEMA_KEYS = frozenset(
    {
        "$schema",
        "additionalProperties",
        "$defs",
        "definitions",
        "const",
        "exclusiveMinimum",
        "exclusiveMaximum",
    }
)

INT32_MIN = -2_147_483_648
INT32_MAX = 2_147_483_647


def sanitize_gemini_schema(schema: Any) -> Any:
    """Recursively strip JSON Schema constructs rejected by Gemini Web connectors."""
    if isinstance(schema, list):
        return [sanitize_gemini_schema(item) for item in schema]
    if not isinstance(schema, dict):
        return schema

    cleaned: dict[str, Any] = {}
    for key, value in schema.items():
        if key in FORBIDDEN_SCHEMA_KEYS:
            continue
        if key == "anyOf" and isinstance(value, list):
            non_null = [v for v in value if not (isinstance(v, dict) and v.get("type") == "null")]
            if len(non_null) == 1 and isinstance(non_null[0], dict):
                merged = sanitize_gemini_schema(non_null[0])
                for mkey, mval in merged.items():
                    cleaned.setdefault(mkey, mval)
                continue
        if key in ("minimum", "maximum") and isinstance(value, int):
            cleaned[key] = max(INT32_MIN, min(INT32_MAX, value))
            continue
        cleaned[key] = sanitize_gemini_schema(value)

    return cleaned


class GeminiSchemaSanitizerMiddleware(Middleware):
    """Sanitizes tool input schemas on tools/list for Gemini Web compatibility."""

    async def on_list_tools(
        self,
        context: MiddlewareContext[Any],
        call_next: CallNext[Any, Sequence[Tool]],
    ) -> Sequence[Tool]:
        tools = await call_next(context)
        sanitized: list[Tool] = []
        for tool in tools:
            if isinstance(tool.parameters, dict):
                tool = tool.model_copy(update={"parameters": sanitize_gemini_schema(tool.parameters)})
            sanitized.append(tool)
        return sanitized
