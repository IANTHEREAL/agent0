import { describe, it, expect, beforeAll, afterAll, beforeEach, afterEach } from 'vitest';
import pg from 'pg';
import { createPgPool, dropTableIfExists } from './client.js';
import { generateTableName } from '../shared/test-utils.js';

// Well-known PostgreSQL OIDs for field metadata assertions.
const PG_OID = {
  INT4: 23,
  INT8: 20,
  TEXT: 25,
  VARCHAR: 1043,
  FLOAT4: 700,
  FLOAT8: 701,
  BOOL: 16,
  TIMESTAMP: 1114,
};

describe('pg client - Prepared Statement Metadata & Schema Drift', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('prep_meta');
    await pool.query(`
      CREATE TABLE ${tableName} (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        score REAL,
        active BOOLEAN DEFAULT true,
        created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
      )
    `);
    await pool.query(`
      INSERT INTO ${tableName} (id, name, score, active) VALUES
      (1, 'Alice', 95.5, true),
      (2, 'Bob', 82.0, false),
      (3, 'Charlie', 91.3, true)
    `);
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('returns correct column metadata for named prepared SELECT', async () => {
    const client = await pool.connect();
    try {
      const stmtName = `meta_${Date.now()}`;
      const result = await client.query({
        name: stmtName,
        text: `SELECT id, name, score, active FROM ${tableName} ORDER BY id`,
        values: [],
      });

      // Verify field count and names.
      expect(result.fields.length).toBe(4);
      expect(result.fields.map((f) => f.name)).toEqual(['id', 'name', 'score', 'active']);

      // Verify OIDs match expected PostgreSQL types.
      expect(result.fields[0].dataTypeID).toBe(PG_OID.INT4);
      expect(result.fields[1].dataTypeID).toBe(PG_OID.TEXT);
      // tipg currently normalizes SQL float-family types to FLOAT8 in metadata.
      expect(result.fields[2].dataTypeID).toBe(PG_OID.FLOAT8);
      expect(result.fields[3].dataTypeID).toBe(PG_OID.BOOL);

      // Sanity-check the data.
      expect(result.rows.length).toBe(3);
      expect(result.rows[0].name).toBe('Alice');
    } finally {
      client.release();
    }
  });

  it('re-executes named prepared statement with different params', async () => {
    const client = await pool.connect();
    try {
      const stmtName = `reexec_${Date.now()}`;

      const r1 = await client.query({
        name: stmtName,
        text: `SELECT id, name FROM ${tableName} WHERE id = $1`,
        values: [1],
      });
      expect(r1.rows.length).toBe(1);
      expect(r1.rows[0].name).toBe('Alice');

      // Re-execute same named statement with a different param value.
      const r2 = await client.query({
        name: stmtName,
        text: `SELECT id, name FROM ${tableName} WHERE id = $1`,
        values: [2],
      });
      expect(r2.rows.length).toBe(1);
      expect(r2.rows[0].name).toBe('Bob');
    } finally {
      client.release();
    }
  });

  it('handles multiple parameter types (TEXT + REAL)', async () => {
    const client = await pool.connect();
    try {
      const stmtName = `multi_param_${Date.now()}`;
      const result = await client.query({
        name: stmtName,
        text: `SELECT id, name, score FROM ${tableName} WHERE name = $1 AND score > $2 ORDER BY id`,
        values: ['Alice', 90.0],
      });

      expect(result.rows.length).toBe(1);
      expect(result.rows[0].name).toBe('Alice');
      expect(result.rows[0].score).toBeGreaterThan(90);
    } finally {
      client.release();
    }
  });

  it('returns correct metadata for prepared INSERT with RETURNING', async () => {
    const client = await pool.connect();
    try {
      const stmtName = `ins_ret_${Date.now()}`;
      const result = await client.query({
        name: stmtName,
        text: `INSERT INTO ${tableName} (id, name, score, active) VALUES ($1, $2, $3, $4) RETURNING id, name, active`,
        values: [10, 'Zara', 99.0, true],
      });

      // Verify RETURNING field metadata.
      expect(result.fields.length).toBe(3);
      expect(result.fields.map((f) => f.name)).toEqual(['id', 'name', 'active']);
      expect(result.fields[0].dataTypeID).toBe(PG_OID.INT4);
      expect(result.fields[1].dataTypeID).toBe(PG_OID.TEXT);
      expect(result.fields[2].dataTypeID).toBe(PG_OID.BOOL);

      // Verify data.
      expect(result.rows.length).toBe(1);
      expect(result.rows[0].name).toBe('Zara');
      expect(result.rows[0].active).toBe(true);
    } finally {
      client.release();
    }
  });

  it('handles schema drift: ADD COLUMN after prepare (proves text fallback)', async () => {
    const client = await pool.connect();
    try {
      const stmtName = `drift_add_${Date.now()}`;

      // First execution with SELECT * — prepare caches the stale column set.
      const r1 = await client.query({
        name: stmtName,
        text: `SELECT * FROM ${tableName} WHERE id = $1`,
        values: [1],
      });
      const originalColCount = r1.fields.length;
      expect(r1.rows[0].name).toBe('Alice');
      // The new column must NOT be present yet.
      expect(r1.rows[0]).not.toHaveProperty('note');

      // ALTER TABLE via pool (different connection) to avoid client cache.
      await pool.query(`ALTER TABLE ${tableName} ADD COLUMN note TEXT DEFAULT 'n/a'`);

      // Re-execute the same named SELECT * query.
      // If drift detection + text fallback works, the SQL is re-parsed so
      // SELECT * now expands to include the new column.
      // If drift detection were broken (stale plan reused), the new column
      // would be absent — this assertion would fail.
      const r2 = await client.query({
        name: stmtName,
        text: `SELECT * FROM ${tableName} WHERE id = $1`,
        values: [2],
      });
      expect(r2.rows.length).toBe(1);
      expect(r2.rows[0].name).toBe('Bob');
      expect(r2.fields.length).toBe(originalColCount + 1);
      expect(r2.rows[0].note).toBe('n/a');
    } finally {
      client.release();
    }
  });

  it('handles schema drift: DROP referenced column', async () => {
    // Add an extra column, then drop it after prepare.
    await pool.query(`ALTER TABLE ${tableName} ADD COLUMN note TEXT DEFAULT 'hello'`);

    const client = await pool.connect();
    try {
      const stmtName = `drift_drop_${Date.now()}`;

      // Prepare with `note` in the projection.
      const r1 = await client.query({
        name: stmtName,
        text: `SELECT id, note FROM ${tableName} WHERE id = $1`,
        values: [1],
      });
      expect(r1.rows[0].note).toBe('hello');

      // DROP the referenced column via pool.
      await pool.query(`ALTER TABLE ${tableName} DROP COLUMN note`);

      // Re-execute — should error because `note` no longer exists.
      await expect(
        client.query({
          name: stmtName,
          text: `SELECT id, note FROM ${tableName} WHERE id = $1`,
          values: [2],
        }),
      ).rejects.toThrow();
    } finally {
      client.release();
    }
  });

  it('handles schema drift: DROP TABLE after prepare', async () => {
    const driftTable = generateTableName('drift_drop_tbl');
    await pool.query(`CREATE TABLE ${driftTable} (id INTEGER PRIMARY KEY, val TEXT)`);
    await pool.query(`INSERT INTO ${driftTable} (id, val) VALUES (1, 'x')`);

    const client = await pool.connect();
    try {
      const stmtName = `drift_tbl_${Date.now()}`;

      // Prepare.
      const r1 = await client.query({
        name: stmtName,
        text: `SELECT id, val FROM ${driftTable} WHERE id = $1`,
        values: [1],
      });
      expect(r1.rows[0].val).toBe('x');

      // DROP TABLE via pool.
      await pool.query(`DROP TABLE ${driftTable}`);

      // Re-execute — should error because the table is gone.
      await expect(
        client.query({
          name: stmtName,
          text: `SELECT id, val FROM ${driftTable} WHERE id = $1`,
          values: [1],
        }),
      ).rejects.toThrow();
    } finally {
      client.release();
    }
  });
});
