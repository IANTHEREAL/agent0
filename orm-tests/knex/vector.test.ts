import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { Knex } from 'knex';
import { createKnexClient } from './client.js';

describe('Knex Vector Operations [db9-server]', () => {
  let db: Knex;

  beforeAll(async () => {
    db = createKnexClient();

    // Create embeddings table with vector column using raw SQL
    await db.raw(`
      CREATE TABLE IF NOT EXISTS knex_embeddings (
        id SERIAL PRIMARY KEY,
        name VARCHAR(255),
        embedding vector(3)
      )
    `);
  });

  afterAll(async () => {
    await db.schema.dropTableIfExists('knex_embeddings');
    await db.destroy();
  });

  beforeEach(async () => {
    await db('knex_embeddings').del();
  });

  describe('Vector CRUD', () => {
    it('should insert vectors', async () => {
      const [result] = await db('knex_embeddings')
        .insert({
          name: 'doc1',
          embedding: '[1.0, 2.0, 3.0]',
        })
        .returning('*');

      expect(result.id).toBeGreaterThan(0);
      expect(result.name).toBe('doc1');
      expect(result.embedding).toBe('[1,2,3]');
    });

    it('should insert multiple vectors', async () => {
      await db('knex_embeddings').insert([
        { name: 'doc1', embedding: '[1.0, 2.0, 3.0]' },
        { name: 'doc2', embedding: '[4.0, 5.0, 6.0]' },
        { name: 'doc3', embedding: '[1.0, 0.0, 0.0]' },
      ]);

      const count = await db('knex_embeddings').count('* as count').first();
      expect(parseInt(count!.count as string, 10)).toBe(3);
    });

    it('should query vectors', async () => {
      await db('knex_embeddings').insert({
        name: 'test',
        embedding: '[7.0, 8.0, 9.0]',
      });

      const result = await db('knex_embeddings')
        .where({ name: 'test' })
        .first();

      expect(result).toBeDefined();
      expect(result?.embedding).toBe('[7,8,9]');
    });
  });

  describe('Vector Distance Functions', () => {
    beforeEach(async () => {
      await db('knex_embeddings').insert([
        { name: 'doc1', embedding: '[1.0, 2.0, 3.0]' },
        { name: 'doc2', embedding: '[4.0, 5.0, 6.0]' },
        { name: 'doc3', embedding: '[1.0, 0.0, 0.0]' },
      ]);
    });

    it('should calculate L2 distance', async () => {
      const results = await db('knex_embeddings')
        .select('name')
        .select(db.raw("l2_distance(embedding, '[1.0, 2.0, 3.0]') as distance"))
        .orderBy('distance', 'asc');

      expect(results).toHaveLength(3);
      expect(results[0].name).toBe('doc1');
      expect(parseFloat(results[0].distance)).toBeCloseTo(0, 2);
    });

    it('should calculate cosine distance', async () => {
      const results = await db('knex_embeddings')
        .select('name')
        .select(db.raw("cosine_distance(embedding, '[1.0, 1.0, 1.0]') as distance"))
        .orderBy('distance', 'asc');

      expect(results).toHaveLength(3);
      // Vectors more aligned with [1,1,1] should have smaller cosine distance
      expect(results[0]).toBeDefined();
    });

    it('should calculate inner product', async () => {
      const results = await db('knex_embeddings')
        .select('name')
        .select(db.raw("inner_product(embedding, '[1.0, 1.0, 1.0]') as product"))
        .orderBy('product', 'asc');

      expect(results).toHaveLength(3);
      expect(results[0]).toBeDefined();
    });

    it('should perform similarity search', async () => {
      const query = '[1.0, 2.0, 3.0]';
      const results = await db('knex_embeddings')
        .select('name')
        .select(db.raw(`cosine_distance(embedding, '${query}') as similarity`))
        .orderBy('similarity', 'asc')
        .limit(2);

      expect(results).toHaveLength(2);
      expect(results[0].name).toBe('doc1'); // Exact match should be first
    });
  });

  describe('Vector Utility Functions', () => {
    beforeEach(async () => {
      await db('knex_embeddings').insert({
        name: 'test',
        embedding: '[3.0, 4.0, 0.0]',
      });
    });

    it('should get vector dimensions', async () => {
      const result = await db('knex_embeddings')
        .select('name')
        .select(db.raw('vector_dims(embedding) as dims'))
        .first();

      expect(result?.dims).toBe(3);
    });

    it('should calculate vector norm', async () => {
      const result = await db('knex_embeddings')
        .select('name')
        .select(db.raw('vector_norm(embedding) as norm'))
        .first();

      // 3-4-5 triangle: sqrt(3^2 + 4^2) = 5
      expect(parseFloat(result?.norm)).toBeCloseTo(5.0, 2);
    });
  });

  describe('Vector Type Casting', () => {
    it('should cast string to vector', async () => {
      const result = await db.raw("SELECT CAST('[1.0, 2.0, 3.0]' AS vector) as vec");

      expect(result.rows).toHaveLength(1);
      expect(result.rows[0].vec).toBe('[1,2,3]');
    });
  });
});
