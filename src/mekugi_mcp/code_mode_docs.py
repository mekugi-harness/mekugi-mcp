"""Build-time extractor and runtime renderer for Mekugi's canonical code-mode instructions."""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

from mekugi_mcp.tool_catalog import EXEC_TOOL_DESCRIPTION, WAIT_TOOL_DESCRIPTION

logger = logging.getLogger(__name__)

DEFAULT_CODEX_REPO = "yusing/codex"
DESCRIPTION_RS_PATH = "codex-rs/code-mode-protocol/src/description.rs"

NESTED_TOOLS_SECTION = """Nested tools available on `tools`:
- `await tools.exec_command({ cmd: string, workdir?: string, shell?: string, login?: boolean, tty?: boolean, yield_time_ms?: number, max_output_tokens?: number }): Promise<string>`
- `await tools.write_stdin({ session_id: number, chars?: string, yield_time_ms?: number, max_output_tokens?: number }): Promise<string>`
- `await tools.apply_patch(patch: string): Promise<string>` (Codex `*** Begin Patch` ... `*** End Patch` format)"""

GEMINI_YIELD_CAP_NOTE = (
    "Note: `yield_time_ms` is capped at 45000 ms to stay within Gemini Web's tool timeout; "
    "if still running, continue with `wait` using the returned `cell_id`."
)

WAIT_MCP_ADDENDUM = (
    "- Set `terminate: true` to stop the running cell.\n"
    "- Unread output from exited shell sessions is retained for 10 minutes."
)


@dataclass(frozen=True)
class ExtractedCodeModeDocs:
    source_repo: str
    source_ref: str
    exec_description_template: str
    wait_description_template: str
    image_helper_description: str

    def to_dict(self) -> dict[str, str]:
        return {
            "source_repo": self.source_repo,
            "source_ref": self.source_ref,
            "exec_description_template": self.exec_description_template,
            "wait_description_template": self.wait_description_template,
            "image_helper_description": self.image_helper_description,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ExtractedCodeModeDocs:
        return cls(
            source_repo=str(data["source_repo"]),
            source_ref=str(data["source_ref"]),
            exec_description_template=str(data["exec_description_template"]),
            wait_description_template=str(data["wait_description_template"]),
            image_helper_description=str(data["image_helper_description"]),
        )


def extract_rust_raw_str_const(source: str, const_name: str) -> str:
    """Extract a Rust `const NAME: &str = r#\"...\"#;` (any number of `#` delimiters)."""
    pattern = re.compile(
        rf"(?:pub\s+)?const\s+{re.escape(const_name)}\s*:\s*&str\s*=\s*r(#*)\"",
    )
    match = pattern.search(source)
    if match is None:
        raise ValueError(f"Could not find Rust raw string constant {const_name!r}")
    hashes = match.group(1)
    start = match.end()
    closing = f'"{hashes};'
    end = source.find(closing, start)
    if end == -1:
        raise ValueError(f"Unterminated Rust raw string constant {const_name!r}")
    return source[start:end]


def parse_description_rs(
    source: str,
    *,
    source_repo: str,
    source_ref: str,
) -> ExtractedCodeModeDocs:
    """Parse Mekugi's `description.rs` into structured code-mode prompt templates."""
    exec_tpl = extract_rust_raw_str_const(source, "EXEC_DESCRIPTION_TEMPLATE")
    wait_tpl = extract_rust_raw_str_const(source, "WAIT_DESCRIPTION_TEMPLATE")
    try:
        image_helper = extract_rust_raw_str_const(source, "UNIFIED_IMAGE_HELPER_DESCRIPTION")
    except ValueError:
        image_helper = extract_rust_raw_str_const(source, "LEGACY_IMAGE_HELPER_DESCRIPTION")
    return ExtractedCodeModeDocs(
        source_repo=source_repo,
        source_ref=source_ref,
        exec_description_template=exec_tpl,
        wait_description_template=wait_tpl,
        image_helper_description=image_helper,
    )


def render_tool_descriptions(
    docs: ExtractedCodeModeDocs,
    *,
    default_exec_yield_time_ms: int = 10_000,
) -> tuple[str, str]:
    """Render Mekugi's extracted templates into MCP `exec` and `wait` tool descriptions."""
    exec_body = (
        docs.exec_description_template.replace(
            "{{ default_exec_yield_time_ms }}", str(default_exec_yield_time_ms)
        )
        .replace("{{ image_helper }}", docs.image_helper_description)
        .strip()
    )
    exec_description = f"{exec_body}\n\n{NESTED_TOOLS_SECTION}\n\n{GEMINI_YIELD_CAP_NOTE}"
    wait_description = f"{docs.wait_description_template.strip()}\n{WAIT_MCP_ADDENDUM}"
    return exec_description, wait_description


def load_extracted_docs_file(path: str | Path) -> ExtractedCodeModeDocs | None:
    p = Path(path)
    if not p.is_file():
        return None
    data = json.loads(p.read_text(encoding="utf-8"))
    return ExtractedCodeModeDocs.from_dict(data)


def resolve_mcp_tool_descriptions(
    *,
    docs_path: str | None = None,
    devbox_url: str | None = None,
    devbox_headers: dict[str, str] | None = None,
    default_exec_yield_time_ms: int = 10_000,
) -> tuple[str, str, str | None]:
    """Resolve `(exec_description, wait_description, source_ref)` for the MCP server.

    Lookup order:
    1. Local extracted JSON file (`docs_path` or `CODE_MODE_DOCS_PATH`).
    2. Running devbox `GET /code-mode-docs` (which holds the build-time extracted docs
       matched to the exact `SOURCE_REVISION` of the downloaded `codex-code-mode-host`).
    3. Built-in fallback descriptions in `tool_catalog.py`.
    """
    resolved_path = docs_path or os.environ.get("CODE_MODE_DOCS_PATH", "").strip()
    if resolved_path:
        try:
            docs = load_extracted_docs_file(resolved_path)
            if docs is not None:
                exec_desc, wait_desc = render_tool_descriptions(
                    docs, default_exec_yield_time_ms=default_exec_yield_time_ms
                )
                return exec_desc, wait_desc, docs.source_ref
        except Exception as exc:
            logger.warning("Failed to load code-mode docs from %s: %s", resolved_path, exc)

    if devbox_url:
        try:
            resp = httpx.get(
                f"{devbox_url.rstrip('/')}/code-mode-docs",
                headers=devbox_headers or {},
                timeout=3.0,
            )
            if resp.status_code == 200:
                docs = ExtractedCodeModeDocs.from_dict(resp.json())
                exec_desc, wait_desc = render_tool_descriptions(
                    docs, default_exec_yield_time_ms=default_exec_yield_time_ms
                )
                return exec_desc, wait_desc, docs.source_ref
        except Exception as exc:
            logger.debug("Could not fetch code-mode docs from devbox %s: %s", devbox_url, exc)

    return EXEC_TOOL_DESCRIPTION, WAIT_TOOL_DESCRIPTION, None


def fetch_description_rs(*, repo: str, ref: str, timeout_s: float = 30.0) -> str:
    url = f"https://raw.githubusercontent.com/{repo}/{ref}/{DESCRIPTION_RS_PATH}"
    resp = httpx.get(url, follow_redirects=True, timeout=timeout_s)
    resp.raise_for_status()
    return resp.text


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Extract Mekugi code-mode tool descriptions from codex description.rs"
    )
    parser.add_argument("--repo", default=DEFAULT_CODEX_REPO, help="GitHub owner/repo for codex fork")
    ref_group = parser.add_mutually_exclusive_group(required=True)
    ref_group.add_argument("--ref", help="Git ref/SHA matching the downloaded Mekugi binary")
    ref_group.add_argument("--ref-file", help="Path to SOURCE_REVISION file from Mekugi tarball")
    ref_group.add_argument("--source-file", help="Local path to description.rs (offline mode)")
    parser.add_argument("--output", required=True, help="Output JSON file path")
    args = parser.parse_args()

    if args.source_file:
        source_text = Path(args.source_file).read_text(encoding="utf-8")
        source_ref = "local"
    else:
        source_ref = (
            Path(args.ref_file).read_text(encoding="utf-8").strip()
            if args.ref_file
            else str(args.ref).strip()
        )
        source_text = fetch_description_rs(repo=args.repo, ref=source_ref)

    docs = parse_description_rs(source_text, source_repo=args.repo, source_ref=source_ref)
    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(docs.to_dict(), indent=2) + "\n", encoding="utf-8")
    print(f"Extracted Mekugi code-mode docs from {args.repo}@{source_ref} -> {out}")


if __name__ == "__main__":
    main()
