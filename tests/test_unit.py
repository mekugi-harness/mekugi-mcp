"""Unit tests for pragma parsing, apply_patch, schema sanitization, and auth config."""

from __future__ import annotations

import json
import os
from pathlib import Path

import httpx
import pytest
from fastmcp import Client
from mekugi_mcp._transport import format_cell_id, parse_cell_route

from mekugi_mcp.auth import build_google_auth_provider, enforce_allowed_email, parse_allowed_emails
from mekugi_mcp.code_mode_docs import (
    EXEC_TOOL_DESCRIPTION,
    ExtractedCodeModeDocs,
    render_tool_descriptions,
)
from mekugi_mcp.schema_sanitizer import FORBIDDEN_SCHEMA_KEYS, sanitize_gemini_schema
from mekugi_mcp.server import create_mcp_server


def test_exec_tool_docs_include_argv_stdin_env() -> None:
    for desc in (
        EXEC_TOOL_DESCRIPTION,
        render_tool_descriptions(
            ExtractedCodeModeDocs(
                source_repo="mekugi-harness/mekugi",
                source_ref="test-ref",
                exec_description_template="Run JS {{ default_exec_yield_time_ms }} {{ image_helper }}",
                wait_description_template="Wait on cell",
                image_helper_description="img",
            )
        )[0],
    ):
        assert "argv: [string, ...string[]]" in desc
        assert "stdin?: string" in desc
        assert "env?: Record<string, string>" in desc


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
    assert names == {"exec", "wait", "list_boxes"}
    by_name = {t.name: t for t in tools}
    assert "box" in by_name["exec"].input_schema["properties"]
    assert by_name["exec"].input_schema["properties"]["box"]["type"] == "string"
    assert "box" not in by_name["exec"].input_schema.get("required", [])
    assert "box" in by_name["wait"].input_schema["properties"]

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


@pytest.mark.asyncio
async def test_mcp_http_oauth_discovery_protection_and_callback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("AUTH_DISABLED", raising=False)
    monkeypatch.delenv("ALLOW_ALL_GOOGLE_USERS", raising=False)
    monkeypatch.setenv("GOOGLE_CLIENT_ID", "test-id.apps.googleusercontent.com")
    monkeypatch.setenv("GOOGLE_CLIENT_SECRET", "test-secret")
    monkeypatch.setenv("BASE_URL", "https://mcp.example.com")
    monkeypatch.setenv("JWT_SIGNING_KEY", "super-secret-signing-key-32-bytes")
    monkeypatch.setenv("ALLOWED_EMAILS", "alice@example.com")
    monkeypatch.setenv("OAUTH_STORAGE_DIR", str(tmp_path / "oauth"))

    mcp = create_mcp_server(devbox_url="http://127.0.0.1:9", use_env_auth=True)
    app = mcp.http_app(path="/mcp")
    async with (
        app.router.lifespan_context(app),
        httpx.AsyncClient(
            transport=httpx.ASGITransport(app=app),
            base_url="https://mcp.example.com",
        ) as client,
    ):
        health = await client.get("/health")
        assert health.status_code == 503
        assert health.json()["status"] == "degraded"

        discovery = await client.get("/.well-known/oauth-authorization-server")
        assert discovery.status_code == 200
        meta = discovery.json()
        assert meta["issuer"].rstrip("/") == "https://mcp.example.com"
        assert "authorization_endpoint" in meta
        assert "token_endpoint" in meta

        unauth_mcp = await client.post("/mcp", json={"jsonrpc": "2.0", "id": 1, "method": "ping"})
        assert unauth_mcp.status_code == 401

        callback = await client.get("/auth/callback")
        assert callback.status_code == 400
        assert "missing" in callback.text.lower() or "code" in callback.text.lower()


@pytest.mark.asyncio


@pytest.mark.asyncio
async def test_mcp_refreshes_tool_descriptions_after_devbox_recovers(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("CODE_MODE_DOCS_PATH", raising=False)
    from mekugi_mcp import server as server_mod

    monkeypatch.setattr(server_mod, "DOCS_RETRY_BACKOFF_S", 0.0)
    state: dict[str, tuple[str, str, str] | None] = {"live": None}

    async def _fake_fetch(**_kwargs: object) -> tuple[str, str, str] | None:
        return state["live"]

    monkeypatch.setattr(server_mod, "fetch_devbox_tool_descriptions", _fake_fetch)

    mcp = create_mcp_server(devbox_url="http://127.0.0.1:9", use_env_auth=False)
    async with Client(mcp) as client:
        before = {t.name: t.description for t in await client.list_tools()}
        assert "Upgraded Mekugi exec" not in (before["exec"] or "")

        state["live"] = ("Upgraded Mekugi exec", "Upgraded Mekugi wait", "rev-2")
        after = {t.name: t.description for t in await client.list_tools()}
        assert after["exec"] == "Upgraded Mekugi exec"
        assert after["wait"] == "Upgraded Mekugi wait"



@pytest.mark.asyncio
async def test_cluster_config_env_and_routing_errors(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    assert format_cell_id("gpu-1", "c1") == "gpu-1:c1"
    assert parse_cell_route("gpu-1:c1", None, "local") == ("gpu-1", "c1")
    assert parse_cell_route("c1", None, "local") == ("local", "c1")
    with pytest.raises(ValueError, match="conflicting box"):
        parse_cell_route("gpu-1:c1", "local", "local")

    boxes_cfg = {
        "default_box": "gpu-1",
        "boxes": [
            {"name": "local", "target": "grpc://127.0.0.1:9", "description": "Local box"},
            {
                "name": "gpu-1",
                "target": "grpc://127.0.0.1:9",
                "pool_size": 2,
                "labels": {"gpu": "rtx4090"},
            },
        ],
    }
    cfg_file = tmp_path / "boxes.json"
    cfg_file.write_text(json.dumps(boxes_cfg), encoding="utf-8")

    monkeypatch.delenv("DEVBOXES_JSON", raising=False)
    monkeypatch.setenv("DEVBOXES_FILE", str(cfg_file))
    monkeypatch.setenv("DEFAULT_BOX", "")
    mcp_from_file_default = create_mcp_server(use_env_auth=False)
    async with Client(mcp_from_file_default) as client:
        listed_default = await client.call_tool("list_boxes", {})
        payload_default = json.loads(listed_default.content[0].text)  # type: ignore[union-attr]
        assert payload_default["default_box"] == "gpu-1"

    monkeypatch.setenv("DEFAULT_BOX", "local")
    mcp = create_mcp_server(use_env_auth=False)
    async with Client(mcp) as client:
        listed = await client.call_tool("list_boxes", {})
        payload = json.loads(listed.content[0].text)  # type: ignore[union-attr]
        assert payload["default_box"] == "local"
        assert [b["name"] for b in payload["boxes"]] == ["local", "gpu-1"]
        assert payload["boxes"][0]["is_default"] is True
        assert payload["boxes"][1]["pool_size"] == 2
        assert payload["boxes"][1]["labels"] == {"gpu": "rtx4090"}

        with pytest.raises(Exception, match="unknown box 'nope'"):
            await client.call_tool("exec", {"code": 'text("hi");', "box": "nope"})
