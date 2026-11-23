"""Unit tests for pragma parsing, apply_patch, schema sanitization, and auth config."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path

import pytest
from fastmcp import Client

from mekugi_mcp import pty_manager
from mekugi_mcp.apply_patch import PatchError, apply_patch
from mekugi_mcp.auth import build_google_auth_provider, enforce_allowed_email, parse_allowed_emails
from mekugi_mcp.pragma import parse_exec_source
from mekugi_mcp.pty_manager import ProcessManager
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


def test_apply_patch_eof_edits_final_occurrence(tmp_path: Path) -> None:
    (tmp_path / "dup.txt").write_text("block\nmiddle\nblock\n")
    apply_patch(
        "*** Begin Patch\n*** Update File: dup.txt\n@@\n-block\n+tail\n*** End of File\n*** End Patch",
        workdir=str(tmp_path),
    )
    assert (tmp_path / "dup.txt").read_text() == "block\nmiddle\ntail\n"


def test_apply_patch_scoped_hunk_rejects_missing_or_out_of_scope(tmp_path: Path) -> None:
    (tmp_path / "scoped.txt").write_text("def a():\n    x = 1\ndef b():\n    y = 2\n")
    with pytest.raises(PatchError):
        apply_patch(
            "*** Begin Patch\n*** Update File: scoped.txt\n@@ def missing():\n-    x = 1\n+    x = 2\n*** End Patch",
            workdir=str(tmp_path),
        )
    with pytest.raises(PatchError):
        apply_patch(
            "*** Begin Patch\n*** Update File: scoped.txt\n@@ def b():\n-    x = 1\n+    x = 2\n*** End Patch",
            workdir=str(tmp_path),
        )
    assert (tmp_path / "scoped.txt").read_text() == "def a():\n    x = 1\ndef b():\n    y = 2\n"


def test_apply_patch_truncated_commits_nothing(tmp_path: Path) -> None:
    with pytest.raises(PatchError, match="End Patch"):
        apply_patch(
            "*** Begin Patch\n*** Add File: partial.txt\n+hello\n",
            workdir=str(tmp_path),
        )
    assert not (tmp_path / "partial.txt").exists()


def test_apply_patch_append_only_appends_to_end(tmp_path: Path) -> None:
    (tmp_path / "log.txt").write_text("first\nsecond\n")
    apply_patch(
        "*** Begin Patch\n*** Update File: log.txt\n@@\n+third\n*** End Patch",
        workdir=str(tmp_path),
    )
    assert (tmp_path / "log.txt").read_text() == "first\nsecond\nthird\n"


def test_apply_patch_preserves_crlf(tmp_path: Path) -> None:
    target = tmp_path / "win.txt"
    target.write_bytes(b"one\r\ntwo\r\nthree\r\n")
    apply_patch(
        "*** Begin Patch\n*** Update File: win.txt\n@@\n one\n-two\n+TWO\n three\n*** End Patch",
        workdir=str(tmp_path),
    )
    assert target.read_bytes() == b"one\r\nTWO\r\nthree\r\n"


def test_apply_patch_failed_rename_to_dir_preserves_source(tmp_path: Path) -> None:
    src = tmp_path / "src.txt"
    src.write_text("hello\n")
    (tmp_path / "dest_dir").mkdir()
    patch = (
        "*** Begin Patch\n"
        "*** Update File: src.txt\n"
        "@@\n"
        "-hello\n"
        "+world\n"
        "*** Update File: src.txt\n"
        "*** Move to: dest_dir\n"
        "*** End Patch"
    )
    with pytest.raises(PatchError, match="directory"):
        apply_patch(patch, workdir=str(tmp_path))
    assert src.exists()
    assert src.read_text() == "hello\n"


def test_apply_patch_deletes_binary_file(tmp_path: Path) -> None:
    img = tmp_path / "pixel.png"
    img.write_bytes(b"\x89PNG\r\n\x1a\n\xff\xfe\x00\x01")
    apply_patch(
        "*** Begin Patch\n*** Delete File: pixel.png\n*** End Patch",
        workdir=str(tmp_path),
    )
    assert not img.exists()


def test_apply_patch_hard_linked_files_preserve_both_edits(tmp_path: Path) -> None:
    a = tmp_path / "a.txt"
    b = tmp_path / "b.txt"
    a.write_text("shared\n")
    os.link(a, b)
    patch = (
        "*** Begin Patch\n"
        "*** Update File: a.txt\n"
        "@@\n"
        "-shared\n"
        "+edited-a\n"
        "*** Update File: b.txt\n"
        "@@\n"
        "-shared\n"
        "+edited-b\n"
        "*** End Patch"
    )
    apply_patch(patch, workdir=str(tmp_path))
    assert a.read_text() == "edited-a\n"
    assert b.read_text() == "edited-b\n"


def test_apply_patch_replace_file_with_directory_child(tmp_path: Path) -> None:
    mod = tmp_path / "mod"
    mod.write_text("old\n")
    patch = (
        "*** Begin Patch\n"
        "*** Delete File: mod\n"
        "*** Add File: mod/index.py\n"
        "+new\n"
        "*** End Patch"
    )
    apply_patch(patch, workdir=str(tmp_path))
    assert (tmp_path / "mod" / "index.py").read_text() == "new\n"


def test_apply_patch_conflicting_file_and_dir_writes_preserves_original(tmp_path: Path) -> None:
    mod = tmp_path / "mod"
    mod.write_text("keep\n")
    patch = (
        "*** Begin Patch\n"
        "*** Delete File: mod\n"
        "*** Add File: mod/sub\n"
        "+file\n"
        "*** Add File: mod/sub/child\n"
        "+child\n"
        "*** End Patch"
    )
    with pytest.raises(PatchError, match="Conflicting patch paths"):
        apply_patch(patch, workdir=str(tmp_path))
    assert mod.read_text() == "keep\n"


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


@pytest.mark.asyncio
async def test_pty_manager_cancelling_exec_command_terminates_process(tmp_path: Path) -> None:
    mgr = ProcessManager(default_workdir=str(tmp_path))
    task = asyncio.create_task(mgr.exec_command("sleep 30", yield_time_ms=10_000))
    await asyncio.sleep(0.1)
    assert len(mgr._sessions) == 1
    proc = next(iter(mgr._sessions.values())).process

    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task

    assert len(mgr._sessions) == 0
    assert proc.returncode is not None


@pytest.mark.asyncio
async def test_pty_manager_reaps_unpolled_exited_pty_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(pty_manager, "EXITED_SESSION_TTL_S", 0.05)
    mgr = ProcessManager(default_workdir=str(tmp_path))
    out = await mgr.exec_command("sleep 0.15; echo done", tty=True, yield_time_ms=20)
    assert "Process running with session ID" in out
    assert len(mgr._sessions) == 1
    managed = next(iter(mgr._sessions.values()))

    for _ in range(50):
        if len(mgr._sessions) == 0:
            break
        await asyncio.sleep(0.05)

    assert len(mgr._sessions) == 0
    assert managed.master_fd is None
    assert managed.closed is True
