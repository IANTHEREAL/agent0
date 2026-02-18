import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { PrismaClient } from '@prisma/client';
import { getPrismaClient } from './client.js';

describe('Prisma Vector Operations [pg-tikv]', () => {
  let prisma: PrismaClient;

  beforeAll(async () => {
    prisma = getPrismaClient();

    // Create embeddings table with vector column using raw SQL
    await prisma.$executeRawUnsafe(`
      CREATE TABLE IF NOT EXISTS prisma_embeddings (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100),
        embedding vector(3)
      )
    `);
  });

  afterAll(async () => {
    await prisma.$executeRawUnsafe('DROP TABLE IF EXISTS prisma_embeddings');
    await prisma.$disconnect();
  });

  beforeEach(async () => {
    await prisma.$executeRawUnsafe('DELETE FROM prisma_embeddings');
  });

  describe('Vector CRUD', () => {
    it('should insert vector', async () => {
      await prisma.$executeRawUnsafe(
        `INSERT INTO prisma_embeddings (name, embedding) VALUES ('doc1', '[1.0, 2.0, 3.0]')`
      );

      const result = await prisma.$queryRawUnsafe<any[]>(
        `SELECT * FROM prisma_embeddings WHERE name = 'doc1'`
      );

      expect(result).toHaveLength(1);
      expect(result[0].name).toBe('doc1');
      expect(result[0].embedding).toBe('[1,2,3]');
    });

    it('should insert multiple vectors', async () => {
      await prisma.$executeRawUnsafe(`
        INSERT INTO prisma_embeddings (name, embedding) VALUES
          ('doc1', '[1.0, 2.0, 3.0]'),
          ('doc2', '[4.0, 5.0, 6.0]'),
          ('doc3', '[1.0, 0.0, 0.0]')
      `);

      const count = await prisma.$queryRawUnsafe<{ count: bigint }[]>(
        'SELECT COUNT(*) as count FROM prisma_embeddings'
      );

      expect(Number(count[0].count)).toBe(3);
    });

    it('should query vectors', async () => {
      await prisma.$executeRawUnsafe(
        `INSERT INTO prisma_embeddings (name, embedding) VALUES ('test', '[7.0, 8.0, 9.0]')`
      );

      const result = await prisma.$queryRawUnsafe<any[]>(
        `SELECT * FROM prisma_embeddings WHERE name = 'test'`
      );

      expect(result).toHaveLength(1);
      expect(result[0].embedding).toBe('[7,8,9]');
    });

    it('should update vector', async () => {
      await prisma.$executeRawUnsafe(
        `INSERT INTO prisma_embeddings (name, embedding) VALUES ('update_test', '[1.0, 2.0, 3.0]')`
      );

      await prisma.$executeRawUnsafe(
        `UPDATE prisma_embeddings SET embedding = '[4.0, 5.0, 6.0]' WHERE name = 'update_test'`
      );

      const result = await prisma.$queryRawUnsafe<any[]>(
        `SELECT * FROM prisma_embeddings WHERE name = 'update_test'`
      );

      expect(result[0].embedding).toBe('[4,5,6]');
    });

    it('should delete vector', async () => {
      await prisma.$executeRawUnsafe(
        `INSERT INTO prisma_embeddings (name, embedding) VALUES ('delete_test', '[1.0, 2.0, 3.0]')`
      );

      await prisma.$executeRawUnsafe(
        `DELETE FROM prisma_embeddings WHERE name = 'delete_test'`
      );

      const result = await prisma.$queryRawUnsafe<any[]>(
        `SELECT * FROM prisma_embeddings WHERE name = 'delete_test'`
      );

      expect(result).toHaveLength(0);
    });
  });

  describe('Vector Distance Functions', () => {
    beforeEach(async () => {
      await prisma.$executeRawUnsafe(`
        INSERT INTO prisma_embeddings (name, embedding) VALUES
          ('doc1', '[1.0, 2.0, 3.0]'),
          ('doc2', '[4.0, 5.0, 6.0]'),
          ('doc3', '[1.0, 0.0, 0.0]')
      `);
    });

    it('should calculate L2 distance', async () => {
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT name, l2_distance(embedding, '[1.0, 2.0, 3.0]') as distance
         FROM prisma_embeddings
         ORDER BY distance ASC`
      );

      expect(results).toHaveLength(3);
      expect(results[0].name).toBe('doc1');
      expect(parseFloat(results[0].distance)).toBeCloseTo(0, 2);
    });

    it('should calculate cosine distance', async () => {
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT name, cosine_distance(embedding, '[1.0, 1.0, 1.0]') as distance
         FROM prisma_embeddings
         ORDER BY distance ASC`
      );

      expect(results).toHaveLength(3);
      expect(results[0].name).toBeDefined();
    });

    it('should calculate inner product', async () => {
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT name, inner_product(embedding, '[1.0, 1.0, 1.0]') as product
         FROM prisma_embeddings
         ORDER BY product ASC`
      );

      expect(results).toHaveLength(3);
      expect(results[0].name).toBeDefined();
    });

    it('should perform similarity search', async () => {
      const query = '[1.0, 2.0, 3.0]';
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT name, cosine_distance(embedding, '${query}') as similarity
         FROM prisma_embeddings
         ORDER BY similarity ASC
         LIMIT 2`
      );

      expect(results).toHaveLength(2);
      expect(results[0].name).toBe('doc1');
    });

    it('should order by distance for nearest neighbors', async () => {
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT name, l2_distance(embedding, '[1.0, 0.5, 0.0]') as distance
         FROM prisma_embeddings
         ORDER BY distance ASC
         LIMIT 1`
      );

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe('doc3');
    });
  });

  describe('Vector Utility Functions', () => {
    beforeEach(async () => {
      await prisma.$executeRawUnsafe(
        `INSERT INTO prisma_embeddings (name, embedding) VALUES ('test', '[3.0, 4.0, 0.0]')`
      );
    });

    it('should get vector dimensions', async () => {
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT name, vector_dims(embedding) as dims FROM prisma_embeddings`
      );

      expect(results).toHaveLength(1);
      expect(results[0].dims).toBe(3);
    });

    it('should calculate vector norm', async () => {
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT name, vector_norm(embedding) as norm FROM prisma_embeddings`
      );

      expect(results).toHaveLength(1);
      // 3-4-5 triangle: sqrt(3^2 + 4^2) = 5
      expect(parseFloat(results[0].norm)).toBeCloseTo(5.0, 2);
    });
  });

  describe('Vector Type Casting', () => {
    it('should cast string to vector', async () => {
      const results = await prisma.$queryRawUnsafe<any[]>(
        `SELECT CAST('[1.0, 2.0, 3.0]' AS vector) as vec`
      );

      expect(results).toHaveLength(1);
      expect(results[0].vec).toBe('[1,2,3]');
    });
  });
});
