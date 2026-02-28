import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { PrismaClient } from '@prisma/client';
import { getPrismaClient, disconnectPrisma } from './client.js';

describe('Prisma DDL lifecycle operations [db9-server]', () => {
  let prisma: PrismaClient;

  beforeAll(async () => {
    prisma = getPrismaClient();
    await prisma.$connect();
  });

  afterAll(async () => {
    await disconnectPrisma();
  });

  it('should run create schema, create table, alter table, create index, drop index, drop table, drop schema (migrate style)', async () => {
    await prisma.$executeRawUnsafe('DROP SCHEMA IF EXISTS prisma_ddl_ops CASCADE');
    await prisma.$executeRawUnsafe('CREATE SCHEMA prisma_ddl_ops');

    await prisma.$executeRawUnsafe(`
      CREATE TABLE prisma_ddl_ops.users (
        id SERIAL PRIMARY KEY,
        email TEXT NOT NULL
      )
    `);

    await prisma.$executeRawUnsafe('ALTER TABLE prisma_ddl_ops.users ADD COLUMN name TEXT');
    await prisma.$executeRawUnsafe('CREATE INDEX idx_prisma_ddl_email ON prisma_ddl_ops.users(email)');
    await prisma.$executeRawUnsafe('DROP INDEX prisma_ddl_ops.idx_prisma_ddl_email');

    await prisma.$executeRawUnsafe('DROP TABLE prisma_ddl_ops.users');
    await prisma.$executeRawUnsafe('DROP SCHEMA prisma_ddl_ops');

    expect(true).toBe(true);
  });
});
