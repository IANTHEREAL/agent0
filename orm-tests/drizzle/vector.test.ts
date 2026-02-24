import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { drizzle, NodePgDatabase } from 'drizzle-orm/node-postgres';
import { sql } from 'drizzle-orm';
import { createDrizzleClient } from './client.js';

describe('Drizzle Vector Operations [db9-server]', () => {
  let db: NodePgDatabase;

  beforeAll(async () => {
    const { db: drizzleDb } = createDrizzleClient();
    db = drizzleDb;

    // Create embeddings table with vector column
    await db.execute(sql`
      CREATE TABLE IF NOT EXISTS drizzle_embeddings (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100),
        embedding vector(3)
      )
    `);
  });

  afterAll(async () => {
    await db.execute(sql`DROP TABLE IF EXISTS drizzle_embeddings`);
  });

  beforeEach(async () => {
    await db.execute(sql`DELETE FROM drizzle_embeddings`);
  });

  describe('Vector CRUD', () => {
    it('should insert vector', async () => {
      await db.execute(
        sql`INSERT INTO drizzle_embeddings (name, embedding) VALUES ('doc1', '[1.0, 2.0, 3.0]')`
      );

      const result = await db.execute(
        sql`SELECT * FROM drizzle_embeddings WHERE name = 'doc1'`
      );

      expect(result.rows).toHaveLength(1);
      expect(result.rows[0].name).toBe('doc1');
      expect(result.rows[0].embedding).toBe('[1,2,3]');
    });

    it('should insert multiple vectors', async () => {
      await db.execute(sql`
        INSERT INTO drizzle_embeddings (name, embedding) VALUES
          ('doc1', '[1.0, 2.0, 3.0]'),
          ('doc2', '[4.0, 5.0, 6.0]'),
          ('doc3', '[1.0, 0.0, 0.0]')
      `);

      const result = await db.execute(
        sql`SELECT COUNT(*) as count FROM drizzle_embeddings`
      );

      expect(parseInt(result.rows[0].count, 10)).toBe(3);
    });

    it('should query vectors', async () => {
      await db.execute(
        sql`INSERT INTO drizzle_embeddings (name, embedding) VALUES ('test', '[7.0, 8.0, 9.0]')`
      );

      const result = await db.execute(
        sql`SELECT * FROM drizzle_embeddings WHERE name = 'test'`
      );

      expect(result.rows).toHaveLength(1);
      expect(result.rows[0].embedding).toBe('[7,8,9]');
    });

    it('should update vector', async () => {
      await db.execute(
        sql`INSERT INTO drizzle_embeddings (name, embedding) VALUES ('update_test', '[1.0, 2.0, 3.0]')`
      );

      await db.execute(
        sql`UPDATE drizzle_embeddings SET embedding = '[4.0, 5.0, 6.0]' WHERE name = 'update_test'`
      );

      const result = await db.execute(
        sql`SELECT * FROM drizzle_embeddings WHERE name = 'update_test'`
      );

      expect(result.rows[0].embedding).toBe('[4,5,6]');
    });

    it('should delete vector', async () => {
      await db.execute(
        sql`INSERT INTO drizzle_embeddings (name, embedding) VALUES ('delete_test', '[1.0, 2.0, 3.0]')`
      );

      await db.execute(
        sql`DELETE FROM drizzle_embeddings WHERE name = 'delete_test'`
      );

      const result = await db.execute(
        sql`SELECT * FROM drizzle_embeddings WHERE name = 'delete_test'`
      );

      expect(result.rows).toHaveLength(0);
    });
  });

  describe('Vector Distance Functions', () => {
    beforeEach(async () => {
      await db.execute(sql`
        INSERT INTO drizzle_embeddings (name, embedding) VALUES
          ('doc1', '[1.0, 2.0, 3.0]'),
          ('doc2', '[4.0, 5.0, 6.0]'),
          ('doc3', '[1.0, 0.0, 0.0]')
      `);
    });

    it('should calculate L2 distance', async () => {
      const result = await db.execute(sql`
        SELECT name, l2_distance(embedding, '[1.0, 2.0, 3.0]') as distance
        FROM drizzle_embeddings
        ORDER BY distance ASC
      `);

      expect(result.rows).toHaveLength(3);
      expect(result.rows[0].name).toBe('doc1');
      expect(parseFloat(result.rows[0].distance)).toBeCloseTo(0, 2);
    });

    it('should calculate cosine distance', async () => {
      const result = await db.execute(sql`
        SELECT name, cosine_distance(embedding, '[1.0, 1.0, 1.0]') as distance
        FROM drizzle_embeddings
        ORDER BY distance ASC
      `);

      expect(result.rows).toHaveLength(3);
      expect(result.rows[0].name).toBeDefined();
    });

    it('should calculate inner product', async () => {
      const result = await db.execute(sql`
        SELECT name, inner_product(embedding, '[1.0, 1.0, 1.0]') as product
        FROM drizzle_embeddings
        ORDER BY product ASC
      `);

      expect(result.rows).toHaveLength(3);
      expect(result.rows[0].name).toBeDefined();
    });

    it('should perform similarity search', async () => {
      const query = '[1.0, 2.0, 3.0]';
      const result = await db.execute(sql`
        SELECT name, cosine_distance(embedding, ${query}) as similarity
        FROM drizzle_embeddings
        ORDER BY similarity ASC
        LIMIT 2
      `);

      expect(result.rows).toHaveLength(2);
      expect(result.rows[0].name).toBe('doc1');
    });

    it('should order by distance for nearest neighbors', async () => {
      const result = await db.execute(sql`
        SELECT name, l2_distance(embedding, '[1.0, 0.5, 0.0]') as distance
        FROM drizzle_embeddings
        ORDER BY distance ASC
        LIMIT 1
      `);

      expect(result.rows).toHaveLength(1);
      expect(result.rows[0].name).toBe('doc3');
    });
  });

  describe('Vector Utility Functions', () => {
    beforeEach(async () => {
      await db.execute(
        sql`INSERT INTO drizzle_embeddings (name, embedding) VALUES ('test', '[3.0, 4.0, 0.0]')`
      );
    });

    it('should get vector dimensions', async () => {
      const result = await db.execute(
        sql`SELECT name, vector_dims(embedding) as dims FROM drizzle_embeddings`
      );

      expect(result.rows).toHaveLength(1);
      expect(result.rows[0].dims).toBe(3);
    });

    it('should calculate vector norm', async () => {
      const result = await db.execute(
        sql`SELECT name, vector_norm(embedding) as norm FROM drizzle_embeddings`
      );

      expect(result.rows).toHaveLength(1);
      // 3-4-5 triangle: sqrt(3^2 + 4^2) = 5
      expect(parseFloat(result.rows[0].norm)).toBeCloseTo(5.0, 2);
    });
  });

  describe('Vector Type Casting', () => {
    it('should cast string to vector', async () => {
      const result = await db.execute(
        sql`SELECT CAST('[1.0, 2.0, 3.0]' AS vector) as vec`
      );

      expect(result.rows).toHaveLength(1);
      expect(result.rows[0].vec).toBe('[1,2,3]');
    });
  });
});
