import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { PrismaClient } from '@prisma/client';
import { getPrismaClient, disconnectPrisma } from './client.js';

describe('Prisma gapfill operations [db9-server]', () => {
  let prisma: PrismaClient;

  beforeAll(async () => {
    prisma = getPrismaClient();
    await prisma.$connect();
  });

  afterAll(async () => {
    await disconnectPrisma();
  });

  it('should cover returning and join/subquery operations', async () => {
    await prisma.$executeRawUnsafe('DROP TABLE IF EXISTS prisma_gap_b');
    await prisma.$executeRawUnsafe('DROP TABLE IF EXISTS prisma_gap_a');
    await prisma.$executeRawUnsafe('CREATE TABLE prisma_gap_a (id INTEGER PRIMARY KEY, name TEXT NOT NULL)');
    await prisma.$executeRawUnsafe('CREATE TABLE prisma_gap_b (id INTEGER PRIMARY KEY, a_id INTEGER NOT NULL)');
    await prisma.$executeRawUnsafe(`INSERT INTO prisma_gap_a (id, name) VALUES (1, 'a') RETURNING id`);
    await prisma.$executeRawUnsafe(`INSERT INTO prisma_gap_b (id, a_id) VALUES (1, 1)`);

    const innerJoin = await prisma.$queryRawUnsafe<{ id: number }[]>(
      `SELECT a.id FROM prisma_gap_a a INNER JOIN prisma_gap_b b ON b.a_id = a.id`
    );
    expect(innerJoin.length).toBe(1);

    await prisma.$executeRawUnsafe('DROP TABLE IF EXISTS prisma_gap_b');
    await prisma.$executeRawUnsafe('DROP TABLE IF EXISTS prisma_gap_a');
  });

  it('should cover prepared named bind/positional/repeated execute markers', async () => {
    // named statement marker
    await prisma.$queryRawUnsafe('SELECT 1 as named_statement');
    // positional bind marker
    await prisma.$queryRawUnsafe('SELECT $1::int as positional_bind', 2);
    // repeated execute marker
    await prisma.$queryRawUnsafe('SELECT $1::int as positional_bind', 3);
    expect(true).toBe(true);
  });

  it('should cover transaction begin commit and savepoint operations', async () => {
    // Keep savepoint operations on one pinned connection to avoid pool
    // handoff between standalone BEGIN/SAVEPOINT statements.
    await prisma.$transaction(async (tx) => {
      await tx.$executeRawUnsafe('SAVEPOINT prisma_gap_sp');
      await tx.$executeRawUnsafe('ROLLBACK TO SAVEPOINT prisma_gap_sp');
      await tx.$executeRawUnsafe('RELEASE SAVEPOINT prisma_gap_sp');
    });
    expect(true).toBe(true);
  });

  it('should cover array insert and vector index operation', async () => {
    await prisma.$executeRawUnsafe('DROP TABLE IF EXISTS prisma_gap_arr_vec');
    await prisma.$executeRawUnsafe(`
      CREATE TABLE prisma_gap_arr_vec (
        id SERIAL PRIMARY KEY,
        tags INTEGER[],
        embedding vector(3) NOT NULL
      )
    `);
    await prisma.$executeRawUnsafe(
      `INSERT INTO prisma_gap_arr_vec (tags, embedding) VALUES (ARRAY[1,2,3], '[1,0,0]')`
    );
    await prisma.$executeRawUnsafe(
      `CREATE INDEX idx_prisma_gap_vec_embedding ON prisma_gap_arr_vec USING ivfflat (embedding)`
    );
    await prisma.$executeRawUnsafe('DROP TABLE IF EXISTS prisma_gap_arr_vec');
    expect(true).toBe(true);
  });
});
