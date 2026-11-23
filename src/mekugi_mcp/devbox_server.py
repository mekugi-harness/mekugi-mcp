"""Devbox HTTP agent supervising codex-code-mode-host and serving /exec and /wait."""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
from collections.abc import AsyncIterator
from pathlib import Path

import uvicorn
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from mekugi_mcp.code_mode_client import MAX_CELL_YIELD_TIME_MS, CodeModeHostClient

logger = logging.getLogger(__name__)


class HostSupervisor:
    """Optionally spawns and supervises `codex-code-mode-host` as a child process."""

    def __init__(self, binary_path: str | None, listen_addr: str) -> None:
        self.binary_path = binary_path
        self.listen_addr = listen_addr
        self._proc: asyncio.subprocess.Process | None = None

    async def start(self) -> None:
        if not self.binary_path:
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
    host_binary: str | None = None,
    workdir: str | None = None,
    default_shell: str | None = None,
    max_yield_time_ms: int | None = None,
) -> Starlette:
    target = grpc_target or os.environ.get("CODE_MODE_HOST_TARGET", "127.0.0.1:50051")
    binary = host_binary if host_binary is not None else os.environ.get("CODE_MODE_HOST_BIN", "/usr/local/bin/codex-code-mode-host")
    cwd = workdir or os.environ.get("DEVBOX_WORKDIR", "/workspace")
    shell = default_shell or os.environ.get("DEVBOX_SHELL", "/bin/bash")
    max_yield = max_yield_time_ms or int(os.environ.get("MAX_CELL_YIELD_TIME_MS", str(MAX_CELL_YIELD_TIME_MS)))

    supervisor = HostSupervisor(binary or None, target)
    client = CodeModeHostClient(
        target=target,
        workdir=cwd,
        default_shell=shell,
        max_yield_time_ms=max_yield,
    )

    @contextlib.asynccontextmanager
    async def lifespan(_app: Starlette) -> AsyncIterator[None]:
        Path(cwd).mkdir(parents=True, exist_ok=True)
        await supervisor.start()
        await client.start()
        try:
            yield
        finally:
            await client.close()
            await supervisor.stop()

    async def health(_request: Request) -> JSONResponse:
        return JSONResponse(
            {
                "status": "ok",
                "session_id": client.session_id,
                "workdir": cwd,
            }
        )

    async def handle_exec(request: Request) -> JSONResponse:
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

    return Starlette(
        routes=[
            Route("/health", health, methods=["GET"]),
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
