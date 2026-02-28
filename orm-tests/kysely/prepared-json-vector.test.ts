import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { Kysely, PostgresDialect, sql } from 'kysely';
import pg from 'pg';
import { getPgConfig } from '../shared/config.js';

const { Pool } = pg;

type DB = {
  kysely_prepared_json_vector: {
    id: number;
    name: string;
    payload: unknown;
    tags: string[] | string;
    embedding: string;
  };
};

describe('Kysely Prepared Statement JSON Array Vector [db9-server]', () => {
  let db: Kysely<DB>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());
    db = new Kysely<DB>({ dialect: new PostgresDialect({ pool }) });

    await sql`
      CREATE TABLE IF NOT EXISTS kysely_prepared_json_vector (
        id SERIAL PRIMARY KEY,
        name TEXT NOT NULL,
        payload JSONB NOT NULL,
        tags TEXT[] NOT NULL,
        embedding vector(3) NOT NULL
      )
    `.execute(db);
  });

  afterAll(async () => {
    await sql`DROP TABLE IF EXISTS kysely_prepared_json_vector`.execute(db);
    await db.destroy();
  });

  beforeEach(async () => {
    await sql`DELETE FROM kysely_prepared_json_vector`.execute(db);
  });

  it('should execute prepared statement with bind parameter filter', async () => {
    await sql`
      INSERT INTO kysely_prepared_json_vector (name, payload, tags, embedding)
      VALUES ('prepared_kysely', '{"kind":"prepared"}'::jsonb, ARRAY['a'], '[1.0,2.0,3.0]')
    `.execute(db);

    const name = 'prepared_kysely';
    const row = await db
      .selectFrom('kysely_prepared_json_vector')
      .select(['id', 'name'])
      .where('name', '=', name)
      .executeTakeFirst();

    expect(row?.name).toBe('prepared_kysely');
  });

  it('should query json and array values', async () => {
    await sql`
      INSERT INTO kysely_prepared_json_vector (name, payload, tags, embedding)
      VALUES ('json_array_case', '{"kind":"json"}'::jsonb, ARRAY['x','y'], '[1.0,1.0,1.0]')
    `.execute(db);

    const rows = await sql<{ payload: unknown; tags: string[] | string }[]>`
      SELECT payload, tags
      FROM kysely_prepared_json_vector
      WHERE name = 'json_array_case'
    `.execute(db);

    expect(rows.rows).toHaveLength(1);
    expect(rows.rows[0].payload).toBeDefined();
    expect(rows.rows[0].tags).toBeDefined();
  });

  it('should insert and read vector value', async () => {
    await sql`
      INSERT INTO kysely_prepared_json_vector (name, payload, tags, embedding)
      VALUES ('vector_case', '{"kind":"vector"}'::jsonb, ARRAY['v'], '[3.0,4.0,5.0]')
    `.execute(db);

    const rows = await sql<{ embedding: string }[]>`
      SELECT embedding FROM kysely_prepared_json_vector WHERE name = 'vector_case'
    `.execute(db);

    expect(rows.rows).toHaveLength(1);
    expect(rows.rows[0].embedding).toBe('[3,4,5]');
  });
});

