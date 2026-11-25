"""Shell and interactive PTY process manager for nested exec_command / write_stdin tools."""

from __future__ import annotations

import asyncio
import contextlib
import errno
import os
import pty
import signal
import time
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_YIELD_TIME_MS = 10_000
MAX_SHELL_YIELD_TIME_MS = 40_000
DEFAULT_MAX_OUTPUT_TOKENS = 10_000
CHARS_PER_TOKEN = 4
MAX_BUFFER_BYTES = 2 * 1024 * 1024
EXITED_SESSION_TTL_S = 600.0


def truncate_output(text: str, max_tokens: int) -> str:
    """Truncate text to an approximate token budget, preserving head and tail."""
    max_chars = max(256, max_tokens * CHARS_PER_TOKEN)
    if len(text) <= max_chars:
        return text
    head = max_chars // 2
    tail = max_chars - head
    omitted = len(text) - max_chars
    return f"{text[:head]}\n...[{omitted} chars truncated]...\n{text[-tail:]}"


@dataclass
class ManagedProcess:
    session_id: int
    process: asyncio.subprocess.Process
    master_fd: int | None
    tty: bool
    started_at: float = field(default_factory=time.monotonic)
    buffer: bytearray = field(default_factory=bytearray)
    data_event: asyncio.Event = field(default_factory=asyncio.Event)
    reader_task: asyncio.Task[None] | None = None
    reaper_task: asyncio.Task[None] | None = None
    closed: bool = False

    def append_output(self, chunk: bytes) -> None:
        if not chunk:
            return
        self.buffer.extend(chunk)
        if len(self.buffer) > MAX_BUFFER_BYTES:
            del self.buffer[: len(self.buffer) - MAX_BUFFER_BYTES]
        self.data_event.set()

    def drain_output(self) -> str:
        if not self.buffer:
            return ""
        data = bytes(self.buffer)
        self.buffer.clear()
        self.data_event.clear()
        return data.decode("utf-8", errors="replace")

    async def write_input(self, chars: str) -> None:
        if not chars:
            return
        data = chars.encode("utf-8")
        if self.master_fd is not None:
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, os.write, self.master_fd, data)
        elif self.process.stdin is not None and not self.process.stdin.is_closing():
            self.process.stdin.write(data)
            await self.process.stdin.drain()

    async def terminate(self) -> None:
        if self.process.returncode is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, signal.SIGTERM)
            try:
                await asyncio.wait_for(self.process.wait(), timeout=2.0)
            except TimeoutError:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(self.process.pid, signal.SIGKILL)
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(self.process.wait(), timeout=2.0)
        if self.reader_task is not None and not self.reader_task.done():
            self.reader_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self.reader_task
        if self.reaper_task is not None and not self.reaper_task.done():
            self.reaper_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self.reaper_task
        if self.master_fd is not None:
            with contextlib.suppress(OSError):
                os.close(self.master_fd)
            self.master_fd = None
        self.buffer.clear()
        self.closed = True


class ProcessManager:
    """Manages interactive and one-shot shell sessions inside the devbox."""

    def __init__(self, default_workdir: str = "/workspace", default_shell: str = "/bin/bash") -> None:
        self.default_workdir = default_workdir
        self.default_shell = default_shell
        self._next_session_id = 1000
        self._sessions: dict[int, ManagedProcess] = {}
        self._session_tmp = Path(os.environ.get("SESSION_TMP", "/tmp/mekugi-session"))
        self._session_tmp.mkdir(parents=True, exist_ok=True)

    @property
    def active_session_count(self) -> int:
        return sum(1 for s in self._sessions.values() if s.process.returncode is None)

    def _resolve_workdir(self, workdir: str | None) -> str:
        if not workdir:
            base = Path(self.default_workdir)
            base.mkdir(parents=True, exist_ok=True)
            return str(base)
        path = Path(workdir)
        if not path.is_absolute():
            path = Path(self.default_workdir) / path
        return str(path.resolve())

    def _build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.setdefault("TERM", "xterm-256color")
        env["SESSION_TMP"] = str(self._session_tmp)
        return env

    async def exec_command(
        self,
        cmd: str,
        *,
        workdir: str | None = None,
        shell: str | None = None,
        login: bool = True,
        tty: bool = False,
        yield_time_ms: int | None = None,
        max_output_tokens: int | None = None,
    ) -> str:
        cwd = self._resolve_workdir(workdir)
        if not Path(cwd).exists():
            return f"Error: working directory does not exist: {cwd}"

        shell_bin = shell or self.default_shell
        flag = "-lc" if login else "-c"
        argv = [shell_bin, flag, cmd]
        env = self._build_env()

        session_id = self._next_session_id
        self._next_session_id += 1

        master_fd: int | None = None
        slave_fd: int | None = None
        try:
            if tty:
                master_fd, slave_fd = pty.openpty()
                proc = await asyncio.create_subprocess_exec(
                    *argv,
                    stdin=slave_fd,
                    stdout=slave_fd,
                    stderr=slave_fd,
                    cwd=cwd,
                    env=env,
                    start_new_session=True,
                )
                os.close(slave_fd)
                slave_fd = None
            else:
                proc = await asyncio.create_subprocess_exec(
                    *argv,
                    stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.STDOUT,
                    cwd=cwd,
                    env=env,
                    start_new_session=True,
                )
        except Exception as exc:
            if slave_fd is not None:
                with contextlib.suppress(OSError):
                    os.close(slave_fd)
            if master_fd is not None:
                with contextlib.suppress(OSError):
                    os.close(master_fd)
            return f"Error starting command: {exc}"

        managed = ManagedProcess(
            session_id=session_id,
            process=proc,
            master_fd=master_fd,
            tty=tty,
        )
        managed.reader_task = asyncio.create_task(self._read_loop(managed))
        self._sessions[session_id] = managed

        wait_ms = DEFAULT_YIELD_TIME_MS if yield_time_ms is None else max(0, min(int(yield_time_ms), MAX_SHELL_YIELD_TIME_MS))
        token_budget = DEFAULT_MAX_OUTPUT_TOKENS if max_output_tokens is None else max(100, int(max_output_tokens))

        try:
            return await self._collect(managed, wait_ms=wait_ms, max_output_tokens=token_budget)
        except asyncio.CancelledError:
            self._sessions.pop(session_id, None)
            await managed.terminate()
            raise

    async def write_stdin(
        self,
        session_id: int,
        *,
        chars: str = "",
        yield_time_ms: int | None = None,
        max_output_tokens: int | None = None,
    ) -> str:
        managed = self._sessions.get(int(session_id))
        if managed is None:
            return f"Error: unknown session_id {session_id}"

        if chars:
            try:
                await managed.write_input(chars)
            except OSError as exc:
                return f"Error writing to session {session_id}: {exc}"

        wait_ms = 250 if yield_time_ms is None else max(0, min(int(yield_time_ms), MAX_SHELL_YIELD_TIME_MS))
        token_budget = DEFAULT_MAX_OUTPUT_TOKENS if max_output_tokens is None else max(100, int(max_output_tokens))
        return await self._collect(managed, wait_ms=wait_ms, max_output_tokens=token_budget)

    async def _read_loop(self, managed: ManagedProcess) -> None:
        loop = asyncio.get_running_loop()
        try:
            if managed.master_fd is not None:
                while True:
                    try:
                        chunk = await loop.run_in_executor(None, os.read, managed.master_fd, 8192)
                    except OSError as exc:
                        if exc.errno == errno.EIO:
                            break
                        raise
                    if not chunk:
                        break
                    managed.append_output(chunk)
            elif managed.process.stdout is not None:
                while True:
                    chunk = await managed.process.stdout.read(8192)
                    if not chunk:
                        break
                    managed.append_output(chunk)
        except asyncio.CancelledError:
            raise
        except Exception:
            pass
        finally:
            if managed.master_fd is not None:
                with contextlib.suppress(OSError):
                    os.close(managed.master_fd)
                managed.master_fd = None
            managed.data_event.set()
            if managed.session_id in self._sessions and managed.reaper_task is None:
                managed.reaper_task = asyncio.create_task(self._reap_exited(managed))

    async def _reap_exited(self, managed: ManagedProcess) -> None:
        try:
            await managed.process.wait()
            await asyncio.sleep(EXITED_SESSION_TTL_S)
            if self._sessions.get(managed.session_id) is managed:
                self._sessions.pop(managed.session_id, None)
                managed.buffer.clear()
                managed.closed = True
        except asyncio.CancelledError:
            raise

    async def _collect(
        self,
        managed: ManagedProcess,
        *,
        wait_ms: int,
        max_output_tokens: int,
    ) -> str:
        deadline = time.monotonic() + (wait_ms / 1000.0)
        wait_task = asyncio.create_task(managed.process.wait())
        try:
            remaining = deadline - time.monotonic()
            if remaining > 0:
                await asyncio.wait({wait_task}, timeout=remaining)
            if not wait_task.done():
                wait_task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await wait_task
        finally:
            if (
                wait_task.done()
                and not wait_task.cancelled()
                and managed.reader_task is not None
                and not managed.reader_task.done()
            ):
                # Drain any final bytes from the reader task after process exit.
                with contextlib.suppress(TimeoutError, asyncio.CancelledError):
                    await asyncio.wait_for(asyncio.shield(managed.reader_task), timeout=0.5)

        elapsed = time.monotonic() - managed.started_at
        output = truncate_output(managed.drain_output(), max_output_tokens)

        if managed.process.returncode is not None:
            exit_code = managed.process.returncode
            self._sessions.pop(managed.session_id, None)
            if managed.reaper_task is not None and not managed.reaper_task.done():
                managed.reaper_task.cancel()
            if managed.master_fd is not None:
                with contextlib.suppress(OSError):
                    os.close(managed.master_fd)
                managed.master_fd = None
            return (
                f"Wall time: {elapsed:.4f} seconds\n"
                f"Process exited with code {exit_code}\n"
                f"Output:\n{output}"
            )

        return (
            f"Wall time: {elapsed:.4f} seconds\n"
            f"Process running with session ID {managed.session_id}\n"
            f"Output:\n{output}"
        )

    async def close_all(self) -> None:
        sessions = list(self._sessions.values())
        self._sessions.clear()
        for managed in sessions:
            await managed.terminate()
