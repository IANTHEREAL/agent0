import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { DataSource, Repository } from 'typeorm';
import { createDataSource } from './datasource.js';
import { Embedding } from './entities/Embedding.js';

describe('TypeORM Vector Operations [pg-tikv]', () => {
  let dataSource: DataSource;
  let embeddingRepo: Repository<Embedding>;

  beforeAll(async () => {
    dataSource = createDataSource();
    await dataSource.initialize();
    embeddingRepo = dataSource.getRepository(Embedding);

    // Create table with vector column using raw query
    await dataSource.query(`
      CREATE TABLE IF NOT EXISTS typeorm_embeddings (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100),
        embedding vector(3)
      )
    `);
  });

  afterAll(async () => {
    await dataSource.query('DROP TABLE IF EXISTS typeorm_embeddings');
    await dataSource.destroy();
  });

  beforeEach(async () => {
    await dataSource.query('DELETE FROM typeorm_embeddings');
  });

  describe('Vector CRUD', () => {
    it('should insert vector', async () => {
      const embedding = embeddingRepo.create({
        name: 'doc1',
        embedding: '[1.0, 2.0, 3.0]',
      });

      const saved = await embeddingRepo.save(embedding);

      expect(saved.id).toBeGreaterThan(0);
      expect(saved.name).toBe('doc1');

      // Query back to verify database formatting
      const retrieved = await embeddingRepo.findOneBy({ id: saved.id });
      expect(retrieved?.embedding).toBe('[1,2,3]');
    });

    it('should insert multiple vectors', async () => {
      const embeddings = embeddingRepo.create([
        { name: 'doc1', embedding: '[1.0, 2.0, 3.0]' },
        { name: 'doc2', embedding: '[4.0, 5.0, 6.0]' },
        { name: 'doc3', embedding: '[1.0, 0.0, 0.0]' },
      ]);

      const saved = await embeddingRepo.save(embeddings);

      expect(saved).toHaveLength(3);
      saved.forEach((emb) => {
        expect(emb.id).toBeGreaterThan(0);
      });
    });

    it('should query vectors', async () => {
      await embeddingRepo.save({
        name: 'test',
        embedding: '[7.0, 8.0, 9.0]',
      });

      const found = await embeddingRepo.findOneBy({ name: 'test' });

      expect(found).toBeDefined();
      expect(found?.embedding).toBe('[7,8,9]');
    });

    it('should update vector', async () => {
      const embedding = await embeddingRepo.save({
        name: 'update_test',
        embedding: '[1.0, 2.0, 3.0]',
      });

      await embeddingRepo.update(
        { id: embedding.id },
        { embedding: '[4.0, 5.0, 6.0]' }
      );

      const updated = await embeddingRepo.findOneBy({ id: embedding.id });
      expect(updated?.embedding).toBe('[4,5,6]');
    });

    it('should delete vector', async () => {
      const embedding = await embeddingRepo.save({
        name: 'delete_test',
        embedding: '[1.0, 2.0, 3.0]',
      });

      await embeddingRepo.delete({ id: embedding.id });

      const found = await embeddingRepo.findOneBy({ id: embedding.id });
      expect(found).toBeNull();
    });
  });

  describe('Vector Distance Functions', () => {
    beforeEach(async () => {
      await embeddingRepo.save([
        { name: 'doc1', embedding: '[1.0, 2.0, 3.0]' },
        { name: 'doc2', embedding: '[4.0, 5.0, 6.0]' },
        { name: 'doc3', embedding: '[1.0, 0.0, 0.0]' },
      ]);
    });

    it('should calculate L2 distance', async () => {
      const results = await dataSource.query(
        `SELECT name, l2_distance(embedding, '[1.0, 2.0, 3.0]') as distance
         FROM typeorm_embeddings
         ORDER BY distance ASC`
      );

      expect(results).toHaveLength(3);
      expect(results[0].name).toBe('doc1');
      expect(parseFloat(results[0].distance)).toBeCloseTo(0, 2);
    });

    it('should calculate cosine distance', async () => {
      const results = await dataSource.query(
        `SELECT name, cosine_distance(embedding, '[1.0, 1.0, 1.0]') as distance
         FROM typeorm_embeddings
         ORDER BY distance ASC`
      );

      expect(results).toHaveLength(3);
      expect(results[0].name).toBeDefined();
    });

    it('should calculate inner product', async () => {
      const results = await dataSource.query(
        `SELECT name, inner_product(embedding, '[1.0, 1.0, 1.0]') as product
         FROM typeorm_embeddings
         ORDER BY product ASC`
      );

      expect(results).toHaveLength(3);
      expect(results[0].name).toBeDefined();
    });

    it('should perform similarity search', async () => {
      const query = '[1.0, 2.0, 3.0]';
      const results = await dataSource.query(
        `SELECT name, cosine_distance(embedding, '${query}') as similarity
         FROM typeorm_embeddings
         ORDER BY similarity ASC
         LIMIT 2`
      );

      expect(results).toHaveLength(2);
      expect(results[0].name).toBe('doc1');
    });

    it('should order by distance for nearest neighbors', async () => {
      const results = await dataSource.query(
        `SELECT name, l2_distance(embedding, '[1.0, 0.5, 0.0]') as distance
         FROM typeorm_embeddings
         ORDER BY distance ASC
         LIMIT 1`
      );

      expect(results).toHaveLength(1);
      expect(results[0].name).toBe('doc3');
    });
  });

  describe('Vector Utility Functions', () => {
    beforeEach(async () => {
      await embeddingRepo.save({
        name: 'test',
        embedding: '[3.0, 4.0, 0.0]',
      });
    });

    it('should get vector dimensions', async () => {
      const results = await dataSource.query(
        `SELECT name, vector_dims(embedding) as dims
         FROM typeorm_embeddings`
      );

      expect(results).toHaveLength(1);
      expect(results[0].dims).toBe(3);
    });

    it('should calculate vector norm', async () => {
      const results = await dataSource.query(
        `SELECT name, vector_norm(embedding) as norm
         FROM typeorm_embeddings`
      );

      expect(results).toHaveLength(1);
      // 3-4-5 triangle: sqrt(3^2 + 4^2) = 5
      expect(parseFloat(results[0].norm)).toBeCloseTo(5.0, 2);
    });
  });

  describe('Vector Type Casting', () => {
    it('should cast string to vector', async () => {
      const results = await dataSource.query(
        `SELECT CAST('[1.0, 2.0, 3.0]' AS vector) as vec`
      );

      expect(results).toHaveLength(1);
      expect(results[0].vec).toBe('[1,2,3]');
    });
  });
});
