import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Sequelize } from 'sequelize';
import { getSharedSequelize, closeSharedSequelize } from './connection.js';

describe('Sequelize DDL lifecycle operations [db9-server]', () => {
  let sequelize: Sequelize;

  beforeAll(async () => {
    sequelize = await getSharedSequelize();
  });

  afterAll(async () => {
    await closeSharedSequelize();
  });

  it('should run create schema, create table, alter table, create index, drop index, drop table, drop schema (migrate style)', async () => {
    await sequelize.query('DROP SCHEMA IF EXISTS sequelize_ddl_ops CASCADE');
    await sequelize.query('CREATE SCHEMA sequelize_ddl_ops');

    await sequelize.query(`
      CREATE TABLE sequelize_ddl_ops.users (
        id SERIAL PRIMARY KEY,
        email TEXT NOT NULL
      )
    `);

    await sequelize.query('ALTER TABLE sequelize_ddl_ops.users ADD COLUMN name TEXT');
    await sequelize.query('CREATE INDEX idx_sequelize_ddl_email ON sequelize_ddl_ops.users(email)');
    await sequelize.query('DROP INDEX sequelize_ddl_ops.idx_sequelize_ddl_email');

    await sequelize.query('DROP TABLE sequelize_ddl_ops.users');
    await sequelize.query('DROP SCHEMA sequelize_ddl_ops');

    expect(true).toBe(true);
  });
});
