import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import pg from 'pg';
import { createPgPool } from './client.js';

describe('pg client - Extended Protocol Transaction Control', () => {
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  it('supports BEGIN/COMMIT via extended protocol', async () => {
    const client = await pool.connect();
    try {
      await client.query({ text: 'BEGIN', queryMode: 'extended' });
      const res = await client.query<{ one: number }>({
        text: 'SELECT 1 AS one',
        queryMode: 'extended',
      });
      expect(res.rows[0].one).toBe(1);
      await client.query({ text: 'COMMIT', queryMode: 'extended' });
    } finally {
      client.release();
    }
  });

  it('supports SAVEPOINT/ROLLBACK TO/RELEASE via extended protocol', async () => {
    const client = await pool.connect();
    try {
      await client.query({ text: 'BEGIN', queryMode: 'extended' });
      await client.query({ text: 'SAVEPOINT sp1', queryMode: 'extended' });
      await client.query({ text: 'ROLLBACK TO SAVEPOINT sp1', queryMode: 'extended' });
      await client.query({ text: 'RELEASE SAVEPOINT sp1', queryMode: 'extended' });
      await client.query({ text: 'ROLLBACK', queryMode: 'extended' });
    } finally {
      client.release();
    }
  });

  it('supports recursive CTE via named prepared statement', async () => {
    const client = await pool.connect();
    try {
      const sql = `
        WITH RECURSIVE numbers AS (
          SELECT 1 AS n
          UNION ALL
          SELECT n + 1 FROM numbers WHERE n < 5
        )
        SELECT * FROM numbers ORDER BY n
      `;

      const result = await client.query<{ n: number }>({
        name: `recursive_cte_stmt_${Date.now()}`,
        text: sql,
        values: [],
      });

      expect(result.rows.map((r) => r.n)).toEqual([1, 2, 3, 4, 5]);
    } finally {
      client.release();
    }
  });
});
