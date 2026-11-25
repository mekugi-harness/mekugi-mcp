"""Async gRPC client for Mekugi's codex-code-mode-host."""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import uuid
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any

import grpc

from mekugi_mcp.apply_patch import PatchError, apply_patch
from mekugi_mcp.pragma import parse_exec_source
from mekugi_mcp.proto import code_mode_v1_pb2 as pb
from mekugi_mcp.proto import code_mode_v1_pb2_grpc as pb_grpc
from mekugi_mcp.pty_manager import ProcessManager
from mekugi_mcp.tool_catalog import build_nested_tool_definitions

logger = logging.getLogger(__name__)

DEFAULT_CELL_YIELD_TIME_MS = 10_000
MAX_CELL_YIELD_TIME_MS = 45_000
DEFAULT_MAX_OUTPUT_TOKENS = 10_000


@dataclass
class FormattedOutcome:
    cell_id: str
    status: str  # completed | yielded | terminated | missing
    text: str
    more_output: bool = False
    error_text: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "cell_id": self.cell_id,
            "status": self.status,
            "text": self.text,
            "more_output": self.more_output,
            "error_text": self.error_text,
        }


def format_execution_outcome(
    outcome: pb.ExecutionOutcome,
    *,
    missing: bool = False,
) -> FormattedOutcome:
    """Render a gRPC ExecutionOutcome into Mekugi-style text."""
    text_parts: list[str] = []
    more_output = False
    continuation = False

    for item in outcome.content_items:
        kind = item.WhichOneof("item")
        if kind == "text":
            text_parts.append(item.text.text)
        elif kind == "image":
            text_parts.append(f"[image: {item.image.image_url[:64]}...]")
        elif kind == "audio":
            text_parts.append(f"[audio: {item.audio.audio_url[:64]}...]")
        elif kind == "page_boundary":
            more_output = more_output or item.page_boundary.more_output
            continuation = continuation or item.page_boundary.continuation

    body = "\n".join(text_parts)
    elapsed_s = outcome.code_mode_host_duration_ns / 1_000_000_000.0
    which = outcome.WhichOneof("outcome")

    if missing:
        status = "missing"
        header = f"Cell {outcome.cell_id} is not running or has already finished."
        error_text = None
    elif which == "yielded":
        status = "yielded"
        header = (
            f"Wall time: {elapsed_s:.4f} seconds\n"
            f"Script running with cell ID {outcome.cell_id}"
        )
        error_text = None
    elif which == "terminated":
        status = "terminated"
        header = (
            f"Wall time: {elapsed_s:.4f} seconds\n"
            f"Script terminated (cell ID {outcome.cell_id})"
        )
        error_text = None
    else:
        status = "completed"
        error_text = outcome.completed.error_text if outcome.completed.HasField("error_text") else None
        if error_text:
            header = (
                f"Wall time: {elapsed_s:.4f} seconds\n"
                f"Script failed:\n{error_text}"
            )
        else:
            header = f"Wall time: {elapsed_s:.4f} seconds\nScript completed"

    lines = [header]
    if body:
        label = "Output (continued):" if continuation else "Output:"
        lines.append(f"{label}\n{body}")
    if more_output:
        lines.append(f"More output available. Call wait with cell_id={outcome.cell_id!r}.")

    return FormattedOutcome(
        cell_id=outcome.cell_id,
        status=status,
        text="\n".join(lines),
        more_output=more_output,
        error_text=error_text,
    )


class CodeModeHostClient:
    """Manages a persistent gRPC session with `codex-code-mode-host`."""

    def __init__(
        self,
        target: str = "127.0.0.1:50051",
        *,
        workdir: str = "/workspace",
        default_shell: str = "/bin/bash",
        max_yield_time_ms: int = MAX_CELL_YIELD_TIME_MS,
        ensure_host_running: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self.target = target
        self.workdir = workdir
        self.max_yield_time_ms = max_yield_time_ms
        self._ensure_host_running = ensure_host_running
        self.process_manager = ProcessManager(default_workdir=workdir, default_shell=default_shell)
        self._nested_tools = build_nested_tool_definitions()

        self._channel: grpc.aio.Channel | None = None
        self._stub: pb_grpc.CodeModeHostStub | None = None
        self._session_id: str | None = None
        self._session_task: asyncio.Task[None] | None = None
        self._subscribe_task: asyncio.Task[None] | None = None
        self._inflight_calls: dict[str, asyncio.Task[None]] = {}
        self._session_lock = asyncio.Lock()
        self._catalog_installed = False
        self._supports_catalog_revision = False
        self._active_cells: set[str] = set()

    @property
    def session_id(self) -> str | None:
        return self._session_id

    @property
    def active_cell_count(self) -> int:
        return len(self._active_cells)

    @property
    def active_shell_count(self) -> int:
        return self.process_manager.active_session_count

    async def start(self) -> None:
        """Connect to codex-code-mode-host and open a session."""
        async with self._session_lock:
            if self._session_id is not None:
                return
            await self._open_session_locked()

    async def _open_session_locked(self) -> None:
        await self._close_streams_locked()
        if self._ensure_host_running is not None:
            await self._ensure_host_running()

        self._channel = grpc.aio.insecure_channel(
            self.target,
            options=[
                ("grpc.max_send_message_length", 32 * 1024 * 1024),
                ("grpc.max_receive_message_length", 32 * 1024 * 1024),
            ],
        )
        await asyncio.wait_for(self._channel.channel_ready(), timeout=10.0)
        self._stub = pb_grpc.CodeModeHostStub(self._channel)

        opened_event: asyncio.Event = asyncio.Event()
        open_error: list[BaseException] = []

        async def _session_loop() -> None:
            assert self._stub is not None
            try:
                stream = self._stub.OpenSession(
                    pb.OpenSessionRequest(
                        output_truncation=pb.OUTPUT_TRUNCATION_SERVER,
                        heartbeat_interval_s=15,
                    ),
                    wait_for_ready=True,
                )
                async for event in stream:
                    kind = event.WhichOneof("event")
                    if kind == "opened":
                        self._session_id = event.opened.session_id
                        self._catalog_installed = False
                        self._supports_catalog_revision = bool(
                            event.opened.supports_catalog_revision
                        )
                        opened_event.set()
                    elif kind == "tool_call_cancelled":
                        inv_id = event.tool_call_cancelled.invocation_id
                        task = self._inflight_calls.pop(inv_id, None)
                        if task is not None and not task.done():
                            task.cancel()
                    elif kind == "notification" and self._session_id is not None:
                        asyncio.create_task(
                            self._ack_notification(
                                self._session_id, event.notification.notification_id
                            )
                        )
                    elif kind == "cell_closed":
                        self._active_cells.discard(event.cell_closed.cell_id)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                logger.warning("codex-code-mode-host session stream ended: %s", exc)
                if not opened_event.is_set():
                    open_error.append(exc)
                    opened_event.set()
            finally:
                self._session_id = None
                self._active_cells.clear()

        self._session_task = asyncio.create_task(_session_loop())
        await asyncio.wait_for(opened_event.wait(), timeout=15.0)
        if open_error:
            raise RuntimeError(f"Failed to open codex-code-mode-host session: {open_error[0]}") from open_error[0]
        if self._session_id is None:
            raise RuntimeError("codex-code-mode-host closed OpenSession before sending SessionOpened")

        session_id = self._session_id

        async def _subscribe_loop() -> None:
            assert self._stub is not None
            try:
                stream = self._stub.SubscribeToToolCalls(
                    pb.SubscribeToToolCallsRequest(
                        session_id=session_id,
                        heartbeat_interval_s=15,
                    )
                )
                async for call in stream:
                    if call.HasField("heartbeat"):
                        continue
                    task = asyncio.create_task(self._handle_tool_call(call))
                    self._inflight_calls[call.invocation_id] = task
                    task.add_done_callback(
                        lambda _t, inv_id=call.invocation_id: self._inflight_calls.pop(inv_id, None)
                    )
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                logger.warning("SubscribeToToolCalls stream ended: %s", exc)

        self._subscribe_task = asyncio.create_task(_subscribe_loop())
        # Give subscription stream a moment to register on the host.
        await asyncio.sleep(0.05)

    async def _ack_notification(self, session_id: str, notification_id: str) -> None:
        if self._stub is None:
            return
        with contextlib.suppress(Exception):
            await self._stub.AcknowledgeNotification(
                pb.AcknowledgeNotificationRequest(
                    session_id=session_id,
                    notification_id=notification_id,
                )
            )

    async def _handle_tool_call(self, call: pb.ToolCall) -> None:
        assert self._stub is not None
        tool_name = call.tool_name.name
        raw_input = call.input_json if call.HasField("input_json") else b""

        try:
            args = json.loads(raw_input.decode("utf-8")) if raw_input else {}
        except Exception as exc:
            await self._complete_failed(call.session_id, call.invocation_id, f"Invalid tool input JSON: {exc}")
            return

        try:
            if tool_name == "exec_command":
                if not isinstance(args, dict) or "cmd" not in args:
                    raise ValueError("exec_command requires an object with 'cmd'")
                result = await self.process_manager.exec_command(
                    str(args["cmd"]),
                    workdir=args.get("workdir"),
                    shell=args.get("shell"),
                    login=bool(args.get("login", True)),
                    tty=bool(args.get("tty", False)),
                    yield_time_ms=args.get("yield_time_ms"),
                    max_output_tokens=args.get("max_output_tokens"),
                )
            elif tool_name == "write_stdin":
                if not isinstance(args, dict) or "session_id" not in args:
                    raise ValueError("write_stdin requires an object with 'session_id'")
                result = await self.process_manager.write_stdin(
                    int(args["session_id"]),
                    chars=str(args.get("chars", "")),
                    yield_time_ms=args.get("yield_time_ms"),
                    max_output_tokens=args.get("max_output_tokens"),
                )
            elif tool_name == "apply_patch":
                patch_str = args if isinstance(args, str) else str(args.get("patch", ""))
                result = await asyncio.to_thread(apply_patch, patch_str, workdir=self.workdir)
            else:
                raise ValueError(f"Unknown nested tool: {tool_name}")

            await self._stub.CompleteToolCall(
                pb.CompleteToolCallRequest(
                    session_id=call.session_id,
                    invocation_id=call.invocation_id,
                    succeeded=pb.ToolCallSucceeded(
                        output_json=json.dumps(result).encode("utf-8"),
                    ),
                )
            )
        except asyncio.CancelledError:
            raise
        except (PatchError, ValueError, OSError) as exc:
            await self._complete_failed(call.session_id, call.invocation_id, str(exc))
        except Exception as exc:
            logger.exception("Nested tool %s failed", tool_name)
            await self._complete_failed(call.session_id, call.invocation_id, f"Internal error: {exc}")

    async def _complete_failed(self, session_id: str, invocation_id: str, message: str) -> None:
        if self._stub is None:
            return
        with contextlib.suppress(Exception):
            await self._stub.CompleteToolCall(
                pb.CompleteToolCallRequest(
                    session_id=session_id,
                    invocation_id=invocation_id,
                    failed=pb.ToolCallFailed(message=message),
                )
            )

    async def _ensure_session(self) -> str:
        if self._session_id is not None:
            return self._session_id
        await self.start()
        assert self._session_id is not None
        return self._session_id

    def _clamp_yield_ms(self, yield_time_ms: int | None) -> int:
        val = DEFAULT_CELL_YIELD_TIME_MS if yield_time_ms is None else int(yield_time_ms)
        return max(0, min(val, self.max_yield_time_ms))

    async def execute(
        self,
        code: str,
        *,
        yield_time_ms: int | None = None,
        max_output_tokens: int | None = None,
    ) -> FormattedOutcome:
        parsed = parse_exec_source(code)
        effective_yield_ms = self._clamp_yield_ms(
            yield_time_ms if yield_time_ms is not None else parsed.yield_time_ms
        )
        requested_tokens = (
            int(max_output_tokens)
            if (max_output_tokens is not None and int(max_output_tokens) > 0)
            else (parsed.max_output_tokens or DEFAULT_MAX_OUTPUT_TOKENS)
        )
        effective_max_tokens = max(100, requested_tokens)

        for attempt in range(2):
            session_id = await self._ensure_session()
            assert self._stub is not None
            execution_id = f"exec-{uuid.uuid4().hex[:12]}"
            tool_call_id = f"call-{uuid.uuid4().hex[:12]}"

            reuse_catalog = self._supports_catalog_revision and self._catalog_installed
            request_kwargs: dict[str, Any] = {
                "session_id": session_id,
                "execution_id": execution_id,
                "tool_call_id": tool_call_id,
                "source": parsed.code,
                "enabled_tools": [] if reuse_catalog else self._nested_tools,
                "yield_time_ms": effective_yield_ms,
                "max_output_tokens": effective_max_tokens,
            }
            if self._supports_catalog_revision:
                request_kwargs["catalog_revision"] = 1
            request = pb.ExecuteRequest(**request_kwargs)

            try:
                outcome: pb.ExecutionOutcome | None = None
                async for event in self._stub.Execute(request):
                    which = event.WhichOneof("event")
                    if which == "started":
                        self._catalog_installed = True
                        if event.started.cell_id:
                            self._active_cells.add(event.started.cell_id)
                    elif which == "outcome":
                        outcome = event.outcome
                if outcome is None:
                    raise RuntimeError("codex-code-mode-host ended Execute stream without outcome")
                formatted = format_execution_outcome(outcome)
                if formatted.status in ("completed", "terminated", "missing"):
                    self._active_cells.discard(formatted.cell_id)
                return formatted
            except grpc.aio.AioRpcError as exc:
                if attempt == 0 and exc.code() in (
                    grpc.StatusCode.NOT_FOUND,
                    grpc.StatusCode.UNAVAILABLE,
                ):
                    logger.warning("Reopening code-mode session after gRPC error: %s", exc.code())
                    async with self._session_lock:
                        self._session_id = None
                    continue
                raise

        raise RuntimeError("Failed to execute code after reconnecting")

    async def wait(
        self,
        cell_id: str,
        *,
        yield_time_ms: int | None = None,
        max_output_tokens: int | None = None,
        terminate: bool = False,
    ) -> FormattedOutcome:
        session_id = await self._ensure_session()
        assert self._stub is not None
        effective_max_tokens = (
            DEFAULT_MAX_OUTPUT_TOKENS if max_output_tokens is None else max(100, int(max_output_tokens))
        )

        if terminate:
            resp = await self._stub.Terminate(
                pb.TerminateRequest(
                    session_id=session_id,
                    cell_id=cell_id,
                    max_output_tokens=effective_max_tokens,
                    discard_output=False,
                )
            )
        else:
            effective_yield_ms = self._clamp_yield_ms(yield_time_ms)
            resp = await self._stub.Wait(
                pb.WaitRequest(
                    session_id=session_id,
                    cell_id=cell_id,
                    wait_id=f"wait-{uuid.uuid4().hex[:12]}",
                    yield_time_ms=effective_yield_ms,
                    max_output_tokens=effective_max_tokens,
                )
            )

        state = resp.WhichOneof("state")
        if state == "missing_cell":
            self._active_cells.discard(cell_id)
            return format_execution_outcome(resp.missing_cell, missing=True)
        formatted = format_execution_outcome(resp.live_cell, missing=False)
        if formatted.status in ("completed", "terminated", "missing"):
            self._active_cells.discard(formatted.cell_id)
        return formatted

    async def _close_streams_locked(self) -> None:
        for task in list(self._inflight_calls.values()):
            if not task.done():
                task.cancel()
        self._inflight_calls.clear()

        for task in (self._subscribe_task, self._session_task):
            if task is not None and not task.done():
                task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await task
        self._subscribe_task = None
        self._session_task = None

        if self._channel is not None:
            await self._channel.close()
            self._channel = None
        self._stub = None
        self._session_id = None
        self._catalog_installed = False
        self._active_cells.clear()

    async def close(self) -> None:
        async with self._session_lock:
            if self._stub is not None and self._session_id is not None:
                with contextlib.suppress(Exception):
                    await self._stub.CloseSession(pb.CloseSessionRequest(session_id=self._session_id))
            await self._close_streams_locked()
        await self.process_manager.close_all()
