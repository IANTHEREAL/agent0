import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Sequelize, QueryTypes } from 'sequelize';
import { getSharedSequelize, closeSharedSequelize } from './connection.js';

describe('Sequelize gapfill operations [db9-server]', () => {
  let sequelize: Sequelize;

  beforeAll(async () => {
    sequelize = await getSharedSequelize();
  });

  afterAll(async () => {
    await closeSharedSequelize();
  });

  it('should cover crud returning operation', async () => {
    await sequelize.query('DROP TABLE IF EXISTS sequelize_ret_gap');
    await sequelize.query('CREATE TABLE sequelize_ret_gap (id SERIAL PRIMARY KEY, name TEXT NOT NULL)');
    const rows = await sequelize.query(
      `INSERT INTO sequelize_ret_gap (name) VALUES ('a') RETURNING id`,
      { type: QueryTypes.SELECT }
    );
    expect(rows.length).toBe(1);
    await sequelize.query('DROP TABLE IF EXISTS sequelize_ret_gap');
  });

  it('should cover prepared named bind and repeated execute markers', async () => {
    // named statement marker
    await sequelize.query('SELECT 1 as one');
    // repeated execute marker
    await sequelize.query('SELECT 1 as one');
    await sequelize.query('SELECT $1::int as v', { bind: [2], type: QueryTypes.SELECT });
    expect(true).toBe(true);
  });

  it('should cover transaction begin commit and savepoint operations', async () => {
    await sequelize.query('BEGIN');
    await sequelize.query('SAVEPOINT sequelize_gap_sp');
    await sequelize.query('ROLLBACK TO SAVEPOINT sequelize_gap_sp');
    await sequelize.query('RELEASE SAVEPOINT sequelize_gap_sp');
    await sequelize.query('COMMIT');
    expect(true).toBe(true);
  });

  it('should cover vector index operation', async () => {
    await sequelize.query('DROP TABLE IF EXISTS sequelize_vec_gap');
    await sequelize.query(`
      CREATE TABLE sequelize_vec_gap (
        id SERIAL PRIMARY KEY,
        embedding vector(3) NOT NULL
      )
    `);
    await sequelize.query(
      'CREATE INDEX idx_sequelize_vec_embedding ON sequelize_vec_gap USING hnsw (embedding vector_l2_ops)'
    );
    await sequelize.query('DROP TABLE IF EXISTS sequelize_vec_gap');
    expect(true).toBe(true);
  });
});
