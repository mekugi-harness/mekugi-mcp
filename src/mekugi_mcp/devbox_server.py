"""Devbox HTTP agent supervising codex-code-mode-host and serving /exec and /wait."""

from __future__ import annotations

import asyncio
import contextlib
import hmac
import logging
import os
import platform
import socket
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

import uvicorn
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from mekugi_mcp._transport import DevboxGrpcServer
from mekugi_mcp.code_mode_client import MAX_CELL_YIELD_TIME_MS, CodeModeHostClient
from mekugi_mcp.code_mode_docs import load_extracted_docs_file

logger = logging.getLogger(__name__)


class HostSupervisor:
    """Optionally spawns and supervises `codex-code-mode-host` as a child process."""

    def __init__(self, binary_path: str | None, listen_addr: str) -> None:
        self.binary_path = binary_path
        self.listen_addr = listen_addr
        self._proc: asyncio.subprocess.Process | None = None
        self._lock = asyncio.Lock()

    @property
    def is_running(self) -> bool:
        if not self.binary_path:
            return True
        return self._proc is not None and self._proc.returncode is None

    async def start(self) -> None:
        await self.ensure_running()

    async def ensure_running(self) -> None:
        if not self.binary_path:
            return
        async with self._lock:
            if self._proc is not None and self._proc.returncode is None:
                return
            if not Path(self.binary_path).exists():
                raise FileNotFoundError(f"CODE_MODE_HOST_BIN not found: {self.binary_path}")

            listen_url = f"grpc://{self.listen_addr}"
            logger.info("Starting codex-code-mode-host: %s --listen %s", self.binary_path, listen_url)
            self._proc = await asyncio.create_subprocess_exec(
                self.binary_path,
                "--listen",
                listen_url,
            )

            host, port_str = self.listen_addr.rsplit(":", 1)
            port = int(port_str)
            for _ in range(100):
                if self._proc.returncode is not None:
                    raise RuntimeError(
                        f"codex-code-mode-host exited early with code {self._proc.returncode}"
                    )
                try:
                    _, writer = await asyncio.open_connection(host, port)
                    writer.close()
                    await writer.wait_closed()
                    return
                except OSError:
                    await asyncio.sleep(0.1)
            raise TimeoutError(f"codex-code-mode-host did not listen on {self.listen_addr} in time")

    async def stop(self) -> None:
        async with self._lock:
            if self._proc is None or self._proc.returncode is not None:
                return
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except TimeoutError:
                self._proc.kill()
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(self._proc.wait(), timeout=2.0)


def create_devbox_app(
    *,
    grpc_target: str | None = None,
    grpc_listen_addr: str | None = None,
    host_binary: str | None = None,
    workdir: str | None = None,
    default_shell: str | None = None,
    max_yield_time_ms: int | None = None,
    internal_token: str | None = None,
    docs_path: str | None = None,
) -> Starlette:
    target = grpc_target or os.environ.get("CODE_MODE_HOST_TARGET", "127.0.0.1:50051")
    resolved_grpc_listen = (
        grpc_listen_addr
        if grpc_listen_addr is not None
        else os.environ.get("DEVBOX_GRPC_ADDR", "0.0.0.0:50052").strip()
    )
    binary = host_binary if host_binary is not None else os.environ.get("CODE_MODE_HOST_BIN", "/usr/local/bin/codex-code-mode-host")
    cwd = workdir or os.environ.get("DEVBOX_WORKDIR", "/workspace")
    shell = default_shell or os.environ.get("DEVBOX_SHELL", "/bin/bash")
    max_yield = max_yield_time_ms or int(os.environ.get("MAX_CELL_YIELD_TIME_MS", str(MAX_CELL_YIELD_TIME_MS)))
    host_hostname = socket.gethostname()
    host_os = platform.system().lower()
    host_arch = platform.machine().lower()
    resolved_docs_path = (
        docs_path
        if docs_path is not None
        else os.environ.get("CODE_MODE_DOCS_PATH", "/usr/local/share/mekugi/code-mode-docs.json")
    )
    extracted_docs = load_extracted_docs_file(resolved_docs_path) if resolved_docs_path else None
    expected_token = (
        internal_token
        if internal_token is not None
        else os.environ.get("DEVBOX_INTERNAL_TOKEN", "").strip()
    )

    def _authorize(request: Request) -> JSONResponse | None:
        if not expected_token:
            return None
        auth_header = request.headers.get("authorization", "")
        prefix = "Bearer "
        if not auth_header.startswith(prefix):
            return JSONResponse({"error": "Unauthorized"}, status_code=401)
        provided = auth_header[len(prefix) :]
        if not hmac.compare_digest(provided, expected_token):
            return JSONResponse({"error": "Unauthorized"}, status_code=401)
        return None

    supervisor = HostSupervisor(binary or None, target)
    client = CodeModeHostClient(
        target=target,
        workdir=cwd,
        default_shell=shell,
        max_yield_time_ms=max_yield,
        ensure_host_running=supervisor.ensure_running,
    )

    class _DevboxGrpcHandler:
        """Async callback adapter bridged to the Rust tonic HTTP/2 gRPC server."""

        async def on_exec(
            self, code: str, yield_time_ms: int, max_output_tokens: int
        ) -> dict[str, Any]:
            if not isinstance(code, str) or not code.strip():
                raise ValueError("'code' must be a non-empty string")
            outcome = await client.execute(
                code,
                yield_time_ms=yield_time_ms,
                max_output_tokens=max_output_tokens,
            )
            return outcome.to_dict()

        async def on_wait(
            self,
            cell_id: str,
            yield_time_ms: int,
            max_output_tokens: int,
            terminate: bool,
        ) -> dict[str, Any]:
            if not isinstance(cell_id, str) or not cell_id.strip():
                raise ValueError("'cell_id' must be a non-empty string")
            outcome = await client.wait(
                cell_id,
                yield_time_ms=yield_time_ms,
                max_output_tokens=max_output_tokens,
                terminate=terminate,
            )
            return outcome.to_dict()

        async def on_info(self) -> dict[str, Any]:
            return {
                "status": "ok" if supervisor.is_running else "degraded",
                "hostname": host_hostname,
                "os": host_os,
                "arch": host_arch,
                "workdir": cwd,
                "session_id": client.session_id,
                "active_cells": client.active_cell_count,
                "active_shells": client.active_shell_count,
            }

        async def on_health(self) -> dict[str, Any]:
            if not supervisor.is_running:
                return {
                    "status": "degraded",
                    "session_id": client.session_id,
                    "workdir": cwd,
                    "error": "codex-code-mode-host is not running",
                    "active_cells": client.active_cell_count,
                    "active_shells": client.active_shell_count,
                }
            return {
                "status": "ok",
                "session_id": client.session_id,
                "workdir": cwd,
                "error": None,
                "active_cells": client.active_cell_count,
                "active_shells": client.active_shell_count,
            }

        async def on_docs(self) -> dict[str, str] | None:
            return extracted_docs.to_dict() if extracted_docs is not None else None

    grpc_server = (
        DevboxGrpcServer(
            resolved_grpc_listen,
            _DevboxGrpcHandler(),
            internal_token=expected_token or None,
        )
        if resolved_grpc_listen
        else None
    )

    @contextlib.asynccontextmanager
    async def lifespan(app: Starlette) -> AsyncIterator[None]:
        Path(cwd).mkdir(parents=True, exist_ok=True)
        await supervisor.start()
        await client.start()
        app.state.grpc_addr = None
        if grpc_server is not None:
            bound = await grpc_server.start()
            app.state.grpc_addr = bound
            logger.info("Rust DevboxAgent gRPC server listening on %s", bound)
        try:
            yield
        finally:
            if grpc_server is not None:
                await grpc_server.stop()
            await client.close()
            await supervisor.stop()

    async def health(_request: Request) -> JSONResponse:
        if not supervisor.is_running:
            return JSONResponse(
                {
                    "status": "degraded",
                    "error": "codex-code-mode-host is not running",
                    "hostname": host_hostname,
                    "os": host_os,
                    "arch": host_arch,
                    "workdir": cwd,
                    "active_cells": client.active_cell_count,
                    "active_shells": client.active_shell_count,
                },
                status_code=503,
            )
        return JSONResponse(
            {
                "status": "ok",
                "session_id": client.session_id,
                "hostname": host_hostname,
                "os": host_os,
                "arch": host_arch,
                "workdir": cwd,
                "active_cells": client.active_cell_count,
                "active_shells": client.active_shell_count,
            }
        )

    async def handle_exec(request: Request) -> JSONResponse:
        if (denied := _authorize(request)) is not None:
            return denied
        try:
            body = await request.json()
            code = body.get("code")
            if not isinstance(code, str) or not code.strip():
                return JSONResponse({"error": "'code' must be a non-empty string"}, status_code=400)
            outcome = await client.execute(
                code,
                yield_time_ms=body.get("yield_time_ms"),
                max_output_tokens=body.get("max_output_tokens"),
            )
            return JSONResponse(outcome.to_dict())
        except ValueError as exc:
            return JSONResponse({"error": str(exc)}, status_code=400)
        except Exception as exc:
            logger.exception("Error in /exec")
            return JSONResponse({"error": str(exc)}, status_code=500)

    async def handle_wait(request: Request) -> JSONResponse:
        if (denied := _authorize(request)) is not None:
            return denied
        try:
            body = await request.json()
            cell_id = body.get("cell_id")
            if not isinstance(cell_id, str) or not cell_id.strip():
                return JSONResponse({"error": "'cell_id' must be a non-empty string"}, status_code=400)
            outcome = await client.wait(
                cell_id,
                yield_time_ms=body.get("yield_time_ms"),
                max_output_tokens=body.get("max_output_tokens"),
                terminate=bool(body.get("terminate", False)),
            )
            return JSONResponse(outcome.to_dict())
        except ValueError as exc:
            return JSONResponse({"error": str(exc)}, status_code=400)
        except Exception as exc:
            logger.exception("Error in /wait")
            return JSONResponse({"error": str(exc)}, status_code=500)

    async def handle_code_mode_docs(request: Request) -> JSONResponse:
        if (denied := _authorize(request)) is not None:
            return denied
        if extracted_docs is None:
            return JSONResponse({"error": "Extracted code-mode docs not available"}, status_code=404)
        return JSONResponse(extracted_docs.to_dict())

    return Starlette(
        routes=[
            Route("/health", health, methods=["GET"]),
            Route("/code-mode-docs", handle_code_mode_docs, methods=["GET"]),
            Route("/exec", handle_exec, methods=["POST"]),
            Route("/wait", handle_wait, methods=["POST"]),
        ],
        lifespan=lifespan,
    )


def main() -> None:
    logging.basicConfig(
        level=os.environ.get("LOG_LEVEL", "INFO").upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    host = os.environ.get("DEVBOX_HOST", "0.0.0.0")
    port = int(os.environ.get("DEVBOX_PORT", "8080"))
    app = create_devbox_app()
    uvicorn.run(app, host=host, port=port, log_level="info")


if __name__ == "__main__":
    main()
