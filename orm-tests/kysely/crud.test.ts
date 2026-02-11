import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { Kysely, PostgresDialect, sql } from 'kysely';
import pg from 'pg';
const { Pool } = pg;
import { getPgConfig } from '../shared/config.js';
import type { Database } from './types.js';

describe('Kysely - CRUD Operations', () => {
  let db: Kysely<Database>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());

    db = new Kysely<Database>({
      dialect: new PostgresDialect({ pool }),
    });

    await setupTables(pool);
  });

  afterAll(async () => {
    await cleanupTables(pool);
    await db.destroy();
  });

  beforeEach(async () => {
    await truncateTables(pool);
  });

  describe('Insert Operations', () => {
    it('should insert single record', async () => {
      const result = await db
        .insertInto('kysely_users')
        .values({
          email: 'alice@example.com',
          name: 'Alice',
          age: 30,
        })
        .returning(['id', 'email', 'name'])
        .executeTakeFirst();

      expect(result?.id).toBeDefined();
      expect(result?.email).toBe('alice@example.com');
    });

    it('should insert multiple records', async () => {
      const result = await db
        .insertInto('kysely_users')
        .values([
          { email: 'user1@example.com', name: 'User 1' },
          { email: 'user2@example.com', name: 'User 2' },
          { email: 'user3@example.com', name: 'User 3' },
        ])
        .returning(['id'])
        .execute();

      expect(result.length).toBe(3);
    });

    it('should insert with default values', async () => {
      const result = await db
        .insertInto('kysely_users')
        .values({
          email: 'defaults@example.com',
          name: 'Default User',
        })
        .returningAll()
        .executeTakeFirst();

      expect(result?.is_active).toBe(true);
    });

    it('should handle on conflict', async () => {
      await db
        .insertInto('kysely_users')
        .values({ email: 'conflict@example.com', name: 'Original' })
        .execute();

      await db
        .insertInto('kysely_users')
        .values({ email: 'conflict@example.com', name: 'Updated' })
        .onConflict((oc) =>
          oc.column('email').doUpdateSet({ name: 'Updated' })
        )
        .execute();

      const result = await db
        .selectFrom('kysely_users')
        .where('email', '=', 'conflict@example.com')
        .selectAll()
        .executeTakeFirst();

      expect(result?.name).toBe('Updated');
    });
  });

  describe('Select Operations', () => {
    beforeEach(async () => {
      await seedTestData(db);
    });

    it('should select all records', async () => {
      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .execute();

      expect(result.length).toBeGreaterThan(0);
    });

    it('should select specific columns', async () => {
      const result = await db
        .selectFrom('kysely_users')
        .select(['id', 'email', 'name'])
        .execute();

      expect(result[0].id).toBeDefined();
      expect(result[0].email).toBeDefined();
      expect((result[0] as Record<string, unknown>).age).toBeUndefined();
    });

    it('should filter with where', async () => {
      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('email', '=', 'alice@example.com')
        .executeTakeFirst();

      expect(result?.name).toBe('Alice');
    });

    it('should filter with comparison operators', async () => {
      const gtResult = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('age', '>', 25)
        .execute();

      const lteResult = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('age', '<=', 30)
        .execute();

      expect(gtResult.length).toBeGreaterThan(0);
      expect(lteResult.length).toBeGreaterThan(0);
    });

    it('should filter with AND conditions', async () => {
      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('is_active', '=', true)
        .where('age', '>', 25)
        .execute();

      expect(result.every(u => u.is_active && (u.age ?? 0) > 25)).toBe(true);
    });

    it('should filter with OR conditions', async () => {
      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where((eb) =>
          eb.or([
            eb('age', '<', 30),
            eb('name', '=', 'Alice'),
          ])
        )
        .execute();

      expect(result.length).toBeGreaterThan(0);
    });

    it('should filter with LIKE', async () => {
      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('email', 'like', '%@example.com')
        .execute();

      expect(result.length).toBeGreaterThan(0);
    });

    it('should filter with IN', async () => {
      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('email', 'in', ['alice@example.com', 'bob@example.com'])
        .execute();

      expect(result.length).toBeLessThanOrEqual(2);
    });

    it('should filter NULL values', async () => {
      await db
        .insertInto('kysely_users')
        .values({ email: 'nullage@example.com', name: 'Null Age' })
        .execute();

      const nullResult = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('age', 'is', null)
        .execute();

      expect(nullResult.some(u => u.email === 'nullage@example.com')).toBe(true);
    });

    it('should order results', async () => {
      const ascResult = await db
        .selectFrom('kysely_users')
        .selectAll()
        .orderBy('age', 'asc')
        .execute();

      const ages = ascResult.filter(u => u.age !== null).map(u => u.age!);
      for (let i = 1; i < ages.length; i++) {
        expect(ages[i]).toBeGreaterThanOrEqual(ages[i - 1]);
      }
    });

    it('should limit and offset results', async () => {
      const page1 = await db
        .selectFrom('kysely_users')
        .selectAll()
        .orderBy('id', 'asc')
        .limit(2)
        .offset(0)
        .execute();

      const page2 = await db
        .selectFrom('kysely_users')
        .selectAll()
        .orderBy('id', 'asc')
        .limit(2)
        .offset(2)
        .execute();

      expect(page1.length).toBeLessThanOrEqual(2);
      if (page2.length > 0) {
        expect(page1[0].id).not.toBe(page2[0].id);
      }
    });
  });

  describe('Update Operations', () => {
    beforeEach(async () => {
      await seedTestData(db);
    });

    it('should update single record', async () => {
      const result = await db
        .updateTable('kysely_users')
        .set({ name: 'Alice Updated', age: 31 })
        .where('email', '=', 'alice@example.com')
        .returning(['name', 'age'])
        .executeTakeFirst();

      expect(result?.name).toBe('Alice Updated');
      expect(result?.age).toBe(31);
    });

    it('should update multiple records', async () => {
      await db
        .updateTable('kysely_users')
        .set({ is_active: false })
        .where('age', '<', 30)
        .execute();

      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('age', '<', 30)
        .execute();

      expect(result.every(u => u.is_active === false)).toBe(true);
    });

    it('should update with expression', async () => {
      await db
        .insertInto('kysely_users')
        .values({ email: 'increment@example.com', name: 'Increment', age: 25 })
        .execute();

      await db
        .updateTable('kysely_users')
        .set((eb) => ({
          age: eb('age', '+', 1),
        }))
        .where('email', '=', 'increment@example.com')
        .execute();

      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('email', '=', 'increment@example.com')
        .executeTakeFirst();

      expect(result?.age).toBe(26);
    });
  });

  describe('Delete Operations', () => {
    beforeEach(async () => {
      await seedTestData(db);
    });

    it('should delete single record', async () => {
      await db
        .deleteFrom('kysely_users')
        .where('email', '=', 'alice@example.com')
        .execute();

      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('email', '=', 'alice@example.com')
        .executeTakeFirst();

      expect(result).toBeUndefined();
    });

    it('should delete with returning', async () => {
      const deleted = await db
        .deleteFrom('kysely_users')
        .where('email', '=', 'alice@example.com')
        .returning(['email', 'name'])
        .executeTakeFirst();

      expect(deleted?.email).toBe('alice@example.com');
    });

    it('should delete multiple records', async () => {
      await db
        .deleteFrom('kysely_users')
        .where('is_active', '=', false)
        .execute();

      const result = await db
        .selectFrom('kysely_users')
        .selectAll()
        .where('is_active', '=', false)
        .execute();

      expect(result.length).toBe(0);
    });
  });
});

describe('Kysely - Aggregations', () => {
  let db: Kysely<Database>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());

    db = new Kysely<Database>({
      dialect: new PostgresDialect({ pool }),
    });

    await setupTables(pool);
  });

  afterAll(async () => {
    await cleanupTables(pool);
    await db.destroy();
  });

  beforeEach(async () => {
    await truncateTables(pool);
    await seedTestData(db);
  });

  it('should count records', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .select((eb) => eb.fn.count<number>('id').as('count'))
      .executeTakeFirst();

    expect(parseInt(String(result?.count))).toBeGreaterThan(0);
  });

  it('should calculate sum', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .select((eb) => eb.fn.sum<number>('age').as('total'))
      .executeTakeFirst();

    expect(parseFloat(String(result?.total))).toBeGreaterThan(0);
  });

  it('should calculate average', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .select((eb) => eb.fn.avg<number>('age').as('average'))
      .executeTakeFirst();

    expect(parseFloat(String(result?.average))).toBeGreaterThan(0);
  });

  it('should find min and max', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .select((eb) => [
        eb.fn.min<number>('age').as('min_age'),
        eb.fn.max<number>('age').as('max_age'),
      ])
      .executeTakeFirst();

    expect(parseInt(String(result?.min_age))).toBeLessThanOrEqual(parseInt(String(result?.max_age)));
  });

  it('should group by', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .select((eb) => [
        'is_active',
        eb.fn.count<number>('id').as('count'),
      ])
      .groupBy('is_active')
      .execute();

    expect(result.length).toBeGreaterThan(0);
  });

  it('should use having clause', async () => {
    await db
      .insertInto('kysely_users')
      .values([
        { email: 'extra1@example.com', name: 'Extra 1', is_active: true },
        { email: 'extra2@example.com', name: 'Extra 2', is_active: true },
      ])
      .execute();

    const result = await db
      .selectFrom('kysely_users')
      .select((eb) => [
        'is_active',
        eb.fn.count<number>('id').as('count'),
      ])
      .groupBy('is_active')
      .having((eb) => eb.fn.count('id'), '>', 1)
      .execute();

    expect(result.length).toBeGreaterThan(0);
  });
});

describe('Kysely - Joins', () => {
  let db: Kysely<Database>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());

    db = new Kysely<Database>({
      dialect: new PostgresDialect({ pool }),
    });

    await setupTables(pool);
  });

  afterAll(async () => {
    await cleanupTables(pool);
    await db.destroy();
  });

  beforeEach(async () => {
    await truncateTables(pool);
    await seedRelationalData(db);
  });

  it('should perform inner join', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .innerJoin('kysely_posts', 'kysely_users.id', 'kysely_posts.author_id')
      .select(['kysely_users.name', 'kysely_posts.title'])
      .execute();

    expect(result.length).toBeGreaterThan(0);
  });

  it('should perform left join', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .leftJoin('kysely_posts', 'kysely_users.id', 'kysely_posts.author_id')
      .select(['kysely_users.name', 'kysely_posts.title'])
      .execute();

    expect(result.length).toBeGreaterThan(0);
  });

  it('should join multiple tables', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .innerJoin('kysely_posts', 'kysely_users.id', 'kysely_posts.author_id')
      .leftJoin('kysely_comments', 'kysely_posts.id', 'kysely_comments.post_id')
      .select([
        'kysely_users.name as user_name',
        'kysely_posts.title as post_title',
        'kysely_comments.text as comment_text',
      ])
      .execute();

    expect(result.length).toBeGreaterThan(0);
  });

  it('should join with aggregation', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .leftJoin('kysely_posts', 'kysely_users.id', 'kysely_posts.author_id')
      .select((eb) => [
        'kysely_users.name',
        eb.fn.count<number>('kysely_posts.id').as('post_count'),
      ])
      .groupBy(['kysely_users.id', 'kysely_users.name'])
      .execute();

    expect(result.length).toBeGreaterThan(0);
  });
});

describe('Kysely - Transactions', () => {
  let db: Kysely<Database>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());

    db = new Kysely<Database>({
      dialect: new PostgresDialect({ pool }),
    });

    await setupTables(pool);
  });

  afterAll(async () => {
    await cleanupTables(pool);
    await db.destroy();
  });

  beforeEach(async () => {
    await truncateTables(pool);
  });

  it('should commit transaction', async () => {
    await db.transaction().execute(async (trx) => {
      const user = await trx
        .insertInto('kysely_users')
        .values({ email: 'txn@example.com', name: 'Transaction User' })
        .returning(['id'])
        .executeTakeFirstOrThrow();

      await trx
        .insertInto('kysely_posts')
        .values({ title: 'Transaction Post', author_id: user.id })
        .execute();
    });

    const result = await db
      .selectFrom('kysely_users')
      .selectAll()
      .where('email', '=', 'txn@example.com')
      .executeTakeFirst();

    expect(result).toBeDefined();
  });

  it('should rollback transaction on error', async () => {
    try {
      await db.transaction().execute(async (trx) => {
        await trx
          .insertInto('kysely_users')
          .values({ email: 'rollback@example.com', name: 'Rollback User' })
          .execute();

        throw new Error('Intentional error');
      });
    } catch {
      // Expected
    }

    const result = await db
      .selectFrom('kysely_users')
      .selectAll()
      .where('email', '=', 'rollback@example.com')
      .executeTakeFirst();

    expect(result).toBeUndefined();
  });
});

describe('Kysely - Subqueries', () => {
  let db: Kysely<Database>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());

    db = new Kysely<Database>({
      dialect: new PostgresDialect({ pool }),
    });

    await setupTables(pool);
  });

  afterAll(async () => {
    await cleanupTables(pool);
    await db.destroy();
  });

  beforeEach(async () => {
    await truncateTables(pool);
    await seedTestData(db);
  });

  it('should use subquery in where', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .selectAll()
      .where('age', '>', (eb) =>
        eb.selectFrom('kysely_users')
          .select((eb2) => eb2.fn.avg<number>('age').as('avg'))
      )
      .execute();

    expect(result.length).toBeGreaterThanOrEqual(0);
  });

  it('should use subquery in select', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .select((eb) => [
        'name',
        'age',
        eb.selectFrom('kysely_users')
          .select((eb2) => eb2.fn.avg<number>('age').as('avg'))
          .as('avg_age'),
      ])
      .execute();

    expect(result.length).toBeGreaterThan(0);
    expect(result[0].avg_age).toBeDefined();
  });
});

describe('Kysely - Raw SQL', () => {
  let db: Kysely<Database>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());

    db = new Kysely<Database>({
      dialect: new PostgresDialect({ pool }),
    });

    await setupTables(pool);
  });

  afterAll(async () => {
    await cleanupTables(pool);
    await db.destroy();
  });

  beforeEach(async () => {
    await truncateTables(pool);
    await seedTestData(db);
  });

  it('should execute raw SQL', async () => {
    const result = await sql<{ email: string; name: string | null }>`
      SELECT email, name FROM kysely_users WHERE age > ${25}
    `.execute(db);

    expect(result.rows.length).toBeGreaterThan(0);
  });

  it('should use raw in select', async () => {
    const result = await db
      .selectFrom('kysely_users')
      .select([
        'name',
        sql<string>`CASE WHEN age < 30 THEN 'young' ELSE 'senior' END`.as('age_group'),
      ])
      .where('age', 'is not', null)
      .execute();

    expect(result.length).toBeGreaterThan(0);
    expect(['young', 'senior']).toContain(result[0].age_group);
  });
});

describe('Kysely - E-commerce Scenario', () => {
  let db: Kysely<Database>;
  let pool: pg.Pool;

  beforeAll(async () => {
    pool = new Pool(getPgConfig());

    db = new Kysely<Database>({
      dialect: new PostgresDialect({ pool }),
    });

    await setupTables(pool);
  });

  afterAll(async () => {
    await cleanupTables(pool);
    await db.destroy();
  });

  beforeEach(async () => {
    await truncateTables(pool);
  });

  it('should create order with items', async () => {
    const user = await db
      .insertInto('kysely_users')
      .values({ email: 'shopper@example.com', name: 'Shopper' })
      .returning(['id'])
      .executeTakeFirstOrThrow();

    const product1 = await db
      .insertInto('kysely_products')
      .values({ name: 'Widget', price: '29.99', stock: 100 })
      .returning(['id'])
      .executeTakeFirstOrThrow();

    const product2 = await db
      .insertInto('kysely_products')
      .values({ name: 'Gadget', price: '49.99', stock: 50 })
      .returning(['id'])
      .executeTakeFirstOrThrow();

    const order = await db
      .insertInto('kysely_orders')
      .values({ user_id: user.id, total: '129.97', status: 'pending' })
      .returning(['id'])
      .executeTakeFirstOrThrow();

    await db
      .insertInto('kysely_order_items')
      .values([
        { order_id: order.id, product_id: product1.id, quantity: 2, price: '29.99' },
        { order_id: order.id, product_id: product2.id, quantity: 1, price: '49.99' },
      ])
      .execute();

    const items = await db
      .selectFrom('kysely_order_items')
      .selectAll()
      .where('order_id', '=', order.id)
      .execute();

    expect(items.length).toBe(2);
  });

  it('should update order status', async () => {
    const user = await db
      .insertInto('kysely_users')
      .values({ email: 'status@example.com', name: 'Status User' })
      .returning(['id'])
      .executeTakeFirstOrThrow();

    const order = await db
      .insertInto('kysely_orders')
      .values({ user_id: user.id, total: '99.99' })
      .returningAll()
      .executeTakeFirstOrThrow();

    expect(order.status).toBe('pending');

    const processing = await db
      .updateTable('kysely_orders')
      .set({ status: 'processing' })
      .where('id', '=', order.id)
      .returning(['status'])
      .executeTakeFirstOrThrow();

    expect(processing.status).toBe('processing');

    const shipped = await db
      .updateTable('kysely_orders')
      .set({ status: 'shipped' })
      .where('id', '=', order.id)
      .returning(['status'])
      .executeTakeFirstOrThrow();

    expect(shipped.status).toBe('shipped');
  });

  it('should calculate order statistics', async () => {
    const user = await db
      .insertInto('kysely_users')
      .values({ email: 'stats@example.com', name: 'Stats User' })
      .returning(['id'])
      .executeTakeFirstOrThrow();

    await db
      .insertInto('kysely_orders')
      .values([
        { user_id: user.id, total: '100.00', status: 'delivered' },
        { user_id: user.id, total: '150.00', status: 'delivered' },
        { user_id: user.id, total: '50.00', status: 'pending' },
      ])
      .execute();

    const stats = await db
      .selectFrom('kysely_orders')
      .select((eb) => [
        eb.fn.count<number>('id').as('order_count'),
        eb.fn.sum<number>('total').as('total_amount'),
        eb.fn.avg<number>('total').as('avg_amount'),
      ])
      .where('user_id', '=', user.id)
      .executeTakeFirst();

    expect(parseInt(String(stats?.order_count))).toBe(3);
    expect(parseFloat(String(stats?.total_amount))).toBeCloseTo(300, 0);
  });
});

async function setupTables(pool: pg.Pool) {
  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_users (
      id SERIAL PRIMARY KEY,
      email VARCHAR(255) NOT NULL UNIQUE,
      name VARCHAR(100),
      age INTEGER,
      is_active BOOLEAN DEFAULT true,
      created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_profiles (
      id SERIAL PRIMARY KEY,
      bio TEXT,
      avatar VARCHAR(255),
      user_id INTEGER NOT NULL UNIQUE
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_posts (
      id SERIAL PRIMARY KEY,
      title VARCHAR(255) NOT NULL,
      content TEXT,
      published BOOLEAN DEFAULT false,
      author_id INTEGER NOT NULL,
      created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_comments (
      id SERIAL PRIMARY KEY,
      text TEXT NOT NULL,
      post_id INTEGER NOT NULL,
      created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_tags (
      id SERIAL PRIMARY KEY,
      name VARCHAR(50) NOT NULL UNIQUE
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_posts_tags (
      post_id INTEGER NOT NULL,
      tag_id INTEGER NOT NULL,
      PRIMARY KEY (post_id, tag_id)
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_products (
      id SERIAL PRIMARY KEY,
      name VARCHAR(255) NOT NULL,
      price DECIMAL(10, 2) NOT NULL,
      stock INTEGER DEFAULT 0
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_orders (
      id SERIAL PRIMARY KEY,
      user_id INTEGER NOT NULL,
      total DECIMAL(10, 2) NOT NULL,
      status VARCHAR(20) DEFAULT 'pending',
      created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )
  `);

  await pool.query(`
    CREATE TABLE IF NOT EXISTS kysely_order_items (
      id SERIAL PRIMARY KEY,
      order_id INTEGER NOT NULL,
      product_id INTEGER NOT NULL,
      quantity INTEGER NOT NULL,
      price DECIMAL(10, 2) NOT NULL
    )
  `);
}

async function cleanupTables(pool: pg.Pool) {
  await pool.query('DROP TABLE IF EXISTS kysely_order_items CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_orders CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_products CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_posts_tags CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_tags CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_comments CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_posts CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_profiles CASCADE');
  await pool.query('DROP TABLE IF EXISTS kysely_users CASCADE');
}

async function truncateTables(pool: pg.Pool) {
  await pool.query('DELETE FROM kysely_order_items');
  await pool.query('DELETE FROM kysely_orders');
  await pool.query('DELETE FROM kysely_products');
  await pool.query('DELETE FROM kysely_posts_tags');
  await pool.query('DELETE FROM kysely_tags');
  await pool.query('DELETE FROM kysely_comments');
  await pool.query('DELETE FROM kysely_posts');
  await pool.query('DELETE FROM kysely_profiles');
  await pool.query('DELETE FROM kysely_users');
}

async function seedTestData(db: Kysely<Database>) {
  await db
    .insertInto('kysely_users')
    .values([
      { email: 'alice@example.com', name: 'Alice', age: 30, is_active: true },
      { email: 'bob@example.com', name: 'Bob', age: 25, is_active: true },
      { email: 'charlie@example.com', name: 'Charlie', age: 35, is_active: false },
    ])
    .execute();
}

async function seedRelationalData(db: Kysely<Database>) {
  const alice = await db
    .insertInto('kysely_users')
    .values({ email: 'alice@example.com', name: 'Alice', age: 30 })
    .returning(['id'])
    .executeTakeFirstOrThrow();

  const bob = await db
    .insertInto('kysely_users')
    .values({ email: 'bob@example.com', name: 'Bob', age: 25 })
    .returning(['id'])
    .executeTakeFirstOrThrow();

  await db
    .insertInto('kysely_profiles')
    .values({ bio: 'Alice bio', user_id: alice.id })
    .execute();

  const post1 = await db
    .insertInto('kysely_posts')
    .values({ title: 'Alice Post 1', content: 'Content 1', published: true, author_id: alice.id })
    .returning(['id'])
    .executeTakeFirstOrThrow();

  await db
    .insertInto('kysely_posts')
    .values({ title: 'Bob Post 1', author_id: bob.id })
    .execute();

  await db
    .insertInto('kysely_comments')
    .values([
      { text: 'Comment 1', post_id: post1.id },
      { text: 'Comment 2', post_id: post1.id },
    ])
    .execute();
}
