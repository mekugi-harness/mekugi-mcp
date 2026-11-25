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
    client = CodeModeHostClient(
        target=target,
        workdir=str(tmp_path),
        ensure_host_running=supervisor.ensure_running,
    )

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

        # 5. Host crash recovery: kill codex-code-mode-host and verify next execute restarts it
        assert supervisor._proc is not None
        supervisor._proc.kill()
        await supervisor._proc.wait()
        res6 = await client.execute('text("recovered");')
        assert res6.status == "completed"
        assert "recovered" in res6.text
    finally:
        await client.close()
        await supervisor.stop()


@pytest.mark.asyncio
async def test_fastmcp_to_devbox_full_stack(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("CODE_MODE_DOCS_PATH", raising=False)
    assert HOST_BIN is not None
    grpc_port = _free_port()
    http_port = _free_port()

    docs_file = tmp_path / "code-mode-docs.json"
    docs_file.write_text(
        '{"source_repo":"mekugi-harness/mekugi","source_ref":"test-rev",'
        '"exec_description_template":"Mekugi canonical exec ({{ default_exec_yield_time_ms }}ms) {{ image_helper }}",'
        '"wait_description_template":"Mekugi canonical wait",'
        '"image_helper_description":"img-helper"}',
        encoding="utf-8",
    )

    app = create_devbox_app(
        grpc_target=f"127.0.0.1:{grpc_port}",
        grpc_listen_addr="127.0.0.1:0",
        host_binary=HOST_BIN,
        workdir=str(tmp_path),
        internal_token="secret-internal-token",
        docs_path=str(docs_file),
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
            denied_docs = await http.get(f"http://127.0.0.1:{http_port}/code-mode-docs")
            assert denied_docs.status_code == 401

        mcp = await asyncio.to_thread(
            create_mcp_server,
            devbox_url=f"http://127.0.0.1:{http_port}",
            devbox_token="secret-internal-token",
            use_env_auth=False,
        )
        async with Client(mcp) as client:
            tools = {t.name: t for t in await client.list_tools()}
            assert "Mekugi canonical exec (10000ms) img-helper" in (tools["exec"].description or "")
            assert "Mekugi canonical wait" in (tools["wait"].description or "")
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


@pytest.mark.asyncio
async def test_fastmcp_multi_box_grpc_cluster_e2e(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Full multi-box e2e over Rust tonic HTTP/2 gRPC transport with concurrent non-blocking exec."""
    import json
    import time

    monkeypatch.delenv("CODE_MODE_DOCS_PATH", raising=False)
    assert HOST_BIN is not None

    dir_a = tmp_path / "box-alpha"
    dir_b = tmp_path / "box-beta"
    dir_a.mkdir()
    dir_b.mkdir()

    docs_file = tmp_path / "grpc-code-mode-docs.json"
    docs_file.write_text(
        '{"source_repo":"mekugi-harness/mekugi","source_ref":"grpc-rev",'
        '"exec_description_template":"gRPC cluster exec ({{ default_exec_yield_time_ms }}ms) {{ image_helper }}",'
        '"wait_description_template":"gRPC cluster wait",'
        '"image_helper_description":"img-helper"}',
        encoding="utf-8",
    )

    app_a = create_devbox_app(
        grpc_target=f"127.0.0.1:{_free_port()}",
        grpc_listen_addr="127.0.0.1:0",
        host_binary=HOST_BIN,
        workdir=str(dir_a),
        internal_token="token-alpha",
        docs_path=str(docs_file),
    )
    app_b = create_devbox_app(
        grpc_target=f"127.0.0.1:{_free_port()}",
        grpc_listen_addr="127.0.0.1:0",
        host_binary=HOST_BIN,
        workdir=str(dir_b),
        internal_token="token-beta",
        docs_path="",
    )

    srv_a = uvicorn.Server(
        uvicorn.Config(app_a, host="127.0.0.1", port=_free_port(), log_level="warning")
    )
    srv_b = uvicorn.Server(
        uvicorn.Config(app_b, host="127.0.0.1", port=_free_port(), log_level="warning")
    )
    task_a = asyncio.create_task(srv_a.serve())
    task_b = asyncio.create_task(srv_b.serve())

    try:
        for _ in range(100):
            if srv_a.started and srv_b.started:
                break
            await asyncio.sleep(0.05)
        assert srv_a.started and srv_b.started
        grpc_addr_a = app_a.state.grpc_addr
        grpc_addr_b = app_b.state.grpc_addr
        assert grpc_addr_a and grpc_addr_b

        boxes_json = json.dumps(
            {
                "default_box": "alpha",
                "boxes": [
                    {
                        "name": "alpha",
                        "target": f"grpc://{grpc_addr_a}",
                        "token": "token-alpha",
                        "description": "Primary box",
                        "labels": {"role": "primary"},
                        "pool_size": 1,
                    },
                    {
                        "name": "beta",
                        "target": f"grpc://{grpc_addr_b}",
                        "token": "token-beta",
                        "description": "Secondary box",
                        "labels": {"role": "worker"},
                        "pool_size": 2,
                    },
                    {
                        "name": "offline",
                        "target": "grpc://127.0.0.1:9",
                        "description": "Offline machine",
                    },
                ],
            }
        )

        # Verify gRPC auth rejects wrong token
        from mekugi_mcp._transport import ClusterTransport

        bad_transport = ClusterTransport.single_box(
            f"grpc://{grpc_addr_a}", box_name="alpha", token="wrong-token"
        )
        with pytest.raises(RuntimeError, match="Unauthenticated|Unauthorized"):
            await bad_transport.exec('text("nope");')

        mcp = create_mcp_server(
            boxes_json=boxes_json,
            use_env_auth=False,
        )

        # Verify /health returns 200 (default ok) without leaking internal box topology
        mcp_http = mcp.http_app(path="/mcp")
        async with (
            mcp_http.router.lifespan_context(mcp_http),
            httpx.AsyncClient(
                transport=httpx.ASGITransport(app=mcp_http),
                base_url="http://testserver",
            ) as http_client,
        ):
            h = await http_client.get("/health")
            assert h.status_code == 200
            hbody = h.json()
            assert hbody["status"] == "ok"
            assert hbody["default_box"] == "alpha"
            assert hbody["healthy_boxes"] == 2
            assert hbody["total_boxes"] == 3
            assert "boxes" not in hbody
            assert "devbox" not in hbody

        async with Client(mcp) as client:
            # 0. gRPC GetCodeModeDocs refreshes tool descriptions asynchronously on tools/list
            tools_map = {t.name: t for t in await client.list_tools()}
            assert "gRPC cluster exec (10000ms) img-helper" in (tools_map["exec"].description or "")
            assert "gRPC cluster wait" in (tools_map["wait"].description or "")

            # 1. list_boxes probes all boxes in parallel; offline box does not block healthy ones
            listed = await client.call_tool("list_boxes", {})
            info = json.loads(listed.content[0].text)  # type: ignore[union-attr]
            assert info["default_box"] == "alpha"
            by_name = {b["name"]: b for b in info["boxes"]}
            assert by_name["alpha"]["status"] == "ok"
            assert by_name["alpha"]["is_default"] is True
            assert by_name["alpha"]["transport"] == "grpc"
            assert by_name["alpha"]["workdir"] == str(dir_a)
            assert by_name["beta"]["status"] == "ok"
            assert by_name["beta"]["workdir"] == str(dir_b)
            assert by_name["offline"]["status"] == "unreachable"

            # 2. Default box routing (box omitted) -> alpha
            res_default = await client.call_tool(
                "exec",
                {"code": 'text(await tools.exec_command({ cmd: "pwd" }));'},
            )
            assert str(dir_a) in res_default.content[0].text  # type: ignore[union-attr]

            # 3. Explicit box="beta" -> beta
            res_beta = await client.call_tool(
                "exec",
                {
                    "box": "beta",
                    "code": 'text(await tools.exec_command({ cmd: "pwd" }));',
                },
            )
            assert str(dir_b) in res_beta.content[0].text  # type: ignore[union-attr]

            # 4. Yield on beta -> namespaced cell_id -> wait routes automatically without box
            yielded = await client.call_tool(
                "exec",
                {
                    "box": "beta",
                    "code": 'await new Promise(r => setTimeout(r, 350)); text("beta-after-wait");',
                    "yield_time_ms": 50,
                },
            )
            ytext = yielded.content[0].text  # type: ignore[union-attr]
            match = re.search(r"cell ID (\S+)", ytext)
            assert match is not None, f"Expected cell ID in: {ytext}"
            cell_id = match.group(1)
            assert cell_id.startswith("beta:")

            waited = await client.call_tool("wait", {"cell_id": cell_id, "yield_time_ms": 5000})
            assert "beta-after-wait" in waited.content[0].text  # type: ignore[union-attr]

            # 5. Concurrent slow + fast command on SAME single HTTP/2 channel (alpha) do not block
            slow_task = asyncio.create_task(
                client.call_tool(
                    "exec",
                    {
                        "box": "alpha",
                        "code": 'await new Promise(r => setTimeout(r, 900)); text("slow-ok");',
                        "yield_time_ms": 5000,
                    },
                )
            )
            await asyncio.sleep(0.05)
            t0 = time.monotonic()
            fast_res = await client.call_tool(
                "exec",
                {"box": "alpha", "code": 'text("fast-ok");'},
            )
            fast_elapsed = time.monotonic() - t0
            assert "fast-ok" in fast_res.content[0].text  # type: ignore[union-attr]
            assert not slow_task.done(), (
                f"Fast command did not finish before slow command ({fast_elapsed:.3f}s)"
            )

            slow_res = await slow_task
            assert "slow-ok" in slow_res.content[0].text  # type: ignore[union-attr]
    finally:
        srv_a.should_exit = True
        srv_b.should_exit = True
        await asyncio.gather(task_a, task_b)
