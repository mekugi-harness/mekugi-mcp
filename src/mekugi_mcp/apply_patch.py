"""Codex-compatible apply_patch parser and applier."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path


@dataclass
class PatchHunk:
    context_header: str | None
    old_lines: list[str]
    new_lines: list[str]
    end_of_file: bool = False


@dataclass
class _AddOp:
    rel: str
    target: Path
    content: str


@dataclass
class _DeleteOp:
    rel: str
    target: Path


@dataclass
class _UpdateOp:
    rel: str
    source: Path
    move_rel: str | None
    move_to: Path | None
    hunks: list[PatchHunk]


class PatchError(ValueError):
    """Raised when an apply_patch payload is malformed or fails to match."""


def _resolve_path(path_str: str, workdir: Path) -> Path:
    path = Path(path_str.strip())
    if not path.is_absolute():
        path = workdir / path
    return path.resolve()


def _lines_equal(left: list[str], right: list[str], *, strip_trailing: bool = False) -> bool:
    if len(left) != len(right):
        return False
    if not strip_trailing:
        return left == right
    return [line.rstrip() for line in left] == [line.rstrip() for line in right]


def _find_subsequence(
    lines: list[str],
    pattern: list[str],
    start: int,
    *,
    eof: bool = False,
) -> int | None:
    if not pattern:
        return len(lines)
    n = len(pattern)
    if eof:
        pos = len(lines) - n
        if pos < start or pos < 0:
            return None
        if _lines_equal(lines[pos:], pattern) or _lines_equal(
            lines[pos:], pattern, strip_trailing=True
        ):
            return pos
        return None
    for idx in range(start, len(lines) - n + 1):
        if _lines_equal(lines[idx : idx + n], pattern):
            return idx
    for idx in range(start, len(lines) - n + 1):
        if _lines_equal(lines[idx : idx + n], pattern, strip_trailing=True):
            return idx
    return None


def _apply_hunks(original: str, hunks: list[PatchHunk], file_label: str) -> str:
    newline = "\r\n" if "\r\n" in original else "\n"
    had_trailing_nl = original.endswith(("\r\n", "\n"))
    lines = original.splitlines()
    cursor = 0

    for hunk in hunks:
        search_start = cursor
        context_found_at: int | None = None
        if hunk.context_header:
            for idx in range(cursor, len(lines)):
                if hunk.context_header in lines[idx]:
                    context_found_at = idx
                    search_start = idx
                    break
            if context_found_at is None:
                raise PatchError(
                    f"Failed to find context header {hunk.context_header!r} in {file_label}"
                )

        if not hunk.old_lines:
            insert_pos = (
                len(lines)
                if hunk.end_of_file or context_found_at is None
                else context_found_at + 1
            )
            lines[insert_pos:insert_pos] = hunk.new_lines
            cursor = insert_pos + len(hunk.new_lines)
            continue

        pos = _find_subsequence(lines, hunk.old_lines, search_start, eof=hunk.end_of_file)
        if pos is None and not hunk.context_header and not hunk.end_of_file and search_start > 0:
            pos = _find_subsequence(lines, hunk.old_lines, 0, eof=False)
        if pos is None:
            raise PatchError(f"Failed to find context in {file_label}")

        lines[pos : pos + len(hunk.old_lines)] = hunk.new_lines
        cursor = pos + len(hunk.new_lines)

    result = newline.join(lines)
    if had_trailing_nl or (lines and not original):
        result += newline
    return result


def _parse_patch(patch_text: str, base: Path) -> list[_AddOp | _DeleteOp | _UpdateOp]:
    raw_lines = patch_text.splitlines()
    idx = 0

    while idx < len(raw_lines) and not raw_lines[idx].startswith("*** Begin Patch"):
        idx += 1
    if idx >= len(raw_lines):
        raise PatchError("Missing '*** Begin Patch' header")
    idx += 1

    ops: list[_AddOp | _DeleteOp | _UpdateOp] = []
    found_end = False

    while idx < len(raw_lines):
        line = raw_lines[idx]
        if line.startswith("*** End Patch"):
            found_end = True
            break

        if line.startswith("*** Add File: "):
            rel = line[len("*** Add File: ") :].strip()
            target = _resolve_path(rel, base)
            idx += 1
            added: list[str] = []
            while idx < len(raw_lines) and not raw_lines[idx].startswith("*** "):
                cur = raw_lines[idx]
                if not cur.startswith("+"):
                    raise PatchError(f"Expected '+' line in Add File for {rel}: {cur!r}")
                added.append(cur[1:])
                idx += 1
            content = "\n".join(added) + ("\n" if added else "")
            ops.append(_AddOp(rel=rel, target=target, content=content))
            continue

        if line.startswith("*** Delete File: "):
            rel = line[len("*** Delete File: ") :].strip()
            target = _resolve_path(rel, base)
            ops.append(_DeleteOp(rel=rel, target=target))
            idx += 1
            continue

        if line.startswith("*** Update File: "):
            rel = line[len("*** Update File: ") :].strip()
            source = _resolve_path(rel, base)
            idx += 1

            move_to: Path | None = None
            move_rel: str | None = None
            if idx < len(raw_lines) and raw_lines[idx].startswith("*** Move to: "):
                move_rel = raw_lines[idx][len("*** Move to: ") :].strip()
                move_to = _resolve_path(move_rel, base)
                idx += 1

            hunks: list[PatchHunk] = []
            while idx < len(raw_lines) and not raw_lines[idx].startswith("*** "):
                cur = raw_lines[idx]
                if cur.startswith("@@"):
                    header = cur[2:].strip() or None
                    idx += 1
                    old_lines: list[str] = []
                    new_lines: list[str] = []
                    eof = False
                    while idx < len(raw_lines):
                        hline = raw_lines[idx]
                        if hline.startswith(("@@", "*** ")):
                            if hline.startswith("*** End of File"):
                                eof = True
                                idx += 1
                            break
                        if hline.startswith(" "):
                            old_lines.append(hline[1:])
                            new_lines.append(hline[1:])
                        elif hline.startswith("-"):
                            old_lines.append(hline[1:])
                        elif hline.startswith("+"):
                            new_lines.append(hline[1:])
                        elif hline == "":
                            old_lines.append("")
                            new_lines.append("")
                        else:
                            raise PatchError(f"Unexpected hunk line in {rel}: {hline!r}")
                        idx += 1
                    hunks.append(
                        PatchHunk(
                            context_header=header,
                            old_lines=old_lines,
                            new_lines=new_lines,
                            end_of_file=eof,
                        )
                    )
                else:
                    raise PatchError(f"Expected '@@' hunk header in {rel}, got: {cur!r}")

            ops.append(
                _UpdateOp(
                    rel=rel,
                    source=source,
                    move_rel=move_rel,
                    move_to=move_to,
                    hunks=hunks,
                )
            )
            continue

        raise PatchError(f"Unexpected patch directive: {line!r}")

    if not found_end:
        raise PatchError("Missing '*** End Patch' footer")
    return ops


def _read_exact(path: Path) -> str:
    with path.open("r", encoding="utf-8", newline="") as f:
        return f.read()


def _write_exact(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="") as f:
        f.write(content)


def apply_patch(patch_text: str, workdir: str = "/workspace") -> str:
    """Atomically validate and apply a Codex `*** Begin Patch` ... `*** End Patch` patch."""
    base = Path(workdir).resolve()
    ops = _parse_patch(patch_text, base)

    # Compute all changes in memory before mutating disk so failed/truncated patches leave no partial edits.
    staged: dict[Path, str | None] = {}
    summary: list[str] = []

    def _current_content(path: Path) -> str | None:
        if path in staged:
            return staged[path]
        if path.exists():
            return _read_exact(path)
        return None

    for op in ops:
        if isinstance(op, _AddOp):
            staged[op.target] = op.content
            summary.append(f"A {op.rel}")
        elif isinstance(op, _DeleteOp):
            if _current_content(op.target) is None:
                raise PatchError(f"Cannot delete missing file: {op.rel}")
            staged[op.target] = None
            summary.append(f"D {op.rel}")
        else:
            original = _current_content(op.source)
            if original is None:
                raise PatchError(f"Cannot update missing file: {op.rel}")
            updated = _apply_hunks(original, op.hunks, op.rel) if op.hunks else original
            if op.move_to is not None and op.move_rel is not None:
                staged[op.move_to] = updated
                if op.move_to != op.source:
                    staged[op.source] = None
                summary.append(f"M {op.rel} -> {op.move_rel}")
            else:
                staged[op.source] = updated
                summary.append(f"M {op.rel}")

    for path, content in staged.items():
        if content is None:
            if path.exists():
                path.unlink()
        else:
            _write_exact(path, content)

    if not summary:
        return "Success. No files changed."
    return "Success. Updated the following files:\n" + "\n".join(summary)
