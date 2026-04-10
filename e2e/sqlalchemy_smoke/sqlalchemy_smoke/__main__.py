from __future__ import annotations

import os
import sys

import pytest


def _env_flag(name: str) -> bool:
    raw = os.environ.get(name, "")
    return raw.lower() in {"1", "true", "t", "yes", "y", "on"}


def _should_run_cop_pushdown_tests() -> bool:
    if _env_flag("DB9_E2E_IGNORE_COP_PUSHDOWN_TESTS"):
        return False
    return _env_flag("DB9_RUN_COP_PUSHDOWN_TESTS")


def main() -> int:
    if not os.environ.get("PG_DSN"):
        print("PG_DSN is required (e.g. postgres://user:pass@127.0.0.1:5433/postgres)", file=sys.stderr)
        return 2
    args = ["-q", "tests"]
    if not _should_run_cop_pushdown_tests():
        args.append("--ignore=tests/test_txn_dirty_table_reads.py")
    return pytest.main(args)


if __name__ == "__main__":
    raise SystemExit(main())
