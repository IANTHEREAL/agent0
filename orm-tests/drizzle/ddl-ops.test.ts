import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { sql } from 'drizzle-orm';
import { getSharedDrizzle, closeSharedDrizzle } from './client.js';

describe('Drizzle DDL lifecycle operations [db9-server]', () => {
  let db: ReturnType<typeof getSharedDrizzle>['db'];

  beforeAll(() => {
    const client = getSharedDrizzle();
    db = client.db;
  });

  afterAll(async () => {
    await closeSharedDrizzle();
  });

  it('should run create schema, create table, alter table, create index, drop index, drop table, drop schema (migrate style)', async () => {
    await db.execute(sql`DROP SCHEMA IF EXISTS drizzle_ddl_ops CASCADE`);
    await db.execute(sql`CREATE SCHEMA drizzle_ddl_ops`);

    await db.execute(sql`
      CREATE TABLE drizzle_ddl_ops.users (
        id SERIAL PRIMARY KEY,
        email TEXT NOT NULL
      )
    `);

    await db.execute(sql`ALTER TABLE drizzle_ddl_ops.users ADD COLUMN name TEXT`);
    await db.execute(sql`CREATE INDEX idx_drizzle_ddl_email ON drizzle_ddl_ops.users(email)`);
    await db.execute(sql`DROP INDEX drizzle_ddl_ops.idx_drizzle_ddl_email`);

    await db.execute(sql`DROP TABLE drizzle_ddl_ops.users`);
    await db.execute(sql`DROP SCHEMA drizzle_ddl_ops`);

    expect(true).toBe(true);
  });
});
