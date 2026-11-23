"""Unit tests for pragma parsing, apply_patch, schema sanitization, and auth config."""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from fastmcp import Client

from mekugi_mcp.apply_patch import PatchError, apply_patch
from mekugi_mcp.auth import build_google_auth_provider, enforce_allowed_email, parse_allowed_emails
from mekugi_mcp.pragma import parse_exec_source
from mekugi_mcp.schema_sanitizer import FORBIDDEN_SCHEMA_KEYS, sanitize_gemini_schema
from mekugi_mcp.server import create_mcp_server


def test_parse_exec_source_plain() -> None:
    parsed = parse_exec_source("text('hi');")
    assert parsed.code == "text('hi');"
    assert parsed.yield_time_ms is None
    assert parsed.max_output_tokens is None


def test_parse_exec_source_with_pragma() -> None:
    raw = '// @exec: {"yield_time_ms": 5000, "max_output_tokens": 2048}\ntext("ok");'
    parsed = parse_exec_source(raw)
    assert parsed.code == 'text("ok");'
    assert parsed.yield_time_ms == 5000
    assert parsed.max_output_tokens == 2048


def test_parse_exec_source_invalid_pragma() -> None:
    with pytest.raises(ValueError, match="Invalid // @exec pragma JSON"):
        parse_exec_source("// @exec: {bad json\ntext('x');")


def test_apply_patch_add_update_move_delete(tmp_path: Path) -> None:
    res = apply_patch(
        "\n".join(
            [
                "*** Begin Patch",
                "*** Add File: hello.txt",
                "+line 1",
                "+line 2",
                "*** End Patch",
            ]
        ),
        workdir=str(tmp_path),
    )
    assert "A hello.txt" in res
    assert (tmp_path / "hello.txt").read_text() == "line 1\nline 2\n"

    res = apply_patch(
        "\n".join(
            [
                "*** Begin Patch",
                "*** Update File: hello.txt",
                "*** Move to: sub/greet.txt",
                "@@",
                " line 1",
                "-line 2",
                "+line two",
                "*** End Patch",
            ]
        ),
        workdir=str(tmp_path),
    )
    assert "M hello.txt -> sub/greet.txt" in res
    assert not (tmp_path / "hello.txt").exists()
    assert (tmp_path / "sub/greet.txt").read_text() == "line 1\nline two\n"

    res = apply_patch(
        "\n".join(
            [
                "*** Begin Patch",
                "*** Delete File: sub/greet.txt",
                "*** End Patch",
            ]
        ),
        workdir=str(tmp_path),
    )
    assert "D sub/greet.txt" in res
    assert not (tmp_path / "sub/greet.txt").exists()


def test_apply_patch_missing_context_raises(tmp_path: Path) -> None:
    (tmp_path / "a.txt").write_text("alpha\n")
    with pytest.raises(PatchError):
        apply_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-nope\n+yes\n*** End Patch",
            workdir=str(tmp_path),
        )


def test_sanitize_gemini_schema_strips_forbidden_keys() -> None:
    raw = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": False,
        "$defs": {"Foo": {"type": "string"}},
        "properties": {
            "count": {"type": "integer", "maximum": 10**15, "exclusiveMinimum": 0},
            "opt": {"anyOf": [{"type": "string"}, {"type": "null"}], "const": "x"},
        },
    }
    cleaned = sanitize_gemini_schema(raw)
    for forbidden in FORBIDDEN_SCHEMA_KEYS:
        assert forbidden not in cleaned
    assert cleaned["properties"]["count"]["maximum"] == 2_147_483_647
    assert "exclusiveMinimum" not in cleaned["properties"]["count"]
    assert cleaned["properties"]["opt"]["type"] == "string"
    assert "const" not in cleaned["properties"]["opt"]


@pytest.mark.asyncio
async def test_mcp_list_tools_gemini_safe_and_no_gate() -> None:
    mcp = create_mcp_server(devbox_url="http://127.0.0.1:9", use_env_auth=False)
    async with Client(mcp) as client:
        tools = await client.list_tools()

    names = {t.name for t in tools}
    assert names == {"exec", "wait"}

    for tool in tools:
        assert tool.annotations is not None
        assert tool.annotations.read_only_hint is True
        assert tool.annotations.destructive_hint is False
        assert tool.annotations.open_world_hint is False
        schema_str = str(tool.input_schema)
        assert "additionalProperties" not in schema_str
        assert "$defs" not in schema_str
        assert "$schema" not in schema_str


def test_google_auth_provider_construction(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("AUTH_DISABLED", raising=False)
    monkeypatch.delenv("ALLOWED_EMAILS", raising=False)
    monkeypatch.delenv("ALLOW_ALL_GOOGLE_USERS", raising=False)
    monkeypatch.setenv("GOOGLE_CLIENT_ID", "test-id.apps.googleusercontent.com")
    monkeypatch.setenv("GOOGLE_CLIENT_SECRET", "test-secret")
    monkeypatch.setenv("BASE_URL", "https://mcp.example.com")
    monkeypatch.setenv("JWT_SIGNING_KEY", "super-secret-signing-key-32-bytes")
    monkeypatch.setenv("OAUTH_STORAGE_DIR", str(tmp_path / "oauth"))

    with pytest.raises(RuntimeError, match="ALLOWED_EMAILS"):
        build_google_auth_provider()

    monkeypatch.setenv("ALLOWED_EMAILS", "Alice@Example.com, bob@example.com ")
    provider = build_google_auth_provider()
    assert provider is not None
    assert (tmp_path / "oauth").exists()
    assert parse_allowed_emails("Alice@Example.com, bob@example.com ") == frozenset(
        {"alice@example.com", "bob@example.com"}
    )
    with pytest.raises(PermissionError, match="ALLOWED_EMAILS"):
        enforce_allowed_email(frozenset(), auth_enabled=True)
    assert os.environ["BASE_URL"] == "https://mcp.example.com"
