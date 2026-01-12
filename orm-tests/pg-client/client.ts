import pg from 'pg';
const { Pool } = pg;
import { defaultConfig, getPgConfig } from '../shared/config.js';

/**
 * Create a new pg Pool instance
 */
export function createPgPool(): pg.Pool {
  return new Pool({
    ...getPgConfig(),
    max: 5,
    idleTimeoutMillis: 30000,
    connectionTimeoutMillis: 10000,
  });
}

/**
 * Execute SQL statements (for setup/teardown)
 */
export async function execSQL(pool: pg.Pool, sql: string): Promise<void> {
  const statements = sql
    .split(';')
    .map(s => s.trim())
    .filter(s => s.length > 0);

  for (const stmt of statements) {
    await pool.query(stmt);
  }
}

/**
 * Cleanup helper - drop table if exists
 */
export async function dropTableIfExists(pool: pg.Pool, tableName: string): Promise<void> {
  await pool.query(`DROP TABLE IF EXISTS ${tableName} CASCADE`);
}
