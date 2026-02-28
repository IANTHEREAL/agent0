import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { sql } from 'drizzle-orm';
import { drizzle, NodePgDatabase } from 'drizzle-orm/node-postgres';
import pg from 'pg';
import { getPgConfig } from '../shared/config.js';

const { Pool } = pg;

describe('Drizzle Prepared Statement Compatibility [db9-server]', () => {
  let pool: pg.Pool;
  let db: NodePgDatabase;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());
    db = drizzle(pool);
    await db.execute(sql`
      CREATE TABLE IF NOT EXISTS drizzle_prepared_cases (
        id SERIAL PRIMARY KEY,
        name TEXT NOT NULL,
        score INT NOT NULL
      )
    `);
  });

  afterAll(async () => {
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_prepared_cases`);
    await pool.end();
  });

  beforeEach(async () => {
    await db.execute(sql`DELETE FROM drizzle_prepared_cases`);
  });

  it('should execute prepared parameter insert and select', async () => {
    const name = 'prepared_user';
    const score = 42;

    await db.execute(
      sql`INSERT INTO drizzle_prepared_cases (name, score) VALUES (${name}, ${score})`
    );

    const result = await db.execute(
      sql`SELECT score FROM drizzle_prepared_cases WHERE name = ${name}`
    );
    expect(result.rows).toHaveLength(1);
    expect(Number(result.rows[0].score)).toBe(42);
  });
});

