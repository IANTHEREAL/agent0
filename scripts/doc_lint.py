#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "pyyaml>=6.0.0",
# ]
# ///

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

import yaml


PROJECT_DIR = Path(__file__).resolve().parent.parent

MODULES_YAML_PATH = PROJECT_DIR / "docs" / "sot" / "modules.yaml"
SOT_README_PATH = PROJECT_DIR / "docs" / "sot" / "README.md"
SOT_DIR = PROJECT_DIR / "docs" / "sot"
OPS_CONFIG_PATH = SOT_DIR / "ops-config.md"

REQUIRED_MODULE_FIELDS: tuple[str, ...] = (
    "module_id",
    "doc_path",
    "scope",
    "non_goals",
    "code_entrypoints",
    "gate_tests",
    "owners",
    "status",
    "coverage_gaps",
    "next_step",
)

ALLOWED_STATUS: set[str] = {"FULL", "PARTIAL", "TBD"}

REQUIRED_DOC_HEADINGS: tuple[str, ...] = (
    "Scope",
    "Non-goals",
    "Entrypoints",
    "Verification (Gates)",
    "Change Management",
)

MARKER_PARITY = "PG_PARITY"
MARKER_DIVERGENCE_RE = re.compile(r"DB9_DIVERGENCE\(([^)]+)\)")
MARKER_REF_RE = re.compile(r"^(#\d+|ADR-[A-Za-z0-9._-]+|DR-[A-Za-z0-9._-]+)$")


@dataclass(frozen=True)
class LintError:
    message: str


class DocLinter:
    def __init__(self) -> None:
        self._errors: list[LintError] = []

    @property
    def errors(self) -> list[LintError]:
        return self._errors

    def error(self, message: str) -> None:
        self._errors.append(LintError(message=message))


def _read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _is_nonempty_str(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _parse_yaml_file(linter: DocLinter, path: Path) -> Any | None:
    try:
        raw = _read_text(path)
    except FileNotFoundError:
        linter.error(f"missing required file: {path}")
        return None

    try:
        return yaml.safe_load(raw)
    except yaml.YAMLError as e:
        linter.error(f"{path}: YAML parse failed: {e}")
        return None


def _repo_path_exists(path_str: str) -> bool:
    path = Path(path_str)
    if path.is_absolute():
        return path.exists()
    return (PROJECT_DIR / path).exists()


_RUST_SYMBOL_KEYWORDS: tuple[str, ...] = ("fn", "struct", "enum", "trait", "const", "type", "mod", "macro_rules!")
_PYTHON_SYMBOL_KEYWORDS: tuple[str, ...] = ("def", "class")


def _build_symbol_pattern(path_str: str) -> re.Pattern[str] | None:
    if path_str.endswith(".rs"):
        keywords = _RUST_SYMBOL_KEYWORDS
    elif path_str.endswith(".py"):
        keywords = _PYTHON_SYMBOL_KEYWORDS
    else:
        return None
    alternatives = "|".join(re.escape(kw) for kw in keywords)
    return re.compile(rf"\b(?:{alternatives})\s+(\w+)")


def _file_defines_symbol(source_text: str, symbol: str, pattern: re.Pattern[str]) -> bool:
    for match in pattern.finditer(source_text):
        if match.group(1) == symbol:
            return True
    return False


def _check_entrypoint_symbols(
    linter: DocLinter,
    ep_path: str,
    symbols: list[Any],
    module_ref: str,
    ep_idx: int,
    source_cache: dict[str, str | None],
) -> None:
    pattern = _build_symbol_pattern(ep_path)
    if pattern is None:
        return

    if ep_path not in source_cache:
        full_path = Path(ep_path)
        if not full_path.is_absolute():
            full_path = PROJECT_DIR / full_path
        try:
            source_cache[ep_path] = _read_text(full_path)
        except (FileNotFoundError, OSError):
            source_cache[ep_path] = None

    source_text = source_cache[ep_path]
    if source_text is None:
        return

    for sym in symbols:
        if not _is_nonempty_str(sym):
            linter.error(
                f"{MODULES_YAML_PATH}: {module_ref}: code_entrypoints[{ep_idx}].symbols contains non-string entry: {sym!r}"
            )
            continue
        if not _file_defines_symbol(source_text, sym, pattern):
            linter.error(
                f"{MODULES_YAML_PATH}: {module_ref}: symbol `{sym}` not found in {ep_path}"
            )


def _validate_module_required_fields(linter: DocLinter, module: dict[str, Any], module_ref: str) -> None:
    for field_name in REQUIRED_MODULE_FIELDS:
        if field_name not in module:
            linter.error(f"{MODULES_YAML_PATH}: {module_ref}: missing required field `{field_name}`")

    module_id = module.get("module_id")
    if module_id is not None and not _is_nonempty_str(module_id):
        linter.error(f"{MODULES_YAML_PATH}: {module_ref}: `module_id` must be a non-empty string")

    status = module.get("status")
    if status is not None:
        if not _is_nonempty_str(status):
            linter.error(f"{MODULES_YAML_PATH}: {module_ref}: `status` must be a non-empty string")
        elif status not in ALLOWED_STATUS:
            linter.error(
                f"{MODULES_YAML_PATH}: {module_ref}: invalid `status`={status!r} (allowed: {sorted(ALLOWED_STATUS)})"
            )

    for list_field in ("scope", "non_goals", "code_entrypoints", "gate_tests", "owners", "coverage_gaps", "next_step"):
        if list_field in module and not isinstance(module[list_field], list):
            linter.error(f"{MODULES_YAML_PATH}: {module_ref}: `{list_field}` must be a list")

    if "doc_path" in module and not _is_nonempty_str(module["doc_path"]):
        linter.error(f"{MODULES_YAML_PATH}: {module_ref}: `doc_path` must be a non-empty string")


def _extract_sot_map_module_ids(readme_text: str) -> list[str] | None:
    lines = readme_text.splitlines()
    header_idx: int | None = None
    for idx, line in enumerate(lines):
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().split("|")[1:-1]]
        if len(cells) < 2:
            continue
        if cells[0] == "Module" and cells[1] == "SoT doc":
            header_idx = idx
            break

    if header_idx is None:
        return None

    module_ids: list[str] = []
    for line in lines[header_idx + 2 :]:
        if not line.startswith("|"):
            break

        cells = [c.strip() for c in line.strip().split("|")[1:-1]]
        if not cells:
            continue

        module_cell = cells[0]
        if module_cell.startswith("`") and module_cell.endswith("`"):
            module_cell = module_cell[1:-1].strip()
        if module_cell:
            module_ids.append(module_cell)

    return module_ids


def _find_heading_line_indexes(text: str) -> dict[str, int]:
    heading_indexes: dict[str, int] = {}
    for idx, line in enumerate(text.splitlines(), start=1):
        if not line.startswith("## "):
            continue
        heading = line.removeprefix("## ").strip()
        if heading:
            heading_indexes.setdefault(heading, idx)
    return heading_indexes


def _section_body_lines(text: str, heading: str) -> list[str] | None:
    lines = text.splitlines()
    heading_line = f"## {heading}"

    start_idx: int | None = None
    for idx, line in enumerate(lines):
        if line.strip() == heading_line:
            start_idx = idx + 1
            break

    if start_idx is None:
        return None

    body: list[str] = []
    for line in lines[start_idx:]:
        if line.startswith("## "):
            break
        body.append(line)
    return body


def _iter_sot_markdown_files() -> Iterable[Path]:
    for path in sorted(SOT_DIR.glob("*.md")):
        if path.name.startswith("."):
            continue
        yield path


def _check_sot_doc(linter: DocLinter, doc_path: Path, module_ref: str) -> None:
    full_path = doc_path if doc_path.is_absolute() else (PROJECT_DIR / doc_path)
    if not full_path.exists():
        return

    doc_text = _read_text(full_path)
    headings = _find_heading_line_indexes(doc_text)

    for heading in REQUIRED_DOC_HEADINGS:
        if heading not in headings:
            linter.error(f"{doc_path}: {module_ref}: missing required heading `## {heading}`")

    entrypoints_body = _section_body_lines(doc_text, "Entrypoints")
    if entrypoints_body is not None:
        has_content = any(line.strip() for line in entrypoints_body)
        if not has_content:
            linter.error(f"{doc_path}: {module_ref}: `## Entrypoints` section is empty")


def _check_sot_map(linter: DocLinter, readme_path: Path, module_ids: list[str]) -> None:
    try:
        readme_text = _read_text(readme_path)
    except FileNotFoundError:
        linter.error(f"missing required file: {readme_path}")
        return

    table_module_ids = _extract_sot_map_module_ids(readme_text)
    if table_module_ids is None:
        linter.error(f"{readme_path}: failed to locate SoT map table (header row starting with `| Module | ... |`)")
        return

    yaml_ids = [m for m in module_ids if m]
    yaml_id_set = set(yaml_ids)
    table_id_set = set(table_module_ids)

    missing = sorted(yaml_id_set - table_id_set)
    extra = sorted(table_id_set - yaml_id_set)

    for module_id in missing:
        linter.error(f"{readme_path}: SoT map table missing module_id: {module_id!r}")
    for module_id in extra:
        linter.error(f"{readme_path}: SoT map table contains unknown module_id (not in modules.yaml): {module_id!r}")

    if len(table_module_ids) != len(table_id_set):
        linter.error(f"{readme_path}: SoT map table has duplicate module rows (module_id appears more than once)")

    if len(yaml_id_set) != len(table_id_set):
        linter.error(
            f"{readme_path}: SoT map table module count mismatch (modules.yaml={len(yaml_id_set)}, table={len(table_id_set)})"
        )


def _check_config_ssot(linter: DocLinter) -> None:
    for md_path in _iter_sot_markdown_files():
        if md_path.resolve() == OPS_CONFIG_PATH.resolve():
            continue
        for line_no, line in enumerate(_read_text(md_path).splitlines(), start=1):
            if line.lstrip().startswith("| Key |"):
                linter.error(
                    f"{md_path.relative_to(PROJECT_DIR)}:{line_no}: config table header `| Key |` is only allowed in {OPS_CONFIG_PATH.relative_to(PROJECT_DIR)}"
                )


def _git_changed_files() -> list[str]:
    """
    Return changed paths for current branch compared to origin/master.

    Falls back to HEAD~1 diff if origin/master is unavailable.
    """
    candidates = [
        ["git", "diff", "--name-only", "--diff-filter=ACMRTUXB", "origin/master...HEAD"],
        ["git", "diff", "--name-only", "--diff-filter=ACMRTUXB", "HEAD~1...HEAD"],
    ]
    for cmd in candidates:
        try:
            out = subprocess.check_output(cmd, cwd=PROJECT_DIR, text=True, stderr=subprocess.DEVNULL)
        except Exception:
            continue
        files = [line.strip() for line in out.splitlines() if line.strip()]
        if files:
            return files
    return []


def _is_sql_contract_test(sql_path: Path) -> bool:
    return any(
        sql_path.with_suffix(suffix).exists()
        for suffix in (".expected", ".errors", ".assert")
    )


def _check_changed_sql_contract_markers(linter: DocLinter) -> None:
    changed = _git_changed_files()
    for rel in changed:
        if not rel.startswith("tests/") or not rel.endswith(".sql"):
            continue
        sql_path = PROJECT_DIR / rel
        if not sql_path.exists() or not _is_sql_contract_test(sql_path):
            continue

        first_lines = _read_text(sql_path).splitlines()[:20]
        header = "\n".join(first_lines)

        has_parity = MARKER_PARITY in header
        divergence_match = MARKER_DIVERGENCE_RE.search(header)
        if not has_parity and divergence_match is None:
            linter.error(
                f"{rel}: SQL contract test must include top-of-file marker "
                f"`{MARKER_PARITY}` or `DB9_DIVERGENCE(<tracking-id>)`"
            )
            continue

        if divergence_match is not None:
            ref = divergence_match.group(1).strip()
            if not MARKER_REF_RE.match(ref):
                linter.error(
                    f"{rel}: invalid DB9_DIVERGENCE tracking id `{ref}`; "
                    "expected `#<issue>`, `ADR-...`, or `DR-...`"
                )


def _report(linter: DocLinter) -> int:
    if not linter.errors:
        print("doc-lint: OK")
        return 0

    print(f"doc-lint: FAILED ({len(linter.errors)} errors)")
    for idx, err in enumerate(linter.errors, start=1):
        print(f"{idx:02d}. {err.message}")
    return 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Fast doc lint for docs/sot/** (drift prevention gate).")
    parser.add_argument("--modules", type=Path, default=MODULES_YAML_PATH, help="Path to docs/sot/modules.yaml")
    parser.add_argument("--readme", type=Path, default=SOT_README_PATH, help="Path to docs/sot/README.md")
    args = parser.parse_args(argv)

    linter = DocLinter()

    registry = _parse_yaml_file(linter, args.modules)
    if registry is None:
        return _report(linter)

    if not isinstance(registry, dict):
        linter.error(f"{args.modules}: YAML root must be a mapping (dict)")
        return _report(linter)

    modules = registry.get("modules")
    if not isinstance(modules, list):
        linter.error(f"{args.modules}: missing or invalid `modules` list")
        return _report(linter)

    module_ids: list[str] = []
    seen_module_ids: set[str] = set()
    source_cache: dict[str, str | None] = {}

    for idx, module in enumerate(modules):
        module_ref = f"modules[{idx}]"
        if not isinstance(module, dict):
            linter.error(f"{args.modules}: {module_ref}: module entry must be a mapping (dict)")
            continue

        module_id = module.get("module_id")
        if _is_nonempty_str(module_id):
            module_ref = f"module_id={module_id}"
            module_ids.append(module_id)
            if module_id in seen_module_ids:
                linter.error(f"{args.modules}: duplicate `module_id`: {module_id!r}")
            seen_module_ids.add(module_id)

        _validate_module_required_fields(linter, module, module_ref)

        doc_path = module.get("doc_path")
        if _is_nonempty_str(doc_path) and not _repo_path_exists(doc_path):
            linter.error(f"{args.modules}: {module_ref}: `doc_path` does not exist: {doc_path}")

        entrypoints = module.get("code_entrypoints")
        if isinstance(entrypoints, list):
            for ep_idx, entry in enumerate(entrypoints):
                if not isinstance(entry, dict):
                    linter.error(f"{args.modules}: {module_ref}: code_entrypoints[{ep_idx}] must be a mapping (dict)")
                    continue
                ep_path = entry.get("path")
                if not _is_nonempty_str(ep_path):
                    linter.error(f"{args.modules}: {module_ref}: code_entrypoints[{ep_idx}].path must be a non-empty string")
                    continue
                if not _repo_path_exists(ep_path):
                    linter.error(f"{args.modules}: {module_ref}: code_entrypoints[{ep_idx}].path does not exist: {ep_path}")
                    continue
                symbols = entry.get("symbols")
                if isinstance(symbols, list) and symbols:
                    _check_entrypoint_symbols(linter, ep_path, symbols, module_ref, ep_idx, source_cache)

        if _is_nonempty_str(doc_path):
            _check_sot_doc(linter, Path(doc_path), module_ref)

    _check_sot_map(linter, args.readme, module_ids)
    _check_config_ssot(linter)
    _check_changed_sql_contract_markers(linter)

    return _report(linter)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
