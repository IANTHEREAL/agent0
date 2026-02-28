from __future__ import annotations

from sqlalchemy import text


def test_dify_sqlalchemy_gapfill_ops(schema, db):
    with db.engine.begin() as conn:
        # vector column / vector insert / vector index / vector filter
        conn.exec_driver_sql(
            f"""
            CREATE TABLE "{schema}"."dify_vec_gap" (
              id INTEGER PRIMARY KEY,
              embedding vector(3) NOT NULL
            )
            """
        )
        conn.exec_driver_sql(
            f'CREATE INDEX "idx_dify_vec_gap_embedding" ON "{schema}"."dify_vec_gap" USING ivfflat (embedding)'
        )
        conn.exec_driver_sql(
            f"""
            INSERT INTO "{schema}"."dify_vec_gap" (id, embedding)
            VALUES (1, '[1,0,0]'), (2, '[0,1,0]')
            """
        )
        rows = conn.exec_driver_sql(
            f"""
            SELECT id
            FROM "{schema}"."dify_vec_gap"
            WHERE embedding <-> '[1,0,0]' < 2.0
            ORDER BY embedding <-> '[1,0,0]'
            """
        ).fetchall()
        assert rows[0] == (1,)

        # batch_write
        conn.exec_driver_sql(
            f"""
            CREATE TABLE "{schema}"."dify_batch_gap" (
              id INTEGER PRIMARY KEY,
              name TEXT NOT NULL
            )
            """
        )
        conn.exec_driver_sql(
            f"""INSERT INTO "{schema}"."dify_batch_gap" (id, name) VALUES (1, 'a'), (2, 'b')"""
        )

        # join_and_subquery: inner_join / left_join / window
        conn.exec_driver_sql(
            f"""
            CREATE TABLE "{schema}"."dify_join_a" (
              id INTEGER PRIMARY KEY,
              name TEXT NOT NULL
            )
            """
        )
        conn.exec_driver_sql(
            f"""
            CREATE TABLE "{schema}"."dify_join_b" (
              id INTEGER PRIMARY KEY,
              a_id INTEGER NOT NULL
            )
            """
        )
        conn.exec_driver_sql(f"""INSERT INTO "{schema}"."dify_join_a" (id, name) VALUES (1, 'x')""")
        conn.exec_driver_sql(f"""INSERT INTO "{schema}"."dify_join_b" (id, a_id) VALUES (1, 1)""")

        inner_rows = conn.exec_driver_sql(
            f"""
            SELECT a.id
            FROM "{schema}"."dify_join_a" a
            INNER JOIN "{schema}"."dify_join_b" b ON b.a_id = a.id
            """
        ).fetchall()
        assert inner_rows == [(1,)]

        left_rows = conn.exec_driver_sql(
            f"""
            SELECT a.id
            FROM "{schema}"."dify_join_a" a
            LEFT JOIN "{schema}"."dify_join_b" b ON b.a_id = a.id
            """
        ).fetchall()
        assert left_rows == [(1,)]

        win_rows = conn.exec_driver_sql(
            f"""
            SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn
            FROM "{schema}"."dify_join_a"
            """
        ).fetchall()
        assert win_rows == [(1, 1)]

        # prepared named statement / positional bind / repeated execute markers
        named_stmt = text(f'SELECT id FROM "{schema}"."dify_batch_gap" WHERE id = :id')
        r1 = conn.execute(named_stmt, {"id": 1}).fetchall()
        r2 = conn.execute(named_stmt, {"id": 1}).fetchall()  # repeated execute
        assert r1 == [(1,)]
        assert r2 == [(1,)]

        positional = conn.exec_driver_sql("SELECT $1::int AS v", (2,)).fetchall()
        assert positional == [(2,)]
