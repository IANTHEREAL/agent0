import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { Sequelize, DataTypes, Model, Optional } from 'sequelize';
import { createSequelize } from './connection.js';

interface EmbeddingAttributes {
  id: number;
  name: string;
  embedding: string; // Vector stored as string
}

interface EmbeddingCreationAttributes
  extends Optional<EmbeddingAttributes, 'id'> {}

class Embedding
  extends Model<EmbeddingAttributes, EmbeddingCreationAttributes>
  implements EmbeddingAttributes
{
  declare id: number;
  declare name: string;
  declare embedding: string;
}

describe('Sequelize Vector Operations [db9-server]', () => {
  let sequelize: Sequelize;

  beforeAll(async () => {
    sequelize = createSequelize();

    Embedding.init(
      {
        id: {
          type: DataTypes.INTEGER,
          autoIncrement: true,
          primaryKey: true,
        },
        name: {
          type: DataTypes.STRING(100),
          allowNull: false,
        },
        embedding: {
          type: DataTypes.STRING, // Store as string, table created with raw SQL
          allowNull: false,
        },
      },
      {
        sequelize,
        tableName: 'sequelize_embeddings',
        timestamps: false,
      }
    );

    // Create table with vector column using raw SQL
    await sequelize.query(`
      CREATE TABLE IF NOT EXISTS sequelize_embeddings (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100) NOT NULL,
        embedding vector(3) NOT NULL
      )
    `);
  });

  afterAll(async () => {
    await Embedding.drop();
    await sequelize.close();
  });

  beforeEach(async () => {
    await Embedding.destroy({ where: {}, force: true });
  });

  describe('Vector CRUD', () => {
    it('should insert vector', async () => {
      const embedding = await Embedding.create({
        name: 'doc1',
        embedding: '[1.0, 2.0, 3.0]',
      });

      expect(embedding.id).toBeGreaterThan(0);
      expect(embedding.name).toBe('doc1');
      expect(embedding.embedding).toBe('[1,2,3]');
    });

    it('should insert multiple vectors', async () => {
      const embeddings = await Embedding.bulkCreate([
        { name: 'doc1', embedding: '[1.0, 2.0, 3.0]' },
        { name: 'doc2', embedding: '[4.0, 5.0, 6.0]' },
        { name: 'doc3', embedding: '[1.0, 0.0, 0.0]' },
      ]);

      expect(embeddings).toHaveLength(3);
      embeddings.forEach((emb) => {
        expect(emb.id).toBeGreaterThan(0);
      });
    });

    it('should query vectors', async () => {
      await Embedding.create({
        name: 'test',
        embedding: '[7.0, 8.0, 9.0]',
      });

      const found = await Embedding.findOne({
        where: { name: 'test' },
      });

      expect(found).toBeDefined();
      expect(found?.embedding).toBe('[7,8,9]');
    });

    it('should update vector', async () => {
      const embedding = await Embedding.create({
        name: 'update_test',
        embedding: '[1.0, 2.0, 3.0]',
      });

      await Embedding.update(
        { embedding: '[4.0, 5.0, 6.0]' },
        { where: { id: embedding.id } }
      );

      const updated = await Embedding.findByPk(embedding.id);
      expect(updated?.embedding).toBe('[4,5,6]');
    });
  });

  describe('Vector Distance Functions', () => {
    beforeEach(async () => {
      await Embedding.bulkCreate([
        { name: 'doc1', embedding: '[1.0, 2.0, 3.0]' },
        { name: 'doc2', embedding: '[4.0, 5.0, 6.0]' },
        { name: 'doc3', embedding: '[1.0, 0.0, 0.0]' },
      ]);
    });

    it('should calculate L2 distance', async () => {
      const results = await sequelize.query(
        `SELECT name, l2_distance(embedding, '[1.0, 2.0, 3.0]') as distance
         FROM sequelize_embeddings
         ORDER BY distance ASC`,
        { type: 'SELECT' }
      );

      expect(results).toHaveLength(3);
      expect((results[0] as any).name).toBe('doc1');
      expect(parseFloat((results[0] as any).distance)).toBeCloseTo(0, 2);
    });

    it('should calculate cosine distance', async () => {
      const results = await sequelize.query(
        `SELECT name, cosine_distance(embedding, '[1.0, 1.0, 1.0]') as distance
         FROM sequelize_embeddings
         ORDER BY distance ASC`,
        { type: 'SELECT' }
      );

      expect(results).toHaveLength(3);
      expect((results[0] as any).name).toBeDefined();
    });

    it('should calculate inner product', async () => {
      const results = await sequelize.query(
        `SELECT name, inner_product(embedding, '[1.0, 1.0, 1.0]') as product
         FROM sequelize_embeddings
         ORDER BY product ASC`,
        { type: 'SELECT' }
      );

      expect(results).toHaveLength(3);
      expect((results[0] as any).name).toBeDefined();
    });

    it('should perform similarity search', async () => {
      const query = '[1.0, 2.0, 3.0]';
      const results = await sequelize.query(
        `SELECT name, cosine_distance(embedding, '${query}') as similarity
         FROM sequelize_embeddings
         ORDER BY similarity ASC
         LIMIT 2`,
        { type: 'SELECT' }
      );

      expect(results).toHaveLength(2);
      expect((results[0] as any).name).toBe('doc1');
    });
  });

  describe('Vector Utility Functions', () => {
    beforeEach(async () => {
      await Embedding.create({
        name: 'test',
        embedding: '[3.0, 4.0, 0.0]',
      });
    });

    it('should get vector dimensions', async () => {
      const results = await sequelize.query(
        `SELECT name, vector_dims(embedding) as dims
         FROM sequelize_embeddings`,
        { type: 'SELECT' }
      );

      expect(results).toHaveLength(1);
      expect((results[0] as any).dims).toBe(3);
    });

    it('should calculate vector norm', async () => {
      const results = await sequelize.query(
        `SELECT name, vector_norm(embedding) as norm
         FROM sequelize_embeddings`,
        { type: 'SELECT' }
      );

      expect(results).toHaveLength(1);
      // 3-4-5 triangle: sqrt(3^2 + 4^2) = 5
      expect(parseFloat((results[0] as any).norm)).toBeCloseTo(5.0, 2);
    });
  });

  describe('Vector Type Casting', () => {
    it('should cast string to vector', async () => {
      const results = await sequelize.query(
        `SELECT CAST('[1.0, 2.0, 3.0]' AS vector) as vec`,
        { type: 'SELECT' }
      );

      expect(results).toHaveLength(1);
      expect((results[0] as any).vec).toBe('[1,2,3]');
    });
  });
});
