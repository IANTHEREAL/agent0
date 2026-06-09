from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from sqlalchemy import text

from sqlalchemy_smoke.harness import SCHEMA_NAME, engine_from_env, managed_schema


MANIFEST_PATH = Path(__file__).resolve().parents[2] / "pushdown_coverage_manifest.json"
MANIFEST = json.loads(MANIFEST_PATH.read_text())
SUITES = MANIFEST["suites"]


def _env_flag(name: str) -> bool:
    return os.environ.get(name, "").strip().lower() in {"1", "true", "t", "yes", "y", "on"}


def _pushdown_tests_enabled() -> bool:
    if _env_flag("DB9_E2E_IGNORE_COP_PUSHDOWN_TESTS"):
        return False
    return _env_flag("DB9_RUN_COP_PUSHDOWN_TESTS")


def _render_sql(sql_text: str) -> str:
    return sql_text.replace("{schema}", SCHEMA_NAME)


def _qualified_table(table_name: str) -> str:
    return f"{SCHEMA_NAME}.{table_name}"


def _run_with_pushdown(engine, enabled: bool, fn):
    setting = "on" if enabled else "off"
    with engine.begin() as conn:
        conn.exec_driver_sql(f"SET LOCAL db9.enable_cop_pushdown = {setting}")
        return fn(conn)


def _execute_sql(conn, sql_text: str):
    return conn.execute(text(sql_text))


def _build_update_query(table_name: str, suite: dict, case: dict) -> str:
    qualified_table = _qualified_table(table_name)
    source_qualified_table = _qualified_table(suite["base_table"])
    update_strategy = suite.get("update_strategy", "direct_filter")

    if update_strategy == "direct_filter":
        return (
            f"UPDATE {qualified_table} "
            f"SET marker = '{case['name']}' "
            f"WHERE {MANIFEST['select_filter']} "
            f"AND (({case['expr']}) IS NOT DISTINCT FROM {case['expected_sql']})"
        )

    if update_strategy == "projected_match_by_id":
        # Keep the update statement separate per case, but source the match from
        # the already-green base-table select path instead of the update clone.
        return (
            f"UPDATE {qualified_table} "
            f"SET marker = '{case['name']}' "
            f"WHERE id = ("
            f"SELECT id FROM {source_qualified_table} "
            f"WHERE {MANIFEST['select_filter']} "
            f"AND (({case['expr']}) IS NOT DISTINCT FROM {case['expected_sql']}) LIMIT 1"
            f")"
        )

    raise AssertionError(f"unsupported pushdown coverage update strategy: {update_strategy}")


def _build_select_match_query(suite: dict, case: dict) -> str:
    return (
        f"SELECT id FROM {_qualified_table(suite['base_table'])} "
        f"WHERE {MANIFEST['select_filter']} "
        f"AND (({case['expr']}) IS NOT DISTINCT FROM {case['expected_sql']}) "
        f"ORDER BY id"
    )


def _build_post_update_select_query(table_name: str, case: dict) -> str:
    return f"SELECT id FROM {_qualified_table(table_name)} WHERE marker = '{case['name']}' ORDER BY id"


def _create_update_clone(engine, source_table: str, clone_table: str) -> None:
    source = _qualified_table(source_table)
    clone = _qualified_table(clone_table)
    with engine.begin() as conn:
        # Materialize the clone directly; db9-server does not accept LIKE ... INCLUDING ALL.
        _execute_sql(conn, f"CREATE TABLE {clone} AS SELECT * FROM {source}")
        _execute_sql(conn, f"ANALYZE {clone}")


def _marker_snapshot(engine, table_name: str):
    with engine.connect() as conn:
        return _execute_sql(
            conn,
            f"SELECT id, marker FROM {_qualified_table(table_name)} ORDER BY id",
        ).fetchall()


@pytest.mark.parametrize("suite", SUITES, ids=[suite["name"] for suite in SUITES])
def test_sqlalchemy_pushdown_function_operator_coverage(suite):
    if not _pushdown_tests_enabled():
        pytest.skip("DB9_RUN_COP_PUSHDOWN_TESTS not enabled; skipping pushdown coverage suite")

    engine = engine_from_env()

    try:
        with managed_schema(engine, SCHEMA_NAME):
            for stmt in suite["setup_sql"]:
                with engine.begin() as conn:
                    _execute_sql(conn, _render_sql(stmt))

            update_on_table = f"{suite['base_table']}_update_on"
            update_off_table = f"{suite['base_table']}_update_off"
            _create_update_clone(engine, suite["base_table"], update_on_table)
            _create_update_clone(engine, suite["base_table"], update_off_table)

            for case in suite["cases"]:
                case_id = f"{suite['name']}/{case['name']}"
                alias = f"{case['name']}_out"
                select_query = (
                    f"SELECT {case['expr']} AS {alias} "
                    f"FROM {_qualified_table(suite['base_table'])} "
                    f"WHERE {MANIFEST['select_filter']} LIMIT 1"
                )
                explain_rows = _run_with_pushdown(
                    engine,
                    True,
                    lambda conn: _execute_sql(conn, f"EXPLAIN VERBOSE {select_query}").fetchall(),
                )
                assert any(f"DB9 Cop Output: {alias}" in row[0] for row in explain_rows), (case_id, explain_rows)

                select_match_query = _build_select_match_query(suite, case)
                select_on = _run_with_pushdown(
                    engine,
                    True,
                    lambda conn: _execute_sql(conn, select_match_query).fetchall(),
                )
                assert len(select_on) == 1, (case_id, "select_on_rows", select_on)
                select_off = _run_with_pushdown(
                    engine,
                    False,
                    lambda conn: _execute_sql(conn, select_match_query).fetchall(),
                )
                assert select_on == select_off, (case_id, "select_parity", select_on, select_off)

                with engine.begin() as conn:
                    _execute_sql(conn, f"UPDATE {_qualified_table(update_on_table)} SET marker = NULL")
                    _execute_sql(conn, f"UPDATE {_qualified_table(update_off_table)} SET marker = NULL")

                update_on_query = _build_update_query(update_on_table, suite, case)
                rows_on = _run_with_pushdown(
                    engine,
                    True,
                    lambda conn: _execute_sql(conn, update_on_query).rowcount,
                )
                assert rows_on == 1, (case_id, "update_on_rows", rows_on)

                update_off_query = _build_update_query(update_off_table, suite, case)
                rows_off = _run_with_pushdown(
                    engine,
                    False,
                    lambda conn: _execute_sql(conn, update_off_query).rowcount,
                )
                assert rows_off == 1, (case_id, "update_off_rows", rows_off)

                assert _marker_snapshot(engine, update_on_table) == _marker_snapshot(engine, update_off_table), case_id

                post_update_on_query = _build_post_update_select_query(update_on_table, case)
                post_update_on = _run_with_pushdown(
                    engine,
                    True,
                    lambda conn: _execute_sql(conn, post_update_on_query).fetchall(),
                )
                assert len(post_update_on) == 1, (case_id, "post_update_select_on", post_update_on)

                post_update_off_query = _build_post_update_select_query(update_off_table, case)
                post_update_off = _run_with_pushdown(
                    engine,
                    False,
                    lambda conn: _execute_sql(conn, post_update_off_query).fetchall(),
                )
                assert post_update_on == post_update_off, (
                    case_id,
                    "post_update_select_parity",
                    post_update_on,
                    post_update_off,
                )
    finally:
        engine.dispose()
