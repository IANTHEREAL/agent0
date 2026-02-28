import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Kysely, PostgresDialect, sql } from 'kysely';
import pg from 'pg';
import { getPgConfig } from '../shared/config.js';

const { Pool } = pg;

type DB = {
  kysely_ddl_tx_users: {
    id: number;
    email: string;
    name: string;
  };
};

describe('Kysely DDL lifecycle and transaction operations [db9-server]', () => {
  let db: Kysely<DB>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());
    db = new Kysely<DB>({ dialect: new PostgresDialect({ pool }) });
  });

  afterAll(async () => {
    await db.destroy();
  });

  it('should run DDL lifecycle operations: create schema/table, alter table, create/drop index, drop table/schema', async () => {
    await sql`DROP SCHEMA IF EXISTS ks_ddl_ops CASCADE`.execute(db);
    await sql`CREATE SCHEMA ks_ddl_ops`.execute(db);

    await sql`
      CREATE TABLE ks_ddl_ops.kysely_ddl_tx_users (
        id SERIAL PRIMARY KEY,
        email TEXT NOT NULL,
        name TEXT NOT NULL
      )
    `.execute(db);

    await sql`ALTER TABLE ks_ddl_ops.kysely_ddl_tx_users ADD COLUMN nick TEXT`.execute(db);
    await sql`CREATE INDEX idx_ks_ddl_ops_email ON ks_ddl_ops.kysely_ddl_tx_users(email)`.execute(db);
    await sql`DROP INDEX ks_ddl_ops.idx_ks_ddl_ops_email`.execute(db);

    await sql`DROP TABLE ks_ddl_ops.kysely_ddl_tx_users`.execute(db);
    await sql`DROP SCHEMA ks_ddl_ops`.execute(db);
  });

  it('should support transaction begin/commit and rollback', async () => {
    await sql`DROP TABLE IF EXISTS kysely_tx_ops`.execute(db);
    await sql`
      CREATE TABLE kysely_tx_ops (
        id SERIAL PRIMARY KEY,
        email TEXT NOT NULL
      )
    `.execute(db);

    await db.transaction().execute(async (trx) => {
      await sql`INSERT INTO kysely_tx_ops (email) VALUES ('commit@example.com')`.execute(trx);
    });

    const committed = await sql<{ cnt: number }[]>`
      SELECT COUNT(*)::int AS cnt FROM kysely_tx_ops WHERE email = 'commit@example.com'
    `.execute(db);
    expect(committed.rows[0].cnt).toBe(1);

    await expect(
      db.transaction().execute(async (trx) => {
        await sql`INSERT INTO kysely_tx_ops (email) VALUES ('rollback@example.com')`.execute(trx);
        throw new Error('force rollback');
      })
    ).rejects.toThrow();

    const rolledBack = await sql<{ cnt: number }[]>`
      SELECT COUNT(*)::int AS cnt FROM kysely_tx_ops WHERE email = 'rollback@example.com'
    `.execute(db);
    expect(rolledBack.rows[0].cnt).toBe(0);

    await sql`DROP TABLE IF EXISTS kysely_tx_ops`.execute(db);
  });

  it('should support savepoint and isolation level statements', async () => {
    await sql`BEGIN`.execute(db);
    await sql`SET TRANSACTION ISOLATION LEVEL READ COMMITTED`.execute(db);
    await sql`SAVEPOINT ks_sp1`.execute(db);
    await sql`ROLLBACK TO SAVEPOINT ks_sp1`.execute(db);
    await sql`RELEASE SAVEPOINT ks_sp1`.execute(db);
    await sql`COMMIT`.execute(db);
  });
});
