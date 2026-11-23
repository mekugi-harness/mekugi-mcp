"""End-to-end integration tests against a real codex-code-mode-host binary."""

from __future__ import annotations

import asyncio
import os
import re
import socket
from pathlib import Path

import httpx
import pytest
import uvicorn
from fastmcp import Client

from mekugi_mcp.code_mode_client import CodeModeHostClient
from mekugi_mcp.devbox_server import HostSupervisor, create_devbox_app
from mekugi_mcp.server import create_mcp_server


def _find_binary() -> str | None:
    for candidate in (
        os.environ.get("CODE_MODE_HOST_BIN"),
        "/tmp/mekugi-release/codex-code-mode-host",
        "/usr/local/bin/codex-code-mode-host",
    ):
        if candidate and Path(candidate).exists():
            return candidate
    return None


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


HOST_BIN = _find_binary()
pytestmark = pytest.mark.skipif(
    HOST_BIN is None,
    reason="codex-code-mode-host binary not available",
)


@pytest.mark.asyncio
async def test_code_mode_host_direct_e2e(tmp_path: Path) -> None:
    assert HOST_BIN is not None
    grpc_port = _free_port()
    target = f"127.0.0.1:{grpc_port}"
    supervisor = HostSupervisor(HOST_BIN, target)
    client = CodeModeHostClient(target=target, workdir=str(tmp_path))

    await supervisor.start()
    try:
        await client.start()

        # 1. Pure JS + store()
        res = await client.execute('store("answer", 42); text("sum=" + (2 + 2));')
        assert res.status == "completed"
        assert res.error_text is None
        assert "sum=4" in res.text

        # 2. load() across cells + exec_command
        res2 = await client.execute(
            'const out = await tools.exec_command({ cmd: "echo hello-from-devbox && pwd" });'
            'text("loaded=" + load("answer") + "\\n" + out);'
        )
        assert res2.status == "completed"
        assert "loaded=42" in res2.text
        assert "hello-from-devbox" in res2.text
        assert str(tmp_path) in res2.text

        # 3. apply_patch inside V8 isolate
        patch_js = (
            'const patch = `*** Begin Patch\n'
            '*** Add File: demo.txt\n'
            '+patched from v8\n'
            '*** End Patch`;\n'
            'text(await tools.apply_patch(patch));'
        )
        res3 = await client.execute(patch_js)
        assert res3.status == "completed"
        assert "A demo.txt" in res3.text
        assert (tmp_path / "demo.txt").read_text() == "patched from v8\n"

        # 4. Yielding cell + wait(terminate=True)
        res4 = await client.execute(
            'await new Promise(r => setTimeout(r, 30000)); text("never");',
            yield_time_ms=150,
        )
        assert res4.status == "yielded"
        assert res4.cell_id

        res5 = await client.wait(res4.cell_id, terminate=True)
        assert res5.status == "terminated"
    finally:
        await client.close()
        await supervisor.stop()


@pytest.mark.asyncio
async def test_fastmcp_to_devbox_full_stack(tmp_path: Path) -> None:
    assert HOST_BIN is not None
    grpc_port = _free_port()
    http_port = _free_port()

    app = create_devbox_app(
        grpc_target=f"127.0.0.1:{grpc_port}",
        host_binary=HOST_BIN,
        workdir=str(tmp_path),
        internal_token="secret-internal-token",
    )
    config = uvicorn.Config(app, host="127.0.0.1", port=http_port, log_level="warning")
    server = uvicorn.Server(config)
    server_task = asyncio.create_task(server.serve())

    try:
        for _ in range(100):
            if server.started:
                break
            await asyncio.sleep(0.05)
        assert server.started

        async with httpx.AsyncClient() as http:
            denied = await http.post(f"http://127.0.0.1:{http_port}/exec", json={"code": "text('x');"})
            assert denied.status_code == 401

        mcp = create_mcp_server(
            devbox_url=f"http://127.0.0.1:{http_port}",
            devbox_token="secret-internal-token",
            use_env_auth=False,
        )
        async with Client(mcp) as client:
            result = await client.call_tool(
                "exec",
                {
                    "code": 'const r = await tools.exec_command({ cmd: "printf ok-mcp" }); text(r);',
                },
            )
            content = result.content[0].text  # type: ignore[union-attr]
            assert "ok-mcp" in content

            # Yield and then wait for completion
            yielded = await client.call_tool(
                "exec",
                {
                    "code": 'await new Promise(r => setTimeout(r, 400)); text("done-after-wait");',
                    "yield_time_ms": 50,
                },
            )
            ytext = yielded.content[0].text  # type: ignore[union-attr]
            match = re.search(r"cell ID (\S+)", ytext)
            assert match is not None, f"Expected cell ID in: {ytext}"
            cell_id = match.group(1)

            waited = await client.call_tool("wait", {"cell_id": cell_id, "yield_time_ms": 5000})
            wtext = waited.content[0].text  # type: ignore[union-attr]
            assert "done-after-wait" in wtext
    finally:
        server.should_exit = True
        await server_task
