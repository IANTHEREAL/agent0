from __future__ import annotations

from sqlalchemy_smoke.harness import SCHEMA_NAME, engine_from_env, managed_schema


def test_sqlalchemy_prepared_dirty_table_reads_stay_txn_visible():
    engine = engine_from_env()

    try:
        with managed_schema(engine, SCHEMA_NAME):
            with engine.connect() as conn:
                conn.exec_driver_sql("SET db9.enable_cop_pushdown = on")
                conn.exec_driver_sql(
                    f"""
                    CREATE TABLE {SCHEMA_NAME}.txn_dirty_reads (
                      id INTEGER PRIMARY KEY,
                      value TEXT NOT NULL
                    )
                    """
                )
                conn.exec_driver_sql(
                    f"INSERT INTO {SCHEMA_NAME}.txn_dirty_reads (id, value) VALUES (1, 'seed')"
                )
                conn.commit()

                explain_rows = conn.exec_driver_sql(
                    f"""
                    EXPLAIN VERBOSE
                    SELECT value
                    FROM {SCHEMA_NAME}.txn_dirty_reads
                    WHERE id = 1
                    LIMIT 1
                    """
                ).fetchall()
                assert any("DB9 Cop" in row[0] for row in explain_rows), explain_rows

                raw = conn.connection.driver_connection
                cur = raw.cursor()

                stmt = (
                    f"SELECT value FROM {SCHEMA_NAME}.txn_dirty_reads "
                    "WHERE id = %s LIMIT 1"
                )
                for _ in range(6):
                    cur.execute(stmt, (1,), prepare=True)
                    assert cur.fetchall() == [("seed",)]
                raw.commit()

                cur.execute("BEGIN")
                cur.execute(
                    f"INSERT INTO {SCHEMA_NAME}.txn_dirty_reads (id, value) VALUES (2, 'inserted')"
                )
                cur.execute(stmt, (2,), prepare=True)
                assert cur.fetchall() == [("inserted",)]
                cur.execute("COMMIT")

                cur.execute("BEGIN")
                cur.execute(
                    f"UPDATE {SCHEMA_NAME}.txn_dirty_reads SET value = 'updated' WHERE id = 1"
                )
                cur.execute(
                    f"""
                    EXPLAIN VERBOSE
                    SELECT value
                    FROM {SCHEMA_NAME}.txn_dirty_reads
                    WHERE id = 1
                    LIMIT 1
                    """
                )
                explain_rows = cur.fetchall()
                assert all("DB9 Cop" not in row[0] for row in explain_rows), explain_rows
                cur.execute(stmt, (1,), prepare=True)
                assert cur.fetchall() == [("updated",)]
                cur.execute("COMMIT")

                cur.execute("BEGIN")
                cur.execute(f"DELETE FROM {SCHEMA_NAME}.txn_dirty_reads WHERE id = 1")
                cur.execute(
                    f"""
                    EXPLAIN VERBOSE
                    SELECT value
                    FROM {SCHEMA_NAME}.txn_dirty_reads
                    WHERE id = 1
                    LIMIT 1
                    """
                )
                explain_rows = cur.fetchall()
                assert all("DB9 Cop" not in row[0] for row in explain_rows), explain_rows
                cur.execute(f"SELECT count(*) FROM {SCHEMA_NAME}.txn_dirty_reads WHERE id = 1")
                assert cur.fetchone() == (0,)
                cur.execute("COMMIT")

                assert conn.exec_driver_sql(
                    f"SELECT id, value FROM {SCHEMA_NAME}.txn_dirty_reads ORDER BY id"
                ).fetchall() == [(2, "inserted")]
    finally:
        engine.dispose()
