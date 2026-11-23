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


class PatchError(ValueError):
    """Raised when an apply_patch payload is malformed or fails to match."""


def _resolve_path(path_str: str, workdir: Path) -> Path:
    path = Path(path_str.strip())
    if not path.is_absolute():
        path = workdir / path
    return path.resolve()


def _find_subsequence(lines: list[str], pattern: list[str], start: int) -> int | None:
    if not pattern:
        return start
    n = len(pattern)
    for idx in range(start, len(lines) - n + 1):
        if lines[idx : idx + n] == pattern:
            return idx
    # Fallback: match ignoring trailing whitespace.
    stripped = [line.rstrip() for line in pattern]
    for idx in range(start, len(lines) - n + 1):
        if [line.rstrip() for line in lines[idx : idx + n]] == stripped:
            return idx
    return None


def _apply_hunks(original: str, hunks: list[PatchHunk], file_label: str) -> str:
    had_trailing_nl = original.endswith("\n")
    lines = original.splitlines()
    cursor = 0

    for hunk in hunks:
        search_start = cursor
        if hunk.context_header:
            for idx in range(cursor, len(lines)):
                if hunk.context_header in lines[idx]:
                    search_start = idx
                    break

        pos = _find_subsequence(lines, hunk.old_lines, search_start)
        if pos is None and search_start > 0:
            pos = _find_subsequence(lines, hunk.old_lines, 0)
        if pos is None:
            raise PatchError(f"Failed to find context in {file_label}")

        lines[pos : pos + len(hunk.old_lines)] = hunk.new_lines
        cursor = pos + len(hunk.new_lines)

    result = "\n".join(lines)
    if had_trailing_nl or (lines and not original):
        result += "\n"
    return result


def apply_patch(patch_text: str, workdir: str = "/workspace") -> str:
    """Apply a Codex `*** Begin Patch` ... `*** End Patch` patch."""
    base = Path(workdir).resolve()
    raw_lines = patch_text.splitlines()
    idx = 0

    while idx < len(raw_lines) and not raw_lines[idx].startswith("*** Begin Patch"):
        idx += 1
    if idx >= len(raw_lines):
        raise PatchError("Missing '*** Begin Patch' header")
    idx += 1

    summary: list[str] = []

    while idx < len(raw_lines):
        line = raw_lines[idx]
        if line.startswith("*** End Patch"):
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
            target.parent.mkdir(parents=True, exist_ok=True)
            content = "\n".join(added) + ("\n" if added else "")
            target.write_text(content, encoding="utf-8")
            summary.append(f"A {rel}")
            continue

        if line.startswith("*** Delete File: "):
            rel = line[len("*** Delete File: ") :].strip()
            target = _resolve_path(rel, base)
            if not target.exists():
                raise PatchError(f"Cannot delete missing file: {rel}")
            target.unlink()
            summary.append(f"D {rel}")
            idx += 1
            continue

        if line.startswith("*** Update File: "):
            rel = line[len("*** Update File: ") :].strip()
            source = _resolve_path(rel, base)
            if not source.exists():
                raise PatchError(f"Cannot update missing file: {rel}")
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

            original = source.read_text(encoding="utf-8")
            updated = _apply_hunks(original, hunks, rel) if hunks else original
            if move_to is not None and move_rel is not None:
                move_to.parent.mkdir(parents=True, exist_ok=True)
                move_to.write_text(updated, encoding="utf-8")
                if move_to != source:
                    source.unlink()
                summary.append(f"M {rel} -> {move_rel}")
            else:
                source.write_text(updated, encoding="utf-8")
                summary.append(f"M {rel}")
            continue

        raise PatchError(f"Unexpected patch directive: {line!r}")

    if not summary:
        return "Success. No files changed."
    return "Success. Updated the following files:\n" + "\n".join(summary)
