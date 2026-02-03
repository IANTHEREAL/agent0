#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""
pg-tikv Integration Test Runner

Usage:
    integration_test.py --dsn postgres://user:pass@host:port/db
    integration_test.py --dsn postgres://... tests/basic.sql
    integration_test.py --dsn postgres://... tests/

Environment:
    PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres
"""

import subprocess
import sys
import os
import time
import argparse
import re
from pathlib import Path
from dataclasses import dataclass, field
from typing import Optional, List, Tuple
from enum import Enum
from urllib.parse import urlparse

PROJECT_DIR = Path(__file__).parent.parent

GREEN = "\033[0;32m"
YELLOW = "\033[1;33m"
RED = "\033[0;31m"
NC = "\033[0m"


class TestResult(Enum):
    PASSED = "PASSED"
    FAILED = "FAILED"
    SKIPPED = "SKIPPED"
    ERROR = "ERROR"

class PsqlOutputMode(Enum):
    UNALIGNED = "unaligned"
    ALIGNED = "aligned"


@dataclass
class DbConfig:
    host: str = "127.0.0.1"
    port: int = 5433
    user: str = "admin"
    password: str = "admin"
    database: str = "postgres"

    @classmethod
    def from_dsn(cls, dsn: str) -> "DbConfig":
        parsed = urlparse(dsn)
        return cls(
            host=parsed.hostname or "127.0.0.1",
            port=parsed.port or 5433,
            user=parsed.username or "admin",
            password=parsed.password or "admin",
            database=parsed.path.lstrip("/") or "postgres",
        )

    def to_dsn(self) -> str:
        return f"postgres://{self.user}:{self.password}@{self.host}:{self.port}/{self.database}"


@dataclass
class TestConfig:
    db: DbConfig = field(default_factory=DbConfig)
    verbose: bool = False
    stop_on_error: bool = False
    test_files: List[Path] = field(default_factory=list)


@dataclass
class TestStats:
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    errors: int = 0

    @property
    def total(self) -> int:
        return self.passed + self.failed + self.skipped + self.errors

    def add(self, result: TestResult):
        if result == TestResult.PASSED:
            self.passed += 1
        elif result == TestResult.FAILED:
            self.failed += 1
        elif result == TestResult.SKIPPED:
            self.skipped += 1
        else:
            self.errors += 1


config = TestConfig()


def log_info(msg: str):
    print(f"{GREEN}[INFO]{NC} {msg}")


def log_warn(msg: str):
    print(f"{YELLOW}[WARN]{NC} {msg}")


def log_error(msg: str):
    print(f"{RED}[ERROR]{NC} {msg}")


def log_test(name: str, result: TestResult, details: str = ""):
    color = {
        TestResult.PASSED: GREEN,
        TestResult.FAILED: RED,
        TestResult.SKIPPED: YELLOW,
        TestResult.ERROR: RED,
    }[result]
    suffix = f" - {details}" if details else ""
    print(f"{color}[{result.value}]{NC} {name}{suffix}")


def psql_args() -> List[str]:
    return psql_args_for_mode(PsqlOutputMode.UNALIGNED)


def psql_args_for_mode(mode: PsqlOutputMode, *, database: Optional[str] = None) -> List[str]:
    db = config.db
    dsn_db = database or db.database

    base = [
        "psql",
        "-X",
        "-q",
        "-P",
        "pager=off",
        "-h",
        db.host,
        "-p",
        str(db.port),
        "-U",
        db.user,
        "-d",
        dsn_db,
    ]

    if mode == PsqlOutputMode.UNALIGNED:
        return base + [
            "-P",
            "format=unaligned",
            "-P",
            "fieldsep=|",
            "-P",
            "null=NULL",
        ]

    # Default `psql` output is aligned; keep defaults to match `.expected` files that
    # were generated with standard `psql` formatting.
    return base


def psql_env(*, client_min_messages: str = "warning") -> dict:
    env = os.environ.copy()
    env["PGPASSWORD"] = config.db.password
    env["PGOPTIONS"] = env.get("PGOPTIONS", "") + f" -c client_min_messages={client_min_messages}"
    return env


def normalize_decimal(text: str) -> str:
    from decimal import Decimal, ROUND_HALF_UP, localcontext

    max_scale = 16

    def repl(match: re.Match) -> str:
        num = match.group(0)
        if "." not in num:
            return num

        frac_len = len(num.split(".", 1)[1])

        with localcontext() as ctx:
            ctx.prec = max(64, len(num))
            try:
                dec = Decimal(num)
            except Exception:
                return num
            if frac_len > max_scale:
                quant = Decimal(1).scaleb(-max_scale)  # 1e-16
                dec = dec.quantize(quant, rounding=ROUND_HALF_UP)

        out = format(dec, "f")
        if "." in out:
            out = out.rstrip("0").rstrip(".")
        return out

    return re.sub(r"-?\d+\.\d+", repl, text)


def normalize_timestamp(text: str) -> str:
    return re.sub(
        r"(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2}:\d{2})(\.\d+)?(?:\+00:00|Z)?",
        r"\1 \2\3",
        text,
    )


def normalize_json_whitespace(text: str) -> str:
    if "{" not in text and "[" not in text:
        return text
    text = re.sub(r"\s+([}\]])", r"\1", text)
    text = re.sub(r"([{\[])\s+", r"\1", text)
    text = re.sub(r":\s+", ":", text)
    text = re.sub(r",\s+", ",", text)
    return text


def normalize_pg_array_literal(text: str) -> str:
    stripped = text.strip()
    if len(stripped) < 2 or not (stripped.startswith("{") and stripped.endswith("}")):
        return text
    # Avoid rewriting JSON objects like {"a":1}.
    if ":" in stripped:
        return text

    i = 0

    def parse_quoted() -> Optional[str]:
        nonlocal i
        if i >= len(stripped) or stripped[i] != '"':
            return None
        i += 1
        out = []
        while i < len(stripped):
            ch = stripped[i]
            if ch == '"':
                i += 1
                return "".join(out)
            if ch == "\\" and i + 1 < len(stripped):
                out.append(stripped[i + 1])
                i += 2
                continue
            out.append(ch)
            i += 1
        return None

    def parse_unquoted() -> str:
        nonlocal i
        out = []
        while i < len(stripped) and stripped[i] not in ",}":
            if stripped[i] == "\\" and i + 1 < len(stripped):
                out.append(stripped[i + 1])
                i += 2
                continue
            out.append(stripped[i])
            i += 1
        return "".join(out)

    def parse_array() -> Optional[list]:
        nonlocal i
        if i >= len(stripped) or stripped[i] != "{":
            return None
        i += 1
        items: list = []
        if i < len(stripped) and stripped[i] == "}":
            i += 1
            return items
        while i < len(stripped):
            if stripped[i] == "{":
                child = parse_array()
                if child is None:
                    return None
                items.append(child)
            elif stripped[i] == '"':
                val = parse_quoted()
                if val is None:
                    return None
                items.append(val)
            else:
                val = parse_unquoted()
                if val.upper() == "NULL":
                    items.append(None)
                else:
                    items.append(val)
            if i >= len(stripped):
                return None
            if stripped[i] == ",":
                i += 1
                continue
            if stripped[i] == "}":
                i += 1
                return items
            return None
        return None

    def needs_quotes(val: str) -> bool:
        if val == "" or val.upper() == "NULL":
            return True
        return any(
            c.isspace() or c in ('{', '}', ',', '"', "\\")
            for c in val
        )

    def escape_elem(val: str) -> str:
        return val.replace("\\", "\\\\").replace('"', '\\"')

    def serialize_array(items: list) -> str:
        parts = []
        for item in items:
            if item is None:
                parts.append("NULL")
            elif isinstance(item, list):
                parts.append(serialize_array(item))
            else:
                if needs_quotes(item):
                    parts.append(f"\"{escape_elem(item)}\"")
                else:
                    parts.append(item)
        return "{" + ",".join(parts) + "}"

    parsed = parse_array()
    if parsed is None or i != len(stripped):
        return text

    normalized = serialize_array(parsed)
    # Preserve original leading/trailing whitespace.
    return text.replace(stripped, normalized, 1)


def normalize_psql_aligned_line(line: str) -> str:
    stripped = line.strip()
    if not stripped:
        return ""

    # Psql table separator lines depend on column widths (which can vary when we
    # normalize numeric literals). Canonicalize by collapsing '-' runs while
    # keeping '+' column separators.
    if all(ch in "-+" for ch in stripped) and "-" in stripped:
        return re.sub(r"-+", "-", stripped)

    # Canonicalize aligned tables (`col | col`) by trimming cell padding so
    # column width differences don't affect comparisons.
    if "|" in stripped:
        parts = [part.strip() for part in stripped.split("|")]
        return "|".join(parts)

    return stripped


_PSQL_FILE_LINE_PREFIX = re.compile(r"^psql:(?P<file>.+?):(?P<line>\d+):(?P<rest>.*)$")
_ABS_TEST_SQL_PATH = re.compile(
    r"(?P<path>(?:[A-Za-z]:)?(?:[/\\][^\s:\"']+)*[/\\](?:tests|tests_pending)[/\\][^\s:\"']+\.sql)"
)


def _canonicalize_test_sql_path(path: str) -> str:
    path = path.replace("\\", "/")
    for marker in ("/tests/", "/tests_pending/"):
        if marker in path:
            base = marker.strip("/")
            return f"{base}/{path.split(marker, 1)[1]}"
    return path


def _normalize_test_sql_paths(line: str) -> str:
    def repl(match: re.Match) -> str:
        return _canonicalize_test_sql_path(match.group("path"))

    return _ABS_TEST_SQL_PATH.sub(repl, line)


def normalize_output(
    text: str,
    *,
    strip_psql_prefix: bool,
    mode: PsqlOutputMode,
) -> List[str]:
    lines = []
    for raw in text.splitlines():
        line = raw.rstrip()
        if line.startswith("psql:"):
            match = _PSQL_FILE_LINE_PREFIX.match(line)
            if match:
                if strip_psql_prefix:
                    line = match.group("rest").lstrip()
                else:
                    canon_file = _canonicalize_test_sql_path(match.group("file"))
                    line_no = match.group("line")
                    rest = match.group("rest")
                    line = f"psql:{canon_file}:{line_no}:{rest}"
            elif strip_psql_prefix:
                line = line[len("psql:") :].lstrip()
        line = _normalize_test_sql_paths(line)
        line = normalize_timestamp(line)
        line = normalize_decimal(line)
        line = normalize_json_whitespace(line)
        line = normalize_pg_array_literal(line)
        if mode == PsqlOutputMode.ALIGNED:
            line = normalize_psql_aligned_line(line)
        if line.strip() == "testdb":
            line = line.replace("testdb", "postgres")
        lines.append(line)
    return lines


def expected_has_trailing_error_block(lines: List[str]) -> bool:
    saw_error = False
    for line in lines:
        if not line.strip():
            continue
        if line.startswith(("ERROR:", "FATAL:", "DETAIL:", "HINT:", "CONTEXT:")):
            saw_error = True
            continue
        if saw_error:
            return False
    return saw_error


def move_error_block_to_end(lines: List[str]) -> List[str]:
    head = []
    tail = []
    for line in lines:
        if line.startswith(("ERROR:", "FATAL:", "DETAIL:", "HINT:", "CONTEXT:")):
            tail.append(line)
        else:
            head.append(line)
    return head + tail

_PSQL_DIAGNOSTIC_LINE = re.compile(
    r"^(?:ERROR|FATAL|PANIC|WARNING|NOTICE|DETAIL|HINT|CONTEXT):|^LINE\s+\d+:|^\s*\^",
    re.IGNORECASE,
)


def stable_partition_psql_diagnostics(lines: List[str]) -> List[str]:
    """Move psql diagnostics (ERROR/NOTICE/etc) to the end, preserving relative order.

    stdout/stderr buffering and capture strategies can reorder psql diagnostics relative to
    query results. Canonicalize by stable-partitioning diagnostic lines to the end for both
    expected and actual outputs before diffing.
    """

    non_diagnostics: List[str] = []
    diagnostics: List[str] = []
    for line in lines:
        if _PSQL_DIAGNOSTIC_LINE.match(line):
            diagnostics.append(line)
        else:
            non_diagnostics.append(line)
    return non_diagnostics + diagnostics


def check_connection() -> bool:
    result = subprocess.run(
        psql_args_for_mode(PsqlOutputMode.UNALIGNED) + ["-c", "SELECT 1"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env=psql_env(client_min_messages="warning"),
        timeout=10,
    )
    return result.returncode == 0


def run_sql(
    sql: str,
    retries: int = 2,
    *,
    mode: PsqlOutputMode = PsqlOutputMode.UNALIGNED,
    client_min_messages: str = "warning",
    database: Optional[str] = None,
) -> Tuple[str, int]:
    output = ""
    for attempt in range(retries + 1):
        result = subprocess.run(
            psql_args_for_mode(mode, database=database) + ["-c", sql],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=psql_env(client_min_messages=client_min_messages),
        )
        output = result.stdout or ""

        if "Failed to connect to TiKV" not in output and "connection refused" not in output.lower():
            return output, result.returncode

        if attempt < retries:
            log_warn(f"Connection failed, retrying ({attempt + 1}/{retries})...")
            time.sleep(2)

    return output, 1


def run_sql_file(
    sql_file: Path,
    *,
    mode: PsqlOutputMode = PsqlOutputMode.UNALIGNED,
    client_min_messages: str = "warning",
    database: Optional[str] = None,
) -> Tuple[str, int]:
    result = subprocess.run(
        psql_args_for_mode(mode, database=database) + ["-f", str(sql_file)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env=psql_env(client_min_messages=client_min_messages),
    )
    return result.stdout or "", result.returncode


def run_sql_test_file(sql_file: Path, stats: TestStats) -> TestResult:
    expected_file = sql_file.with_suffix(".expected")
    errors_file = sql_file.with_suffix(".errors")
    assert_file = sql_file.with_suffix(".assert")
    out_file = sql_file.with_suffix(".out")
    setup_file = sql_file.with_name(sql_file.stem + "_setup.sql")
    load_script = sql_file.with_name(sql_file.stem + "_load.py")

    log_info(f"Running: {sql_file.name}")

    expected_text = expected_file.read_text() if expected_file.exists() else ""
    expected_lines = expected_text.splitlines()
    expected_has_psql = any(line.startswith("psql:") for line in expected_lines)
    expected_has_bare_diagnostics = any(
        line.startswith(("ERROR:", "FATAL:", "WARNING:", "NOTICE:")) for line in expected_lines
    )

    # Determine which `psql` formatting was used to generate the `.expected` file.
    # - Historical tests use `format=unaligned` with `fieldsep=|` and `null=NULL`.
    # - Newer tests use default aligned output (tables with borders, including 1-column tables).
    expects_aligned = False
    if any(" | " in line for line in expected_lines):
        expects_aligned = True
    elif any(re.match(r"^-{3,}\\+-", line) for line in expected_lines):
        expects_aligned = True
    elif any(re.match(r"^-{3,}$", line) for line in expected_lines):
        expects_aligned = True
    elif any("|" in line for line in expected_lines):
        expects_aligned = False

    mode = PsqlOutputMode.ALIGNED if expects_aligned else PsqlOutputMode.UNALIGNED
    expected_wants_notice = any("NOTICE:" in line for line in expected_lines)
    client_min_messages = (
        "notice"
        if mode == PsqlOutputMode.ALIGNED or expected_wants_notice
        else "warning"
    )

    if setup_file.exists():
        log_info(f"  Running setup: {setup_file.name}")
        setup_output, _ = run_sql_file(
            setup_file,
            mode=PsqlOutputMode.UNALIGNED,
            client_min_messages="warning",
        )
        if "ERROR:" in setup_output or "FATAL:" in setup_output:
            log_test(sql_file.name, TestResult.FAILED, "setup failed")
            print(f"  {RED}Setup error: {setup_output[:200]}{NC}")
            return TestResult.FAILED

    if load_script.exists():
        log_info(f"  Running load script: {load_script.name}")
        db = config.db
        result = subprocess.run(
            ["python3", str(load_script), "--port", str(db.port), "--user", db.user, "--password", db.password],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            log_test(sql_file.name, TestResult.FAILED, "load script failed")
            print(f"  {RED}Load error: {result.stdout[:200]}{result.stderr[:200]}{NC}")
            return TestResult.FAILED

    output, _ = run_sql_file(sql_file, mode=mode, client_min_messages=client_min_messages)
    out_file.write_text(output)

    if config.verbose:
        print(output)

    error_patterns = ["ERROR:", "FATAL:", "error:", "fatal:"]
    has_error = any(pattern in output for pattern in error_patterns)

    if expected_file.exists():
        expected = expected_text

        unordered = False
        for line in expected_lines:
            if not line.strip():
                continue
            if line.strip().lower() == "# unordered":
                unordered = True
            break

        if unordered:
            expected_lines = [line for line in expected_lines if line.strip().lower() != "# unordered"]
            expected = "\n".join(expected_lines)

        normalized_output = normalize_output(output, strip_psql_prefix=True, mode=mode)
        normalized_expected = normalize_output(expected, strip_psql_prefix=True, mode=mode)
        normalized_output = stable_partition_psql_diagnostics(normalized_output)
        normalized_expected = stable_partition_psql_diagnostics(normalized_expected)

        if unordered:
            normalized_output = [line for line in normalized_output if line.strip()]
            normalized_expected = [line for line in normalized_expected if line.strip()]
            if sorted(normalized_output) == sorted(normalized_expected):
                log_test(sql_file.name, TestResult.PASSED)
                return TestResult.PASSED
        elif normalized_output == normalized_expected:
            log_test(sql_file.name, TestResult.PASSED)
            return TestResult.PASSED
        else:
            if expected_has_bare_diagnostics and expected_has_trailing_error_block(normalized_expected):
                reordered_output = move_error_block_to_end(normalized_output)
                if reordered_output == normalized_expected:
                    log_test(sql_file.name, TestResult.PASSED)
                    return TestResult.PASSED
            log_test(sql_file.name, TestResult.FAILED, "output differs from expected")
            if config.verbose:
                print(f"--- Expected ({expected_file}):")
                print(expected[:500])
                print(f"--- Actual ({out_file}):")
                print(output[:500])
            return TestResult.FAILED

    errors_ok = True
    if has_error:
        errors_ok = False
        if errors_file.exists():
            expected_errors = [
                line.strip()
                for line in errors_file.read_text().strip().split("\n")
                if line.strip()
            ]
            actual_errors = [
                line
                for line in output.split("\n")
                if any(p in line for p in error_patterns)
            ]
            unexpected_errors = [
                actual
                for actual in actual_errors
                if not any(exp in actual for exp in expected_errors)
            ]
            if unexpected_errors:
                log_test(sql_file.name, TestResult.FAILED, "unexpected SQL errors")
                for line in unexpected_errors[:3]:
                    print(f"  {RED}{line}{NC}")
                return TestResult.FAILED
            errors_ok = True
        else:
            log_test(sql_file.name, TestResult.FAILED, "SQL errors detected")
            for line in output.split("\n"):
                if any(p in line for p in error_patterns):
                    print(f"  {RED}{line}{NC}")
                    break
            return TestResult.FAILED

    if assert_file.exists():
        required = [
            line.strip()
            for line in assert_file.read_text().splitlines()
            if line.strip() and not line.strip().startswith("#")
        ]
        missing = [needle for needle in required if needle not in output]
        if missing:
            log_test(sql_file.name, TestResult.FAILED, f"missing expected output: {missing[0]}")
            if config.verbose:
                print(output[:500])
            return TestResult.FAILED

    if errors_ok and (has_error or assert_file.exists()):
        details = []
        if has_error:
            details.append("all errors were expected")
        if assert_file.exists():
            details.append("assertions passed")
        log_test(sql_file.name, TestResult.PASSED, "; ".join(details))
        return TestResult.PASSED

    log_test(sql_file.name, TestResult.PASSED, "no .expected file, checked for errors only")
    return TestResult.PASSED


def run_external_tests(test_paths: List[Path]) -> TestStats:
    stats = TestStats()

    sql_files = []
    for path in test_paths:
        if path.is_file() and path.suffix == ".sql":
            sql_files.append(path)
        elif path.is_dir():
            sql_files.extend(sorted(path.glob("*.sql")))

    if not sql_files:
        log_error("No .sql test files found")
        return stats

    # Skip *_setup.sql only when a matching base test exists.
    filtered_files = []
    for sql_file in sql_files:
        if sql_file.stem.endswith("_setup"):
            base_name = sql_file.stem[: -len("_setup")]
            base_file = sql_file.with_name(base_name + ".sql")
            if base_file.exists():
                continue
        filtered_files.append(sql_file)

    sql_files = filtered_files

    log_info(f"Found {len(sql_files)} test file(s)")
    log_info("=========================================")

    for sql_file in sql_files:
        result = run_sql_test_file(sql_file, stats)
        stats.add(result)

        if config.stop_on_error and result in (TestResult.FAILED, TestResult.ERROR):
            log_warn("Stopping on first error (--stop-on-error)")
            break

        time.sleep(0.1)

    return stats


class TestRunner:
    def __init__(self):
        self.stats = TestStats()

    def run_test(self, name: str, test_func) -> bool:
        try:
            if test_func():
                self.stats.passed += 1
                return True
        except Exception as e:
            log_error(f"{name}: EXCEPTION - {e}")
        self.stats.failed += 1
        return False


def test_basic_connection() -> bool:
    log_info("Testing basic connection...")
    result, _ = run_sql("SELECT 1 as test")
    if "1" in result:
        log_info("Basic connection: PASSED")
        return True
    log_error("Basic connection: FAILED")
    print(result)
    return False


def test_ddl_operations() -> bool:
    log_info("Testing DDL operations...")

    run_sql("DROP TABLE IF EXISTS test_ddl")
    run_sql("""CREATE TABLE test_ddl (
        id SERIAL PRIMARY KEY,
        name TEXT NOT NULL,
        email TEXT,
        created_at TIMESTAMP DEFAULT NOW()
    )""")

    tables, _ = run_sql("SHOW TABLES")
    if "test_ddl" not in tables:
        log_error("CREATE TABLE: FAILED")
        return False
    log_info("CREATE TABLE: PASSED")

    run_sql("ALTER TABLE test_ddl ADD COLUMN age INTEGER")
    run_sql("CREATE INDEX idx_test_name ON test_ddl (name)")
    run_sql("DROP TABLE test_ddl")

    log_info("DDL operations: PASSED")
    return True


def test_alter_table_migration() -> bool:
    log_info("Testing ALTER TABLE migration features...")

    # Cleanup from prior runs
    run_sql("DROP TABLE IF EXISTS atm_posts")
    run_sql("DROP TABLE IF EXISTS atm_users")
    run_sql("DROP TABLE IF EXISTS atm_pk_shift")

    out, code = run_sql(
        """CREATE TABLE atm_users (
            id INT PRIMARY KEY,
            email TEXT,
            age INT,
            nickname TEXT,
            CONSTRAINT atm_users_email_key UNIQUE (email),
            CONSTRAINT atm_users_age_chk CHECK (age > 0)
        )"""
    )
    if code != 0:
        log_error("CREATE TABLE (atm_users): FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, age) VALUES (1, 'a@example.com', 10)"
    )
    if code != 0:
        log_error("INSERT (atm_users): FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        """CREATE TABLE atm_posts (
            id INT PRIMARY KEY,
            user_id INT,
            CONSTRAINT atm_posts_user_fk FOREIGN KEY (user_id) REFERENCES atm_users (id)
        )"""
    )
    if code != 0:
        log_error("CREATE TABLE (atm_posts): FAILED")
        print(out[:200])
        return False

    out, code = run_sql("INSERT INTO atm_posts (id, user_id) VALUES (1, 1)")
    if code != 0:
        log_error("INSERT (atm_posts): FAILED")
        print(out[:200])
        return False

    # Rename FK column and ensure FK still enforced.
    out, code = run_sql("ALTER TABLE atm_posts RENAME COLUMN user_id TO author_id")
    if code != 0:
        log_error("RENAME COLUMN (FK column): FAILED")
        print(out[:200])
        return False

    out, code = run_sql("INSERT INTO atm_posts (id, author_id) VALUES (2, 999)")
    if code == 0 or "violates foreign key constraint" not in out.lower():
        log_error("FK enforcement after RENAME COLUMN: FAILED")
        print(out[:200])
        return False

    # Rename column used by CHECK and ensure CHECK expression is rewritten.
    out, code = run_sql("ALTER TABLE atm_users RENAME COLUMN age TO years")
    if code != 0:
        log_error("RENAME COLUMN (CHECK column): FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (2, 'b@example.com', -1)"
    )
    if code == 0 or "violates check constraint" not in out.lower() or "atm_users_age_chk" not in out:
        log_error("CHECK enforcement after RENAME COLUMN: FAILED")
        print(out[:200])
        return False

    # RENAME CONSTRAINT should update metadata and error messages.
    out, code = run_sql(
        "ALTER TABLE atm_users RENAME CONSTRAINT atm_users_age_chk TO atm_users_years_chk"
    )
    if code != 0:
        log_error("RENAME CONSTRAINT (CHECK): FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (2, 'b@example.com', -1)"
    )
    if code == 0 or "atm_users_years_chk" not in out:
        log_error("CHECK error message after RENAME CONSTRAINT: FAILED")
        print(out[:200])
        return False

    # DROP CONSTRAINT should stop enforcing CHECK.
    out, code = run_sql("ALTER TABLE atm_users DROP CONSTRAINT atm_users_years_chk")
    if code != 0:
        log_error("DROP CONSTRAINT (CHECK): FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (2, 'b@example.com', -1)"
    )
    if code != 0:
        log_error("INSERT after DROP CONSTRAINT (CHECK): FAILED")
        print(out[:200])
        return False

    # UNIQUE constraint should be enforced, then removable via DROP CONSTRAINT.
    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (3, 'b@example.com', 1)"
    )
    if code == 0 or "violates unique constraint" not in out.lower() or "atm_users_email_key" not in out:
        log_error("UNIQUE enforcement: FAILED")
        print(out[:200])
        return False

    out, code = run_sql("ALTER TABLE atm_users DROP CONSTRAINT atm_users_email_key")
    if code != 0:
        log_error("DROP CONSTRAINT (UNIQUE): FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (3, 'b@example.com', 1)"
    )
    if code != 0:
        log_error("INSERT after DROP CONSTRAINT (UNIQUE): FAILED")
        print(out[:200])
        return False

    # DROP CONSTRAINT should stop enforcing FK.
    out, code = run_sql("ALTER TABLE atm_posts DROP CONSTRAINT atm_posts_user_fk")
    if code != 0:
        log_error("DROP CONSTRAINT (FK): FAILED")
        print(out[:200])
        return False

    out, code = run_sql("INSERT INTO atm_posts (id, author_id) VALUES (2, 999)")
    if code != 0:
        log_error("INSERT after DROP CONSTRAINT (FK): FAILED")
        print(out[:200])
        return False

    # ALTER COLUMN DEFAULT should affect missing-column inserts.
    out, code = run_sql("ALTER TABLE atm_users ALTER COLUMN nickname SET DEFAULT 'anon'")
    if code != 0:
        log_error("ALTER COLUMN SET DEFAULT: FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (4, 'd@example.com', 40)"
    )
    if code != 0:
        log_error("INSERT with DEFAULT: FAILED")
        print(out[:200])
        return False

    out, code = run_sql("SELECT nickname FROM atm_users WHERE id = 4")
    if code != 0 or "anon" not in out:
        log_error("DEFAULT value not applied: FAILED")
        print(out[:200])
        return False

    out, code = run_sql("ALTER TABLE atm_users ALTER COLUMN nickname DROP DEFAULT")
    if code != 0:
        log_error("ALTER COLUMN DROP DEFAULT: FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (5, 'e@example.com', 50)"
    )
    if code != 0:
        log_error("INSERT after DROP DEFAULT: FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "SELECT COALESCE(nickname, 'NULL') FROM atm_users WHERE id = 5"
    )
    if code != 0 or "NULL" not in out:
        log_error("DROP DEFAULT not reflected: FAILED")
        print(out[:200])
        return False

    # ALTER COLUMN SET NOT NULL should validate existing rows.
    out, code = run_sql("ALTER TABLE atm_users ALTER COLUMN nickname SET NOT NULL")
    if code == 0:
        log_error("ALTER COLUMN SET NOT NULL should fail on existing NULLs: FAILED")
        return False

    out, code = run_sql("UPDATE atm_users SET nickname = 'x' WHERE nickname IS NULL")
    if code != 0:
        log_error("UPDATE to satisfy NOT NULL: FAILED")
        print(out[:200])
        return False

    out, code = run_sql("ALTER TABLE atm_users ALTER COLUMN nickname SET NOT NULL")
    if code != 0:
        log_error("ALTER COLUMN SET NOT NULL: FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years) VALUES (6, 'f@example.com', 60)"
    )
    if code == 0 or (
        "cannot be null" not in out.lower()
        and "violates not-null constraint" not in out.lower()
    ):
        log_error("NOT NULL enforcement on INSERT: FAILED")
        print(out[:200])
        return False

    # ALTER COLUMN TYPE should rewrite and keep UNIQUE indexes consistent.
    out, code = run_sql("UPDATE atm_users SET years = id * 10")
    if code != 0:
        log_error("UPDATE years for UNIQUE test: FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "ALTER TABLE atm_users ADD CONSTRAINT atm_users_years_key UNIQUE (years)"
    )
    if code != 0:
        log_error("ADD CONSTRAINT (UNIQUE): FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years, nickname) VALUES (6, 'g@example.com', 10, 'x')"
    )
    if code == 0 or "violates unique constraint" not in out.lower():
        log_error("UNIQUE enforcement before TYPE change: FAILED")
        print(out[:200])
        return False

    out, code = run_sql("ALTER TABLE atm_users ALTER COLUMN years TYPE BIGINT")
    if code != 0:
        log_error("ALTER COLUMN TYPE: FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years, nickname) VALUES (6, 'g@example.com', 10, 'x')"
    )
    if code == 0 or "violates unique constraint" not in out.lower():
        log_error("UNIQUE enforcement after TYPE change: FAILED")
        print(out[:200])
        return False

    out, code = run_sql(
        "INSERT INTO atm_users (id, email, years, nickname) VALUES (6, 'g@example.com', 2147483648, 'x')"
    )
    if code != 0:
        log_error("INSERT BIGINT after TYPE change: FAILED")
        print(out[:200])
        return False

    # DROP COLUMN should not corrupt composite PK key encoding.
    out, code = run_sql(
        "CREATE TABLE atm_pk_shift (a INT, b INT, c INT, PRIMARY KEY (b, c))"
    )
    if code != 0:
        log_error("CREATE TABLE (atm_pk_shift): FAILED")
        print(out[:200])
        return False
    run_sql("INSERT INTO atm_pk_shift (a, b, c) VALUES (1, 10, 20)")
    out, code = run_sql("ALTER TABLE atm_pk_shift DROP COLUMN a")
    if code != 0:
        log_error("DROP COLUMN (atm_pk_shift): FAILED")
        print(out[:200])
        return False
    out, code = run_sql("DELETE FROM atm_pk_shift WHERE b = 10 AND c = 20")
    if code != 0:
        log_error("DELETE after DROP COLUMN (atm_pk_shift): FAILED")
        print(out[:200])
        return False
    out, code = run_sql("SELECT COUNT(*) FROM atm_pk_shift")
    if code != 0 or re.search(r"\b0\b", out) is None:
        log_error("Row not deleted after DROP COLUMN (PK shift): FAILED")
        print(out[:200])
        return False

    run_sql("DROP TABLE atm_pk_shift")
    run_sql("DROP TABLE atm_posts")
    run_sql("DROP TABLE atm_users")

    log_info("ALTER TABLE migration features: PASSED")
    return True


def test_dml_operations() -> bool:
    log_info("Testing DML operations...")

    run_sql("DROP TABLE IF EXISTS test_dml")
    run_sql("CREATE TABLE test_dml (id SERIAL PRIMARY KEY, value INTEGER)")
    run_sql("INSERT INTO test_dml (value) VALUES (10), (20), (30)")

    result, _ = run_sql("SELECT SUM(value) FROM test_dml")
    if "60" not in result:
        log_error("INSERT + SELECT: FAILED (expected 60)")
        return False
    log_info("INSERT + SELECT: PASSED")

    run_sql("UPDATE test_dml SET value = value * 2 WHERE value > 15")
    result, _ = run_sql("SELECT SUM(value) FROM test_dml")
    if "110" not in result:
        log_error("UPDATE: FAILED (expected 110)")
        return False
    log_info("UPDATE: PASSED")

    run_sql("DELETE FROM test_dml WHERE value > 50")
    result, _ = run_sql("SELECT COUNT(*) FROM test_dml")
    if "1" not in result:
        log_error("DELETE: FAILED (expected 1 row)")
        return False
    log_info("DELETE: PASSED")

    run_sql("DROP TABLE test_dml")
    log_info("DML operations: PASSED")
    return True


def test_transactions() -> bool:
    log_info("Testing transactions...")

    run_sql("DROP TABLE IF EXISTS test_txn")
    run_sql("CREATE TABLE test_txn (id INTEGER PRIMARY KEY, value INTEGER)")
    run_sql("INSERT INTO test_txn VALUES (1, 100)")

    run_sql("BEGIN; UPDATE test_txn SET value = 200 WHERE id = 1; ROLLBACK")
    result, _ = run_sql("SELECT value FROM test_txn WHERE id = 1")
    if "100" not in result:
        log_error("ROLLBACK: FAILED (expected 100)")
        return False
    log_info("ROLLBACK: PASSED")

    run_sql("UPDATE test_txn SET value = 300 WHERE id = 1")
    result, _ = run_sql("SELECT value FROM test_txn WHERE id = 1")
    if "300" not in result:
        log_error("UPDATE (auto-commit): FAILED (expected 300)")
        return False
    log_info("UPDATE (auto-commit): PASSED")

    run_sql("DROP TABLE test_txn")
    log_info("Transaction tests: PASSED")
    return True


def test_savepoints() -> bool:
    log_info("Testing savepoints...")

    run_sql("DROP TABLE IF EXISTS test_savepoints")
    run_sql("CREATE TABLE test_savepoints (id INTEGER PRIMARY KEY, value INTEGER)")
    run_sql("INSERT INTO test_savepoints VALUES (1, 100)")

    # NOTE: pg-tikv currently returns only the result of the last statement in
    # a multi-statement simple query, so we validate savepoint semantics by
    # checking the final persisted state after COMMIT.

    # ROLLBACK TO SAVEPOINT should revert changes.
    out, code = run_sql(
        "BEGIN; SAVEPOINT a; UPDATE test_savepoints SET value = 200 WHERE id = 1; ROLLBACK TO a; COMMIT;"
    )
    if code != 0:
        log_error("ROLLBACK TO SAVEPOINT: FAILED")
        print(out[:200])
        return False
    result, _ = run_sql("SELECT value FROM test_savepoints WHERE id = 1")
    if "100" not in result:
        log_error("ROLLBACK TO SAVEPOINT: FAILED (expected 100)")
        print(result[:200])
        return False
    log_info("ROLLBACK TO SAVEPOINT: PASSED")

    # ROLLBACK TO should re-establish the savepoint (can rollback twice).
    run_sql("UPDATE test_savepoints SET value = 100 WHERE id = 1")
    out, code = run_sql(
        "BEGIN; SAVEPOINT a; UPDATE test_savepoints SET value = 200 WHERE id = 1; ROLLBACK TO a; "
        "UPDATE test_savepoints SET value = 300 WHERE id = 1; ROLLBACK TO a; COMMIT;"
    )
    if code != 0:
        log_error("ROLLBACK TO re-establish: FAILED")
        print(out[:200])
        return False
    result, _ = run_sql("SELECT value FROM test_savepoints WHERE id = 1")
    if "100" not in result:
        log_error("ROLLBACK TO re-establish: FAILED (expected 100)")
        print(result[:200])
        return False
    log_info("ROLLBACK TO re-establish: PASSED")

    # RELEASE should keep changes.
    run_sql("UPDATE test_savepoints SET value = 100 WHERE id = 1")
    out, code = run_sql(
        "BEGIN; SAVEPOINT a; UPDATE test_savepoints SET value = 200 WHERE id = 1; RELEASE SAVEPOINT a; COMMIT;"
    )
    if code != 0:
        log_error("RELEASE SAVEPOINT: FAILED")
        print(out[:200])
        return False
    result, _ = run_sql("SELECT value FROM test_savepoints WHERE id = 1")
    if "200" not in result:
        log_error("RELEASE SAVEPOINT: FAILED (expected 200)")
        print(result[:200])
        return False
    log_info("RELEASE SAVEPOINT: PASSED")

    # RELEASE must not prevent outer savepoint rollback.
    run_sql("UPDATE test_savepoints SET value = 100 WHERE id = 1")
    out, code = run_sql(
        "BEGIN; SAVEPOINT outer; SAVEPOINT inner; UPDATE test_savepoints SET value = 500 WHERE id = 1; "
        "RELEASE SAVEPOINT inner; ROLLBACK TO SAVEPOINT outer; COMMIT;"
    )
    if code != 0:
        log_error("Nested savepoint rollback after RELEASE: FAILED")
        print(out[:200])
        return False
    result, _ = run_sql("SELECT value FROM test_savepoints WHERE id = 1")
    if "100" not in result:
        log_error("Nested savepoint rollback after RELEASE: FAILED (expected 100)")
        print(result[:200])
        return False
    log_info("Nested savepoint rollback after RELEASE: PASSED")

    # SAVEPOINT outside a transaction should error.
    out, code = run_sql("SAVEPOINT should_fail")
    if code == 0 or "SAVEPOINT can only be used in transaction blocks" not in out:
        log_error("SAVEPOINT outside transaction: FAILED (expected error)")
        print(out[:200])
        return False
    log_info("SAVEPOINT outside transaction: PASSED")

    # RELEASE SAVEPOINT outside a transaction should error.
    out, code = run_sql("RELEASE SAVEPOINT should_fail")
    if code == 0 or "RELEASE SAVEPOINT can only be used in transaction blocks" not in out:
        log_error("RELEASE SAVEPOINT outside transaction: FAILED (expected error)")
        print(out[:200])
        return False
    log_info("RELEASE SAVEPOINT outside transaction: PASSED")

    # ROLLBACK TO SAVEPOINT outside a transaction should error.
    out, code = run_sql("ROLLBACK TO SAVEPOINT should_fail")
    if code == 0 or "ROLLBACK TO SAVEPOINT can only be used in transaction blocks" not in out:
        log_error("ROLLBACK TO SAVEPOINT outside transaction: FAILED (expected error)")
        print(out[:200])
        return False
    log_info("ROLLBACK TO SAVEPOINT outside transaction: PASSED")

    run_sql("DROP TABLE test_savepoints")
    log_info("Savepoint tests: PASSED")
    return True


def test_json_operations() -> bool:
    log_info("Testing JSON operations...")

    run_sql("DROP TABLE IF EXISTS test_json")
    run_sql("CREATE TABLE test_json (id SERIAL PRIMARY KEY, data JSONB)")
    run_sql("""INSERT INTO test_json (data) VALUES ('{"name": "Alice", "age": 30}')""")
    run_sql("""INSERT INTO test_json (data) VALUES ('{"name": "Bob", "age": 25}')""")

    result, _ = run_sql("SELECT data->>'name' FROM test_json WHERE id = 1")
    if "Alice" not in result:
        log_error(f"JSON extraction: FAILED - got: {result[:200]}")
        return False
    log_info("JSON extraction: PASSED")

    run_sql("DROP TABLE test_json")
    log_info("JSON operations: PASSED")
    return True


def test_query_features() -> bool:
    log_info("Testing advanced query features...")

    run_sql("DROP TABLE IF EXISTS orders_test")
    run_sql("DROP TABLE IF EXISTS customers_test")

    run_sql("""CREATE TABLE customers_test (
        id SERIAL PRIMARY KEY,
        name TEXT NOT NULL,
        city TEXT
    )""")

    run_sql("""CREATE TABLE orders_test (
        id SERIAL PRIMARY KEY,
        customer_id INT,
        amount DOUBLE PRECISION
    )""")

    run_sql("INSERT INTO customers_test (name, city) VALUES ('Alice', 'NYC'), ('Bob', 'LA')")
    run_sql("INSERT INTO orders_test (customer_id, amount) VALUES (1, 100), (1, 200), (2, 150)")

    result, _ = run_sql("""
        SELECT c.name, SUM(o.amount)
        FROM customers_test c
        JOIN orders_test o ON c.id = o.customer_id
        GROUP BY c.id, c.name
    """)
    if "Alice" not in result or "300" not in result:
        log_error("JOIN + GROUP BY: FAILED")
        return False
    log_info("JOIN + GROUP BY: PASSED")

    result, _ = run_sql("""
        SELECT name FROM customers_test
        WHERE id IN (SELECT customer_id FROM orders_test WHERE amount > 100)
    """)
    if "Alice" not in result:
        log_error("Subquery: FAILED")
        return False
    log_info("Subquery: PASSED")

    run_sql("DROP TABLE orders_test")
    run_sql("DROP TABLE customers_test")

    log_info("Advanced query features: PASSED")
    return True


def run_builtin_tests() -> TestStats:
    runner = TestRunner()

    log_info("=========================================")
    log_info("Running Built-in Integration Tests")
    log_info("=========================================")

    tests = [
        ("Basic Connection", test_basic_connection),
        ("DDL Operations", test_ddl_operations),
        ("ALTER TABLE Migration", test_alter_table_migration),
        ("DML Operations", test_dml_operations),
        ("Transactions", test_transactions),
        ("Savepoints", test_savepoints),
        ("JSON Operations", test_json_operations),
        ("Query Features", test_query_features),
    ]

    for name, test_func in tests:
        success = runner.run_test(name, test_func)
        if config.stop_on_error and not success:
            log_warn("Stopping on first error (--stop-on-error)")
            break

    return runner.stats


def parse_args() -> TestConfig:
    default_dsn = os.environ.get("PG_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres")

    parser = argparse.ArgumentParser(
        description="pg-tikv Integration Test Runner",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s --dsn postgres://admin:admin@127.0.0.1:5433/postgres
  %(prog)s --dsn postgres://... tests/basic.sql
  %(prog)s --dsn postgres://... tests/
        """,
    )
    parser.add_argument("tests", nargs="*", help="SQL test files or directories")
    parser.add_argument("--dsn", default=default_dsn, help="PostgreSQL connection DSN")
    parser.add_argument("--verbose", "-v", action="store_true", help="Show SQL output")
    parser.add_argument("--stop-on-error", "-x", action="store_true", help="Stop on first error")

    args = parser.parse_args()

    return TestConfig(
        db=DbConfig.from_dsn(args.dsn),
        verbose=args.verbose,
        stop_on_error=args.stop_on_error,
        test_files=[Path(t) for t in args.tests] if args.tests else [],
    )


def main():
    global config
    config = parse_args()

    log_info("pg-tikv Integration Test Runner")
    log_info("================================")
    log_info(f"DSN: {config.db.to_dsn()}")

    try:
        if not check_connection():
            log_error(f"Cannot connect to pg-tikv at {config.db.host}:{config.db.port}")
            log_error("Make sure pg-tikv is running:")
            log_error("  1. uv run scripts/tikv_admin.py start --persistent")
            log_error("  2. PD_ENDPOINTS=127.0.0.1:<pd_port> cargo run --release")
            sys.exit(1)
    except subprocess.TimeoutExpired:
        log_error(f"Connection timeout to {config.db.host}:{config.db.port}")
        sys.exit(1)

    log_info("Connection verified")

    if config.test_files:
        stats = run_external_tests(config.test_files)
    else:
        stats = run_builtin_tests()

    log_info("=========================================")
    log_info(f"Test Results: {stats.passed} passed, {stats.failed} failed, {stats.skipped} skipped")
    log_info("=========================================")

    if stats.failed == 0 and stats.errors == 0:
        log_info("All tests passed!")
        sys.exit(0)
    else:
        log_error("Some tests failed!")
        sys.exit(1)


if __name__ == "__main__":
    main()
