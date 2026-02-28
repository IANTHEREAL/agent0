import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { QueryTypes, Sequelize } from 'sequelize';
import { createSequelize } from './connection.js';

describe('Sequelize Prepared Statement Compatibility [db9-server]', () => {
  let sequelize: Sequelize;

  beforeAll(async () => {
    sequelize = createSequelize();
    await sequelize.query(`
      CREATE TABLE IF NOT EXISTS sequelize_prepared_cases (
        id SERIAL PRIMARY KEY,
        name TEXT NOT NULL,
        score INT NOT NULL
      )
    `);
  });

  afterAll(async () => {
    await sequelize.query('DROP TABLE IF EXISTS sequelize_prepared_cases');
    await sequelize.close();
  });

  beforeEach(async () => {
    await sequelize.query('DELETE FROM sequelize_prepared_cases');
  });

  it('should execute prepared bind parameter query', async () => {
    await sequelize.query(
      'INSERT INTO sequelize_prepared_cases (name, score) VALUES ($1, $2)',
      { bind: ['prepared_row', 88] }
    );

    const rows = await sequelize.query<{ score: number }>(
      'SELECT score FROM sequelize_prepared_cases WHERE name = $1',
      { bind: ['prepared_row'], type: QueryTypes.SELECT }
    );

    expect(rows).toHaveLength(1);
    expect(Number(rows[0].score)).toBe(88);
  });
});

