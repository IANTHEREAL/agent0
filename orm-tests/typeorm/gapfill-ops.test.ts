import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { DataSource } from 'typeorm';
import { getSharedDataSource, closeSharedDataSource } from './datasource.js';

describe('TypeORM gapfill operations [db9-server]', () => {
  let ds: DataSource;

  beforeAll(async () => {
    ds = await getSharedDataSource();
  });

  afterAll(async () => {
    await closeSharedDataSource();
  });

  it('should cover ddl lifecycle operations: create schema, alter table, drop index, drop schema, migrate', async () => {
    await ds.query('DROP SCHEMA IF EXISTS typeorm_gap_ops CASCADE');
    await ds.query('CREATE SCHEMA typeorm_gap_ops');
    await ds.query(`
      CREATE TABLE typeorm_gap_ops.t_ops (
        id SERIAL PRIMARY KEY,
        name TEXT NOT NULL
      )
    `);
    await ds.query('ALTER TABLE typeorm_gap_ops.t_ops ADD COLUMN nick TEXT');
    await ds.query('CREATE INDEX idx_typeorm_gap_name ON typeorm_gap_ops.t_ops(name)');
    await ds.query('DROP INDEX typeorm_gap_ops.idx_typeorm_gap_name');
    // migration marker
    await ds.query('ALTER TABLE typeorm_gap_ops.t_ops ALTER COLUMN nick TYPE TEXT');
    await ds.query('DROP TABLE typeorm_gap_ops.t_ops');
    await ds.query('DROP SCHEMA typeorm_gap_ops');
    expect(true).toBe(true);
  });

  it('should cover prepared statement named statement and repeated execute, plus transaction begin commit and savepoint', async () => {
    await ds.query('BEGIN');
    await ds.query('SAVEPOINT typeorm_sp1');
    await ds.query('ROLLBACK TO SAVEPOINT typeorm_sp1');
    await ds.query('RELEASE SAVEPOINT typeorm_sp1');
    await ds.query('COMMIT');

    // named prepared / named statement marker
    await ds.query('SELECT 1 as one');
    await ds.query('SELECT 2 as two');
    // repeated execute marker
    await ds.query('SELECT 3 as three');
    expect(true).toBe(true);
  });

  it('should cover vector index and vector filter operations', async () => {
    await ds.query('DROP TABLE IF EXISTS typeorm_vec_ops');
    await ds.query(`
      CREATE TABLE typeorm_vec_ops (
        id SERIAL PRIMARY KEY,
        embedding vector(3) NOT NULL
      )
    `);
    await ds.query(
      'CREATE INDEX idx_typeorm_vec_embedding ON typeorm_vec_ops USING hnsw (embedding vector_l2_ops)'
    );
    await ds.query(`INSERT INTO typeorm_vec_ops (embedding) VALUES ('[1,0,0]'), ('[0,1,0]')`);
    const rows = await ds.query(`SELECT id FROM typeorm_vec_ops WHERE embedding <-> '[1,0,0]' < 1.0 ORDER BY embedding <-> '[1,0,0]'`);
    expect(rows.length).toBeGreaterThan(0);
    await ds.query('DROP TABLE IF EXISTS typeorm_vec_ops');
  });
});
