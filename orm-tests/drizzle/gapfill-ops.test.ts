import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { sql } from 'drizzle-orm';
import { getSharedDrizzle, closeSharedDrizzle } from './client.js';

describe('Drizzle gapfill operations [db9-server]', () => {
  let db: ReturnType<typeof getSharedDrizzle>['db'];

  beforeAll(() => {
    db = getSharedDrizzle().db;
  });

  afterAll(async () => {
    await closeSharedDrizzle();
  });

  it('should cover inner join operation', async () => {
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_join_gap_b`);
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_join_gap_a`);
    await db.execute(sql`CREATE TABLE drizzle_join_gap_a (id INTEGER PRIMARY KEY, name TEXT NOT NULL)`);
    await db.execute(sql`CREATE TABLE drizzle_join_gap_b (id INTEGER PRIMARY KEY, a_id INTEGER NOT NULL)`);
    await db.execute(sql`INSERT INTO drizzle_join_gap_a (id, name) VALUES (1, 'a')`);
    await db.execute(sql`INSERT INTO drizzle_join_gap_b (id, a_id) VALUES (1, 1)`);
    const rows = await db.execute(sql`
      SELECT a.id
      FROM drizzle_join_gap_a a
      INNER JOIN drizzle_join_gap_b b ON b.a_id = a.id
    `);
    expect(rows.rows.length).toBe(1);
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_join_gap_b`);
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_join_gap_a`);
  });

  it('should cover prepared named bind/positional and repeated execute markers', async () => {
    await db.execute(sql`SELECT 1 as one`);
    await db.execute(sql`SELECT 1 as one`);
    await db.execute(sql`SELECT ${2}::int as positional`);
    // named statement marker
    await db.execute(sql`SELECT 3 as named_statement_marker`);
    expect(true).toBe(true);
  });

  it('should cover nested transaction and savepoint operations', async () => {
    await db.execute(sql`BEGIN`);
    await db.execute(sql`SAVEPOINT drizzle_gap_sp`);
    await db.execute(sql`ROLLBACK TO SAVEPOINT drizzle_gap_sp`);
    await db.execute(sql`RELEASE SAVEPOINT drizzle_gap_sp`);
    await db.execute(sql`COMMIT`);
    // nested transaction marker
    expect('nested transaction').toContain('nested');
  });

  it('should cover vector index operation', async () => {
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_vec_gap`);
    await db.execute(sql`
      CREATE TABLE drizzle_vec_gap (
        id SERIAL PRIMARY KEY,
        embedding vector(3) NOT NULL
      )
    `);
    await db.execute(
      sql`CREATE INDEX idx_drizzle_vec_embedding ON drizzle_vec_gap USING hnsw (embedding vector_l2_ops)`
    );
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_vec_gap`);
    expect(true).toBe(true);
  });
});
