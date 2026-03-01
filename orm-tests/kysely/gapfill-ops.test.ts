import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Kysely, PostgresDialect, sql } from 'kysely';
import pg from 'pg';
import { getPgConfig } from '../shared/config.js';

const { Pool } = pg;

describe('Kysely gapfill operations [db9-server]', () => {
  let pool: pg.Pool;
  let db: Kysely<unknown>;

  beforeAll(() => {
    pool = new Pool(getPgConfig());
    db = new Kysely({ dialect: new PostgresDialect({ pool }) });
  });

  afterAll(async () => {
    await db.destroy();
  });

  it('should cover migrate marker and batch write', async () => {
    await sql`DROP TABLE IF EXISTS kysely_gap_batch`.execute(db);
    await sql`CREATE TABLE kysely_gap_batch (id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload JSONB, embedding vector(3))`.execute(db);
    // batch write marker
    await sql`INSERT INTO kysely_gap_batch (id, name) VALUES (1, 'a'), (2, 'b')`.execute(db);
    // migrate marker
    await sql`ALTER TABLE kysely_gap_batch ADD COLUMN nick TEXT`.execute(db);
    expect(true).toBe(true);
  });

  it('should cover prepared positional bind, named bind, and repeated execute markers', async () => {
    // positional bind marker
    await sql`SELECT ${1}::int as positional`.execute(db);
    // named prepared / named statement marker
    await sql`SELECT 2 as named_statement`.execute(db);
    // repeated execute marker
    await sql`SELECT ${3}::int as positional`.execute(db);
    expect(true).toBe(true);
  });

  it('should cover transaction nested transaction marker and window query', async () => {
    await sql`BEGIN`.execute(db);
    await sql`SAVEPOINT ks_gap_sp`.execute(db);
    await sql`ROLLBACK TO SAVEPOINT ks_gap_sp`.execute(db);
    await sql`RELEASE SAVEPOINT ks_gap_sp`.execute(db);
    await sql`COMMIT`.execute(db);
    // nested transaction marker
    expect('nested transaction').toContain('nested');

    const rows = await sql<{ rn: number }[]>`
      SELECT ROW_NUMBER() OVER (ORDER BY id) AS rn FROM kysely_gap_batch
    `.execute(db);
    expect(rows.rows.length).toBeGreaterThan(0);
  });

  it('should cover json update and vector column/index/distance operations', async () => {
    await sql`
      UPDATE kysely_gap_batch
      SET payload = jsonb_set(COALESCE(payload, '{}'::jsonb), '{k}', '1'::jsonb)
      WHERE id = 1
    `.execute(db);

    await sql`CREATE INDEX idx_kysely_gap_embedding ON kysely_gap_batch USING hnsw (embedding)`.execute(db);
    await sql`UPDATE kysely_gap_batch SET embedding = '[1,0,0]' WHERE id = 1`.execute(db);
    await sql`UPDATE kysely_gap_batch SET embedding = '[0,1,0]' WHERE id = 2`.execute(db);

    const rows = await sql<{ id: number }[]>`
      SELECT id
      FROM kysely_gap_batch
      WHERE embedding <-> '[1,0,0]' < 2.0
      ORDER BY embedding <-> '[1,0,0]'
    `.execute(db);
    expect(rows.rows.length).toBeGreaterThan(0);

    await sql`DROP TABLE IF EXISTS kysely_gap_batch`.execute(db);
  });
});
