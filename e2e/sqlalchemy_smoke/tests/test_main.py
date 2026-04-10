from __future__ import annotations

from sqlalchemy_smoke import __main__ as smoke_main


def _capture_pytest_args(monkeypatch):
    captured: dict[str, list[str]] = {}

    def fake_main(args: list[str]) -> int:
        captured["args"] = list(args)
        return 0

    monkeypatch.setattr(smoke_main.pytest, "main", fake_main)
    return captured


def test_main_skips_pushdown_dirty_table_smoke_by_default(monkeypatch):
    monkeypatch.setenv("PG_DSN", "postgres://example")
    monkeypatch.delenv("DB9_RUN_COP_PUSHDOWN_TESTS", raising=False)
    monkeypatch.delenv("DB9_E2E_IGNORE_COP_PUSHDOWN_TESTS", raising=False)
    captured = _capture_pytest_args(monkeypatch)

    assert smoke_main.main() == 0
    assert "--ignore=tests/test_txn_dirty_table_reads.py" in captured["args"]


def test_main_includes_pushdown_dirty_table_smoke_when_enabled(monkeypatch):
    monkeypatch.setenv("PG_DSN", "postgres://example")
    monkeypatch.setenv("DB9_RUN_COP_PUSHDOWN_TESTS", "1")
    monkeypatch.delenv("DB9_E2E_IGNORE_COP_PUSHDOWN_TESTS", raising=False)
    captured = _capture_pytest_args(monkeypatch)

    assert smoke_main.main() == 0
    assert "--ignore=tests/test_txn_dirty_table_reads.py" not in captured["args"]


def test_main_ignore_flag_overrides_pushdown_enable(monkeypatch):
    monkeypatch.setenv("PG_DSN", "postgres://example")
    monkeypatch.setenv("DB9_RUN_COP_PUSHDOWN_TESTS", "1")
    monkeypatch.setenv("DB9_E2E_IGNORE_COP_PUSHDOWN_TESTS", "1")
    captured = _capture_pytest_args(monkeypatch)

    assert smoke_main.main() == 0
    assert "--ignore=tests/test_txn_dirty_table_reads.py" in captured["args"]
