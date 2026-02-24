import { describe, it, expect, beforeAll, afterAll, beforeEach } from 'vitest';
import { DataSource } from 'typeorm';
import { createDataSource } from './datasource.js';

describe('TypeORM SQL Features [db9-server]', () => {
  let dataSource: DataSource;

  beforeAll(async () => {
    dataSource = createDataSource({ synchronize: false });
    await dataSource.initialize();

    await dataSource.query(`DROP TABLE IF EXISTS sql_orders CASCADE`);
    await dataSource.query(`DROP TABLE IF EXISTS sql_products CASCADE`);
    await dataSource.query(`DROP TABLE IF EXISTS sql_categories CASCADE`);

    await dataSource.query(`
      CREATE TABLE sql_categories (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100) NOT NULL,
        tags TEXT[]
      )
    `);

    await dataSource.query(`
      CREATE TABLE sql_products (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100) NOT NULL,
        category_id INTEGER REFERENCES sql_categories(id),
        price DECIMAL(10,2) NOT NULL,
        created_at TIMESTAMPTZ DEFAULT NOW(),
        attributes TEXT[]
      )
    `);

    await dataSource.query(`
      CREATE TABLE sql_orders (
        id SERIAL PRIMARY KEY,
        product_id INTEGER REFERENCES sql_products(id),
        quantity INTEGER NOT NULL,
        order_date TIMESTAMPTZ DEFAULT NOW(),
        status VARCHAR(20) DEFAULT 'pending',
        notes TEXT
      )
    `);
  });

  afterAll(async () => {
    if (dataSource?.isInitialized) {
      await dataSource.query(`DROP TABLE IF EXISTS sql_orders CASCADE`);
      await dataSource.query(`DROP TABLE IF EXISTS sql_products CASCADE`);
      await dataSource.query(`DROP TABLE IF EXISTS sql_categories CASCADE`);
      await dataSource.destroy();
    }
  });

  beforeEach(async () => {
    await dataSource.query(`DELETE FROM sql_orders`);
    await dataSource.query(`DELETE FROM sql_products`);
    await dataSource.query(`DELETE FROM sql_categories`);

    await dataSource.query(`
      INSERT INTO sql_categories (id, name, tags) VALUES
      (1, 'Electronics', ARRAY['tech', 'gadgets']),
      (2, 'Clothing', ARRAY['fashion', 'apparel']),
      (3, 'Books', ARRAY['reading', 'education'])
    `);

    await dataSource.query(`
      INSERT INTO sql_products (id, name, category_id, price, attributes) VALUES
      (1, 'Laptop', 1, 999.99, ARRAY['portable', 'powerful']),
      (2, 'Phone', 1, 699.99, ARRAY['mobile', 'smart']),
      (3, 'T-Shirt', 2, 29.99, ARRAY['cotton', 'casual']),
      (4, 'Jeans', 2, 59.99, ARRAY['denim', 'classic']),
      (5, 'Novel', 3, 14.99, ARRAY['fiction', 'paperback']),
      (6, 'Textbook', 3, 89.99, ARRAY['education', 'hardcover'])
    `);

    await dataSource.query(`
      INSERT INTO sql_orders (id, product_id, quantity, status, notes) VALUES
      (1, 1, 2, 'completed', 'Express shipping'),
      (2, 1, 1, 'pending', NULL),
      (3, 2, 3, 'completed', 'Gift wrapped'),
      (4, 3, 5, 'shipped', NULL),
      (5, 4, 2, 'completed', 'Standard delivery'),
      (6, 5, 10, 'pending', 'Bulk order'),
      (7, 6, 1, 'cancelled', 'Wrong item')
    `);
  });

  describe('DISTINCT ON', () => {
    it('should support DISTINCT ON with single column', async () => {
      const result = await dataSource.query(`
        SELECT DISTINCT ON (category_id) id, name, category_id, price
        FROM sql_products
        ORDER BY category_id, price DESC
      `);
      expect(result).toHaveLength(3);
      expect(result.map((r: any) => r.category_id).sort()).toEqual([1, 2, 3]);
    });

    it('should support DISTINCT ON with ORDER BY', async () => {
      const result = await dataSource.query(`
        SELECT DISTINCT ON (status) id, product_id, status, quantity
        FROM sql_orders
        ORDER BY status, quantity DESC
      `);
      expect(result.length).toBeGreaterThanOrEqual(3);
    });
  });

  describe('CASE expressions', () => {
    it('should support simple CASE', async () => {
      const result = await dataSource.query(`
        SELECT name, price,
          CASE
            WHEN price > 500 THEN 'expensive'
            WHEN price > 50 THEN 'moderate'
            ELSE 'cheap'
          END as price_category
        FROM sql_products
        ORDER BY price DESC
      `);
      expect(result[0].price_category).toBe('expensive');
      expect(result[result.length - 1].price_category).toBe('cheap');
    });

    it('should support searched CASE', async () => {
      const result = await dataSource.query(`
        SELECT status,
          CASE status
            WHEN 'completed' THEN 'Done'
            WHEN 'pending' THEN 'Waiting'
            WHEN 'shipped' THEN 'In Transit'
            ELSE 'Other'
          END as status_label
        FROM sql_orders
      `);
      expect(result.length).toBe(7);
      const completed = result.find((r: any) => r.status === 'completed');
      expect(completed.status_label).toBe('Done');
    });

    it('should support CASE in ORDER BY', async () => {
      const result = await dataSource.query(`
        SELECT id, status FROM sql_orders
        ORDER BY CASE status
          WHEN 'pending' THEN 1
          WHEN 'shipped' THEN 2
          WHEN 'completed' THEN 3
          ELSE 4
        END
      `);
      expect(result[0].status).toBe('pending');
    });
  });

  describe('COALESCE and NULLIF', () => {
    it('should support COALESCE', async () => {
      const result = await dataSource.query(`
        SELECT id, COALESCE(notes, 'No notes') as notes_display
        FROM sql_orders
        ORDER BY id
      `);
      expect(result[1].notes_display).toBe('No notes');
      expect(result[0].notes_display).toBe('Express shipping');
    });

    it('should support COALESCE with multiple arguments', async () => {
      const result = await dataSource.query(`
        SELECT COALESCE(NULL, NULL, 'default') as val
      `);
      expect(result[0].val).toBe('default');
    });

    it('should support NULLIF', async () => {
      const result = await dataSource.query(`
        SELECT id, NULLIF(status, 'cancelled') as active_status
        FROM sql_orders
        WHERE id = 7
      `);
      expect(result[0].active_status).toBeNull();
    });
  });

  describe('UNION, INTERSECT, EXCEPT', () => {
    it('should support UNION', async () => {
      const result = await dataSource.query(`
        SELECT name FROM sql_products WHERE category_id = 1
        UNION
        SELECT name FROM sql_products WHERE price > 50
        ORDER BY name
      `);
      expect(result.length).toBeGreaterThanOrEqual(2);
    });

    it('should support UNION ALL', async () => {
      const result = await dataSource.query(`
        SELECT status FROM sql_orders WHERE status = 'completed'
        UNION ALL
        SELECT status FROM sql_orders WHERE status = 'completed'
      `);
      expect(result.length).toBe(6);
    });

    it('should support INTERSECT', async () => {
      const result = await dataSource.query(`
        SELECT category_id FROM sql_products WHERE price > 50
        INTERSECT
        SELECT category_id FROM sql_products WHERE price < 200
      `);
      expect(result.length).toBeGreaterThanOrEqual(1);
    });

    it('should support EXCEPT', async () => {
      const result = await dataSource.query(`
        SELECT DISTINCT category_id FROM sql_products
        EXCEPT
        SELECT DISTINCT category_id FROM sql_products WHERE price > 500
      `);
      expect(result.length).toBeGreaterThanOrEqual(1);
    });
  });

  describe('GROUP BY with HAVING', () => {
    it('should support HAVING with COUNT', async () => {
      const result = await dataSource.query(`
        SELECT category_id, COUNT(*) as product_count
        FROM sql_products
        GROUP BY category_id
        HAVING COUNT(*) >= 2
        ORDER BY category_id
      `);
      expect(result.length).toBe(3);
      expect(Number(result[0].product_count)).toBeGreaterThanOrEqual(2);
    });

    it('should support HAVING with SUM', async () => {
      const result = await dataSource.query(`
        SELECT product_id, SUM(quantity) as total_qty
        FROM sql_orders
        GROUP BY product_id
        HAVING SUM(quantity) > 2
        ORDER BY total_qty DESC
      `);
      expect(result.length).toBeGreaterThanOrEqual(1);
    });

    it('should support HAVING with AVG', async () => {
      const result = await dataSource.query(`
        SELECT category_id, AVG(price) as avg_price
        FROM sql_products
        GROUP BY category_id
        HAVING AVG(price) > 40
        ORDER BY avg_price DESC
      `);
      expect(result.length).toBeGreaterThanOrEqual(1);
    });
  });

  describe('Aggregate functions', () => {
    it('should support STRING_AGG', async () => {
      const result = await dataSource.query(`
        SELECT category_id, STRING_AGG(name, ', ' ORDER BY name) as product_names
        FROM sql_products
        GROUP BY category_id
        ORDER BY category_id
      `);
      expect(result).toHaveLength(3);
      expect(result[0].product_names).toContain(',');
    });

    it('should support STRING_AGG with DISTINCT', async () => {
      const result = await dataSource.query(`
        SELECT STRING_AGG(DISTINCT status, ', ' ORDER BY status) as all_statuses
        FROM sql_orders
      `);
      expect(result[0].all_statuses).toContain('completed');
    });

    it('should support COUNT with FILTER', async () => {
      const result = await dataSource.query(`
        SELECT 
          COUNT(*) as total,
          COUNT(*) FILTER (WHERE status = 'completed') as completed_count
        FROM sql_orders
      `);
      expect(Number(result[0].total)).toBe(7);
      expect(Number(result[0].completed_count)).toBe(3);
    });

    it('should support SUM with FILTER', async () => {
      const result = await dataSource.query(`
        SELECT 
          SUM(quantity) as total_qty,
          SUM(quantity) FILTER (WHERE status = 'completed') as completed_qty
        FROM sql_orders
      `);
      expect(Number(result[0].total_qty)).toBeGreaterThan(Number(result[0].completed_qty));
    });
  });

  describe('Date/Time functions', () => {
    it('should support NOW()', async () => {
      const result = await dataSource.query(`SELECT NOW() as current_time`);
      expect(result[0].current_time).toBeDefined();
    });

    it('should support CURRENT_DATE', async () => {
      const result = await dataSource.query(`SELECT CURRENT_DATE as today`);
      expect(result[0].today).toBeDefined();
    });

    it('should support EXTRACT', async () => {
      const result = await dataSource.query(`
        SELECT EXTRACT(YEAR FROM NOW()) as year,
               EXTRACT(MONTH FROM NOW()) as month,
               EXTRACT(DAY FROM NOW()) as day
      `);
      expect(Number(result[0].year)).toBeGreaterThan(2020);
    });

    it('should support DATE_TRUNC', async () => {
      const result = await dataSource.query(`
        SELECT DATE_TRUNC('month', NOW()) as month_start
      `);
      expect(result[0].month_start).toBeDefined();
    });

    it('should support interval arithmetic', async () => {
      const result = await dataSource.query(`
        SELECT NOW() - INTERVAL '1 day' as yesterday,
               NOW() + INTERVAL '1 hour' as next_hour
      `);
      expect(result[0].yesterday).toBeDefined();
      expect(result[0].next_hour).toBeDefined();
    });
  });

  describe('ARRAY operations', () => {
    it('should support array contains ANY', async () => {
      const result = await dataSource.query(`
        SELECT name, tags FROM sql_categories
        WHERE 'tech' = ANY(tags)
      `);
      expect(result).toHaveLength(1);
      expect(result[0].name).toBe('Electronics');
    });

    it('should support array contains ALL', async () => {
      const result = await dataSource.query(`
        SELECT name FROM sql_categories
        WHERE tags @> ARRAY['tech', 'gadgets']
      `);
      expect(result).toHaveLength(1);
    });

    it('should support ARRAY_LENGTH', async () => {
      const result = await dataSource.query(`
        SELECT name, ARRAY_LENGTH(tags, 1) as tag_count
        FROM sql_categories
        ORDER BY name
      `);
      expect(Number(result[0].tag_count)).toBe(2);
    });

    it.skip('should support UNNEST', async () => {
      const result = await dataSource.query(`
        SELECT UNNEST(tags) as tag
        FROM sql_categories
        WHERE name = 'Electronics'
      `);
      expect(result).toHaveLength(2);
    });

    it('should support array concatenation', async () => {
      const result = await dataSource.query(`
        SELECT ARRAY['a', 'b'] || ARRAY['c', 'd'] as combined
      `);
      expect(result[0].combined).toContain('a');
      expect(result[0].combined).toContain('d');
    });
  });

  describe('Conditional expressions', () => {
    it('should support GREATEST', async () => {
      const result = await dataSource.query(`
        SELECT GREATEST(10, 20, 5, 15) as max_val
      `);
      expect(Number(result[0].max_val)).toBe(20);
    });

    it('should support LEAST', async () => {
      const result = await dataSource.query(`
        SELECT LEAST(10, 20, 5, 15) as min_val
      `);
      expect(Number(result[0].min_val)).toBe(5);
    });

    it('should support BETWEEN', async () => {
      const result = await dataSource.query(`
        SELECT name, price FROM sql_products
        WHERE price BETWEEN 50 AND 100
        ORDER BY price
      `);
      expect(result.length).toBeGreaterThanOrEqual(1);
      result.forEach((r: any) => {
        expect(Number(r.price)).toBeGreaterThanOrEqual(50);
        expect(Number(r.price)).toBeLessThanOrEqual(100);
      });
    });

    it('should support IN with subquery', async () => {
      const result = await dataSource.query(`
        SELECT name FROM sql_products
        WHERE category_id IN (
          SELECT id FROM sql_categories WHERE name LIKE '%ics'
        )
      `);
      expect(result.length).toBeGreaterThanOrEqual(1);
    });
  });

  describe('String operations', () => {
    it('should support LIKE with wildcards', async () => {
      const result = await dataSource.query(`
        SELECT name FROM sql_products
        WHERE name LIKE '%book%'
      `);
      expect(result).toHaveLength(1);
    });

    it('should support ILIKE (case insensitive)', async () => {
      const result = await dataSource.query(`
        SELECT name FROM sql_products
        WHERE name ILIKE '%LAPTOP%'
      `);
      expect(result).toHaveLength(1);
    });

    it('should support string concatenation with ||', async () => {
      const result = await dataSource.query(`
        SELECT name || ' - $' || price::text as display
        FROM sql_products
        WHERE id = 1
      `);
      expect(result[0].display).toContain('Laptop');
      expect(result[0].display).toContain('$');
    });

    it('should support POSITION', async () => {
      const result = await dataSource.query(`
        SELECT POSITION('top' IN 'Laptop') as pos
      `);
      expect(Number(result[0].pos)).toBe(4);
    });
  });
});
