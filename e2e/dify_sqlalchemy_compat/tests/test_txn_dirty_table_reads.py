from __future__ import annotations


def test_dify_prepared_dirty_table_reads_stay_txn_visible(schema, db):
    with db.engine.connect() as conn:
        conn.exec_driver_sql("SET db9.enable_cop_pushdown = on")
        conn.exec_driver_sql(
            f"""
            CREATE TABLE "{schema}"."txn_dirty_reads" (
              id INTEGER PRIMARY KEY,
              value TEXT NOT NULL
            )
            """
        )
        conn.exec_driver_sql(
            f"""INSERT INTO "{schema}"."txn_dirty_reads" (id, value) VALUES (1, 'seed')"""
        )
        conn.commit()

        explain_rows = conn.exec_driver_sql(
            f"""
            EXPLAIN VERBOSE
            SELECT value
            FROM "{schema}"."txn_dirty_reads"
            WHERE id = 1
            LIMIT 1
            """
        ).fetchall()
        assert any("DB9 Cop" in row[0] for row in explain_rows), explain_rows

        conn.exec_driver_sql(
            f"""
            PREPARE txn_dirty_stmt (INTEGER) AS
            SELECT value
            FROM "{schema}"."txn_dirty_reads"
            WHERE id = $1
            LIMIT 1
            """
        )
        for _ in range(5):
            assert conn.exec_driver_sql("EXECUTE txn_dirty_stmt(1)").fetchall() == [("seed",)]
        conn.commit()

        with conn.begin():
            conn.exec_driver_sql(
                f"""INSERT INTO "{schema}"."txn_dirty_reads" (id, value) VALUES (2, 'inserted')"""
            )
            assert conn.exec_driver_sql("EXECUTE txn_dirty_stmt(2)").fetchall() == [
                ("inserted",)
            ]

        with conn.begin():
            conn.exec_driver_sql(
                f"""UPDATE "{schema}"."txn_dirty_reads" SET value = 'updated' WHERE id = 1"""
            )
            explain_rows = conn.exec_driver_sql(
                f"""
                EXPLAIN VERBOSE
                SELECT value
                FROM "{schema}"."txn_dirty_reads"
                WHERE id = 1
                LIMIT 1
                """
            ).fetchall()
            assert all("DB9 Cop" not in row[0] for row in explain_rows), explain_rows
            assert conn.exec_driver_sql("EXECUTE txn_dirty_stmt(1)").fetchall() == [
                ("updated",)
            ]

        with conn.begin():
            conn.exec_driver_sql(f"""DELETE FROM "{schema}"."txn_dirty_reads" WHERE id = 1""")
            explain_rows = conn.exec_driver_sql(
                f"""
                EXPLAIN VERBOSE
                SELECT value
                FROM "{schema}"."txn_dirty_reads"
                WHERE id = 1
                LIMIT 1
                """
            ).fetchall()
            assert all("DB9 Cop" not in row[0] for row in explain_rows), explain_rows
            assert (
                conn.exec_driver_sql(
                    f"""SELECT count(*) FROM "{schema}"."txn_dirty_reads" WHERE id = 1"""
                ).scalar_one()
                == 0
            )

        assert conn.exec_driver_sql(
            f"""SELECT id, value FROM "{schema}"."txn_dirty_reads" ORDER BY id"""
        ).fetchall() == [(2, "inserted")]

        conn.exec_driver_sql("DEALLOCATE txn_dirty_stmt")
        conn.commit()
