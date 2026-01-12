import { Kysely, PostgresDialect } from 'kysely';
import pg from 'pg';
const { Pool } = pg;
import { getPgConfig } from '../shared/config.js';
import type { Database } from './types.js';

let kyselyInstance: Kysely<Database> | null = null;
let poolInstance: pg.Pool | null = null;

/**
 * Create a new Kysely instance with PostgreSQL dialect
 */
export function createKyselyClient(): Kysely<Database> {
  if (kyselyInstance) {
    return kyselyInstance;
  }

  poolInstance = new Pool(getPgConfig());

  kyselyInstance = new Kysely<Database>({
    dialect: new PostgresDialect({ pool: poolInstance }),
  });

  return kyselyInstance;
}

/**
 * Get the underlying pg.Pool for raw queries
 */
export function getPool(): pg.Pool {
  if (!poolInstance) {
    throw new Error('Pool not initialized. Call createKyselyClient() first.');
  }
  return poolInstance;
}

/**
 * Destroy the Kysely instance and close the pool
 */
export async function destroyKyselyClient(): Promise<void> {
  if (kyselyInstance) {
    await kyselyInstance.destroy();
    kyselyInstance = null;
  }
  if (poolInstance) {
    await poolInstance.end();
    poolInstance = null;
  }
}
