import { describe, it, expect, beforeAll, afterAll, beforeEach, afterEach } from 'vitest';
import pg from 'pg';
const { Pool } = pg;
import { createPgPool, dropTableIfExists } from './client.js';
import { generateTableName } from '../shared/test-utils.js';

describe('pg client - Basic Operations', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('pg_basic');
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  describe('DDL Operations', () => {
    it('should create table with various column types', async () => {
      await pool.query(`
        CREATE TABLE ${tableName} (
          id SERIAL PRIMARY KEY,
          name VARCHAR(100) NOT NULL,
          email TEXT,
          age INTEGER,
          balance DECIMAL(10, 2),
          is_active BOOLEAN DEFAULT true,
          created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )
      `);

      const result = await pool.query(`
        INSERT INTO ${tableName} (name, email, age, balance)
        VALUES ('Test', 'test@example.com', 25, 100.50)
        RETURNING *
      `);

      expect(result.rows[0].name).toBe('Test');
      expect(result.rows[0].is_active).toBe(true);
    });

    it('should alter table - add column', async () => {
      await pool.query(`CREATE TABLE ${tableName} (id SERIAL PRIMARY KEY)`);
      await pool.query(`ALTER TABLE ${tableName} ADD COLUMN name VARCHAR(100)`);

      await pool.query(`INSERT INTO ${tableName} (name) VALUES ('Test')`);
      const result = await pool.query(`SELECT name FROM ${tableName}`);

      expect(result.rows[0].name).toBe('Test');
    });

    it('should drop table', async () => {
      await pool.query(`CREATE TABLE ${tableName} (id SERIAL PRIMARY KEY)`);
      await pool.query(`DROP TABLE ${tableName}`);

      await expect(
        pool.query(`SELECT * FROM ${tableName}`)
      ).rejects.toThrow();
    });
  });

  describe('DML Operations', () => {
    beforeEach(async () => {
      await pool.query(`
        CREATE TABLE ${tableName} (
          id SERIAL PRIMARY KEY,
          name VARCHAR(100) NOT NULL,
          email TEXT,
          age INTEGER
        )
      `);
    });

    it('should insert single row', async () => {
      const result = await pool.query(`
        INSERT INTO ${tableName} (name, email, age) 
        VALUES ($1, $2, $3) 
        RETURNING *
      `, ['Alice', 'alice@example.com', 30]);

      expect(result.rows[0].name).toBe('Alice');
      expect(result.rows[0].email).toBe('alice@example.com');
      expect(result.rows[0].age).toBe(30);
    });

    it('should insert multiple rows', async () => {
      await pool.query(`
        INSERT INTO ${tableName} (name, email, age) VALUES
        ('Alice', 'alice@example.com', 30),
        ('Bob', 'bob@example.com', 25),
        ('Charlie', 'charlie@example.com', 35)
      `);

      const result = await pool.query(`SELECT COUNT(*) as count FROM ${tableName}`);
      expect(parseInt(result.rows[0].count)).toBe(3);
    });

    it('should update rows', async () => {
      await pool.query(`
        INSERT INTO ${tableName} (name, email, age) VALUES ($1, $2, $3)
      `, ['Alice', 'alice@example.com', 30]);

      await pool.query(`
        UPDATE ${tableName} SET age = $1 WHERE name = $2
      `, [31, 'Alice']);

      const result = await pool.query(`SELECT age FROM ${tableName} WHERE name = $1`, ['Alice']);
      expect(result.rows[0].age).toBe(31);
    });

    it('should delete rows', async () => {
      await pool.query(`
        INSERT INTO ${tableName} (name, email, age) VALUES ($1, $2, $3)
      `, ['Alice', 'alice@example.com', 30]);

      await pool.query(`DELETE FROM ${tableName} WHERE name = $1`, ['Alice']);

      const result = await pool.query(`SELECT COUNT(*) as count FROM ${tableName}`);
      expect(parseInt(result.rows[0].count)).toBe(0);
    });

    it('should support RETURNING clause', async () => {
      const insertResult = await pool.query(`
        INSERT INTO ${tableName} (name, email, age) 
        VALUES ($1, $2, $3) 
        RETURNING id, name
      `, ['Alice', 'alice@example.com', 30]);

      expect(insertResult.rows[0].id).toBeDefined();
      expect(insertResult.rows[0].name).toBe('Alice');

      const updateResult = await pool.query(`
        UPDATE ${tableName} SET age = $1 WHERE name = $2 RETURNING *
      `, [31, 'Alice']);

      expect(updateResult.rows[0].age).toBe(31);
    });
  });

  describe('Query Operations', () => {
    beforeEach(async () => {
      await pool.query(`
        CREATE TABLE ${tableName} (
          id SERIAL PRIMARY KEY,
          name VARCHAR(100),
          department VARCHAR(50),
          salary INTEGER
        )
      `);

      await pool.query(`
        INSERT INTO ${tableName} (name, department, salary) VALUES
        ('Alice', 'Engineering', 100000),
        ('Bob', 'Engineering', 90000),
        ('Charlie', 'Sales', 80000),
        ('Diana', 'Sales', 85000),
        ('Eve', 'Marketing', 75000)
      `);
    });

    it('should select with WHERE clause', async () => {
      const result = await pool.query(`
        SELECT * FROM ${tableName} WHERE department = $1
      `, ['Engineering']);

      expect(result.rows.length).toBe(2);
    });

    it('should select with ORDER BY', async () => {
      const result = await pool.query(`
        SELECT name, salary FROM ${tableName} ORDER BY salary DESC
      `);

      expect(result.rows[0].name).toBe('Alice');
      expect(result.rows[result.rows.length - 1].name).toBe('Eve');
    });

    it('should select with LIMIT and OFFSET', async () => {
      const result = await pool.query(`
        SELECT name FROM ${tableName} ORDER BY name LIMIT 2 OFFSET 1
      `);

      expect(result.rows.length).toBe(2);
      expect(result.rows[0].name).toBe('Bob');
    });

    it('should support aggregate functions', async () => {
      const result = await pool.query(`
        SELECT 
          department,
          COUNT(*) as count,
          SUM(salary) as total_salary,
          AVG(salary) as avg_salary,
          MIN(salary) as min_salary,
          MAX(salary) as max_salary
        FROM ${tableName}
        GROUP BY department
        ORDER BY department
      `);

      expect(result.rows.length).toBe(3);
      const engineering = result.rows.find((r: { department: string }) => r.department === 'Engineering');
      expect(parseInt(engineering.count)).toBe(2);
    });

    it('should support HAVING clause', async () => {
      const result = await pool.query(`
        SELECT department, COUNT(*) as count
        FROM ${tableName}
        GROUP BY department
        HAVING COUNT(*) > 1
      `);

      expect(result.rows.length).toBe(2);
    });

    it('should support LIKE pattern matching', async () => {
      const result = await pool.query(`
        SELECT name FROM ${tableName} WHERE name LIKE $1
      `, ['%li%']);

      expect(result.rows.length).toBe(2);
    });

    it('should support IN clause', async () => {
      const result = await pool.query(`
        SELECT name FROM ${tableName} WHERE department IN ('Engineering', 'Marketing')
      `);

      expect(result.rows.length).toBe(3);
    });

    it('should support BETWEEN', async () => {
      const result = await pool.query(`
        SELECT name FROM ${tableName} WHERE salary BETWEEN 80000 AND 90000
      `);

      expect(result.rows.length).toBe(3);
    });

    it('should support IS NULL / IS NOT NULL', async () => {
      await pool.query(`INSERT INTO ${tableName} (name) VALUES ('NoSalary')`);

      const nullResult = await pool.query(`
        SELECT name FROM ${tableName} WHERE salary IS NULL
      `);
      expect(nullResult.rows.length).toBe(1);

      const notNullResult = await pool.query(`
        SELECT name FROM ${tableName} WHERE salary IS NOT NULL
      `);
      expect(notNullResult.rows.length).toBe(5);
    });
  });
});

describe('pg client - Transactions', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('pg_txn');
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100),
        balance INTEGER DEFAULT 0
      )
    `);
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('should commit transaction', async () => {
    const client = await pool.connect();
    try {
      await client.query('BEGIN');
      await client.query(`INSERT INTO ${tableName} (name, balance) VALUES ($1, $2)`, ['Alice', 1000]);
      await client.query(`INSERT INTO ${tableName} (name, balance) VALUES ($1, $2)`, ['Bob', 500]);
      await client.query('COMMIT');
    } finally {
      client.release();
    }

    const result = await pool.query(`SELECT COUNT(*) as count FROM ${tableName}`);
    expect(parseInt(result.rows[0].count)).toBe(2);
  });

  it('should rollback transaction', async () => {
    const client = await pool.connect();
    try {
      await client.query('BEGIN');
      await client.query(`INSERT INTO ${tableName} (name, balance) VALUES ($1, $2)`, ['Alice', 1000]);
      await client.query('ROLLBACK');
    } finally {
      client.release();
    }

    const result = await pool.query(`SELECT COUNT(*) as count FROM ${tableName}`);
    expect(parseInt(result.rows[0].count)).toBe(0);
  });

  it('should handle transaction with error', async () => {
    await pool.query(`INSERT INTO ${tableName} (name, balance) VALUES ($1, $2)`, ['Alice', 1000]);

    const client = await pool.connect();
    try {
      await client.query('BEGIN');
      await client.query(`UPDATE ${tableName} SET balance = balance - 100 WHERE name = $1`, ['Alice']);

      await expect(
        client.query(`INSERT INTO nonexistent_table_xyz (col) VALUES (1)`)
      ).rejects.toThrow();

      await client.query('ROLLBACK');
    } finally {
      client.release();
    }

    const result = await pool.query(`SELECT balance FROM ${tableName} WHERE name = $1`, ['Alice']);
    expect(result.rows[0].balance).toBe(1000);
  });

  it('should isolate concurrent transactions', async () => {
    await pool.query(`INSERT INTO ${tableName} (name, balance) VALUES ($1, $2)`, ['Alice', 1000]);

    const client1 = await pool.connect();
    const client2 = await pool.connect();

    try {
      await client1.query('BEGIN');
      await client1.query(`UPDATE ${tableName} SET balance = balance - 100 WHERE name = $1`, ['Alice']);

      const result = await client2.query(`SELECT balance FROM ${tableName} WHERE name = $1`, ['Alice']);
      expect(result.rows[0].balance).toBe(1000);

      await client1.query('COMMIT');

      const result2 = await client2.query(`SELECT balance FROM ${tableName} WHERE name = $1`, ['Alice']);
      expect(result2.rows[0].balance).toBe(900);
    } finally {
      client1.release();
      client2.release();
    }
  });
});

describe('pg client - Joins', () => {
  let pool: pg.Pool;
  let usersTable: string;
  let ordersTable: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    usersTable = generateTableName('users');
    ordersTable = generateTableName('orders');

    await pool.query(`
      CREATE TABLE ${usersTable} (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100)
      )
    `);

    await pool.query(`
      CREATE TABLE ${ordersTable} (
        id SERIAL PRIMARY KEY,
        user_id INTEGER,
        amount DECIMAL(10, 2),
        status VARCHAR(20)
      )
    `);

    await pool.query(`
      INSERT INTO ${usersTable} (id, name) VALUES
      (1, 'Alice'),
      (2, 'Bob'),
      (3, 'Charlie')
    `);

    await pool.query(`
      INSERT INTO ${ordersTable} (user_id, amount, status) VALUES
      (1, 100.00, 'completed'),
      (1, 200.00, 'pending'),
      (2, 150.00, 'completed')
    `);
  });

  afterEach(async () => {
    await dropTableIfExists(pool, ordersTable);
    await dropTableIfExists(pool, usersTable);
  });

  it('should perform INNER JOIN', async () => {
    const result = await pool.query(`
      SELECT u.name, o.amount
      FROM ${usersTable} u
      INNER JOIN ${ordersTable} o ON u.id = o.user_id
      ORDER BY u.name, o.amount
    `);

    expect(result.rows.length).toBe(3);
  });

  it('should perform LEFT JOIN', async () => {
    const result = await pool.query(`
      SELECT u.name, o.amount
      FROM ${usersTable} u
      LEFT JOIN ${ordersTable} o ON u.id = o.user_id
      ORDER BY u.name
    `);

    expect(result.rows.length).toBe(4);
    const charlie = result.rows.find((r: { name: string }) => r.name === 'Charlie');
    expect(charlie.amount).toBeNull();
  });

  it('should perform JOIN with aggregation', async () => {
    const result = await pool.query(`
      SELECT u.name, COUNT(o.id) as order_count, COALESCE(SUM(o.amount), 0) as total
      FROM ${usersTable} u
      LEFT JOIN ${ordersTable} o ON u.id = o.user_id
      GROUP BY u.id, u.name
      ORDER BY u.name
    `);

    expect(result.rows.length).toBe(3);
    const alice = result.rows.find((r: { name: string }) => r.name === 'Alice');
    expect(parseInt(alice.order_count)).toBe(2);
  });
});

describe('pg client - Subqueries', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('employees');
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100),
        department VARCHAR(50),
        salary INTEGER
      )
    `);

    await pool.query(`
      INSERT INTO ${tableName} (name, department, salary) VALUES
      ('Alice', 'Engineering', 100000),
      ('Bob', 'Engineering', 90000),
      ('Charlie', 'Sales', 80000),
      ('Diana', 'Sales', 85000)
    `);
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('should support scalar subquery', async () => {
    const result = await pool.query(`
      SELECT name, salary,
        (SELECT AVG(salary) FROM ${tableName}) as avg_salary
      FROM ${tableName}
      ORDER BY name
    `);

    expect(result.rows.length).toBe(4);
    expect(parseFloat(result.rows[0].avg_salary)).toBeCloseTo(88750, 0);
  });

  it('should support subquery in WHERE', async () => {
    const result = await pool.query(`
      SELECT name, salary
      FROM ${tableName}
      WHERE salary > (SELECT AVG(salary) FROM ${tableName})
      ORDER BY salary DESC
    `);

    expect(result.rows.length).toBe(2);
    expect(result.rows[0].name).toBe('Alice');
  });

  it('should support EXISTS subquery', async () => {
    const highEarners = generateTableName('high_earners');
    await pool.query(`CREATE TABLE ${highEarners} (employee_id INTEGER)`);
    await pool.query(`INSERT INTO ${highEarners} (employee_id) VALUES (1)`);

    const result = await pool.query(`
      SELECT name FROM ${tableName} e
      WHERE EXISTS (SELECT 1 FROM ${highEarners} h WHERE h.employee_id = e.id)
    `);

    expect(result.rows.length).toBe(1);
    expect(result.rows[0].name).toBe('Alice');

    await dropTableIfExists(pool, highEarners);
  });

  it('should support IN subquery', async () => {
    const result = await pool.query(`
      SELECT name FROM ${tableName}
      WHERE department IN (
        SELECT department FROM ${tableName}
        GROUP BY department
        HAVING AVG(salary) > 85000
      )
    `);

    expect(result.rows.length).toBe(2);
  });
});

describe('pg client - Window Functions', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('sales');
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        region VARCHAR(50),
        product VARCHAR(50),
        amount INTEGER
      )
    `);

    await pool.query(`
      INSERT INTO ${tableName} (region, product, amount) VALUES
      ('North', 'Widget', 100),
      ('North', 'Gadget', 150),
      ('South', 'Widget', 200),
      ('South', 'Gadget', 120),
      ('East', 'Widget', 180)
    `);
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('should support ROW_NUMBER()', async () => {
    const result = await pool.query(`
      SELECT region, product, amount,
        ROW_NUMBER() OVER (ORDER BY amount DESC) as rank
      FROM ${tableName}
    `);

    expect(result.rows.length).toBe(5);
    const top = result.rows.find((r: { rank: string | number }) => parseInt(String(r.rank)) === 1);
    expect(top.amount).toBe(200);
  });

  it('should support RANK() with PARTITION BY', async () => {
    const result = await pool.query(`
      SELECT region, product, amount,
        RANK() OVER (PARTITION BY region ORDER BY amount DESC) as rank_in_region
      FROM ${tableName}
    `);

    expect(result.rows.length).toBe(5);
  });

  it('should support SUM() OVER with running total', async () => {
    const result = await pool.query(`
      SELECT region, amount,
        SUM(amount) OVER (ORDER BY amount) as running_total
      FROM ${tableName}
      ORDER BY amount
    `);

    expect(result.rows.length).toBe(5);
    expect(result.rows[result.rows.length - 1].running_total).toBe(750);
  });

  it('should support partition aggregates', async () => {
    const result = await pool.query(`
      SELECT region, amount,
        SUM(amount) OVER (PARTITION BY region) as region_total
      FROM ${tableName}
      ORDER BY region, amount
    `);

    expect(result.rows.length).toBe(5);
    const eastRow = result.rows.find((r: { region: string }) => r.region === 'East');
    expect(eastRow.region_total).toBe(180);
  });
});

describe('pg client - CTEs', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('employees');
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100),
        manager_id INTEGER,
        salary INTEGER
      )
    `);

    await pool.query(`
      INSERT INTO ${tableName} (id, name, manager_id, salary) VALUES
      (1, 'CEO', NULL, 200000),
      (2, 'VP Engineering', 1, 150000),
      (3, 'VP Sales', 1, 150000),
      (4, 'Senior Engineer', 2, 120000),
      (5, 'Junior Engineer', 4, 80000)
    `);
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('should support simple CTE', async () => {
    const result = await pool.query(`
      WITH high_earners AS (
        SELECT * FROM ${tableName} WHERE salary > 100000
      )
      SELECT name FROM high_earners ORDER BY name
    `);

    expect(result.rows.length).toBe(4);
  });

  it('should support multiple CTEs', async () => {
    const result = await pool.query(`
      WITH 
        managers AS (
          SELECT DISTINCT manager_id FROM ${tableName} WHERE manager_id IS NOT NULL
        ),
        manager_details AS (
          SELECT e.* FROM ${tableName} e
          JOIN managers m ON e.id = m.manager_id
        )
      SELECT name FROM manager_details ORDER BY name
    `);

    expect(result.rows.length).toBeGreaterThan(0);
  });

  it('should support recursive CTE', async () => {
    const result = await pool.query(`
      WITH RECURSIVE org_tree AS (
        SELECT id, name, manager_id, 1 as level
        FROM ${tableName}
        WHERE manager_id IS NULL
        
        UNION ALL
        
        SELECT e.id, e.name, e.manager_id, t.level + 1
        FROM ${tableName} e
        JOIN org_tree t ON e.manager_id = t.id
      )
      SELECT name, level FROM org_tree ORDER BY level, name
    `);

    expect(result.rows.length).toBe(5);
    expect(result.rows[0].name).toBe('CEO');
    expect(result.rows[0].level).toBe(1);
  });
});

describe('pg client - Data Types', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('types_test');
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('should handle numeric types', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        small_int SMALLINT,
        regular_int INTEGER,
        big_int BIGINT,
        decimal_val DECIMAL(10, 2),
        real_val REAL,
        double_val DOUBLE PRECISION
      )
    `);

    await pool.query(`
      INSERT INTO ${tableName} (small_int, regular_int, big_int, decimal_val, real_val, double_val)
      VALUES ($1, $2, $3, $4, $5, $6)
    `, [32767, 2147483647, '9223372036854775807', 12345.67, 3.14159, 2.718281828459045]);

    const result = await pool.query(`SELECT * FROM ${tableName}`);
    expect(result.rows[0].small_int).toBe(32767);
    expect(result.rows[0].regular_int).toBe(2147483647);
    expect(parseFloat(result.rows[0].decimal_val)).toBeCloseTo(12345.67);
  });

  it('should handle string types', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        char_col CHAR(10),
        varchar_col VARCHAR(100),
        text_col TEXT
      )
    `);

    const longText = 'A'.repeat(1000);
    await pool.query(`
      INSERT INTO ${tableName} (char_col, varchar_col, text_col)
      VALUES ($1, $2, $3)
    `, ['hello', 'world', longText]);

    const result = await pool.query(`SELECT * FROM ${tableName}`);
    expect(result.rows[0].varchar_col).toBe('world');
    expect(result.rows[0].text_col.length).toBe(1000);
  });

  it('should handle boolean type', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        is_active BOOLEAN
      )
    `);

    await pool.query(`INSERT INTO ${tableName} (is_active) VALUES (true), (false), (NULL)`);

    const result = await pool.query(`SELECT is_active FROM ${tableName} ORDER BY id`);
    expect(result.rows[0].is_active).toBe(true);
    expect(result.rows[1].is_active).toBe(false);
    expect(result.rows[2].is_active).toBeNull();
  });

  it('should handle date/time types', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        date_col DATE,
        time_col TIME,
        timestamp_col TIMESTAMP
      )
    `);

    await pool.query(`
      INSERT INTO ${tableName} (date_col, time_col, timestamp_col)
      VALUES ($1, $2, $3)
    `, ['2024-01-15', '14:30:00', '2024-01-15 14:30:00']);

    const result = await pool.query(`SELECT * FROM ${tableName}`);
    expect(result.rows[0].date_col).toBeDefined();
  });

  it('should handle UUID type', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        name VARCHAR(100)
      )
    `);

    const result = await pool.query(`
      INSERT INTO ${tableName} (name) VALUES ($1) RETURNING id
    `, ['Test']);

    expect(result.rows[0].id).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
    );
  });

  it('should handle JSON/JSONB types', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        data JSONB
      )
    `);

    const jsonData = { name: 'Alice', tags: ['admin', 'user'], nested: { key: 'value' } };
    await pool.query(`
      INSERT INTO ${tableName} (data) VALUES ($1)
    `, [JSON.stringify(jsonData)]);

    const result = await pool.query(`SELECT data FROM ${tableName}`);
    const data = typeof result.rows[0].data === 'string' 
      ? JSON.parse(result.rows[0].data) 
      : result.rows[0].data;
    expect(data.name).toBe('Alice');
    expect(data.tags).toContain('admin');
  });
});

describe('pg client - Constraints', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('constraints_test');
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('should enforce NOT NULL constraint', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100) NOT NULL
      )
    `);

    await expect(
      pool.query(`INSERT INTO ${tableName} (name) VALUES (NULL)`)
    ).rejects.toThrow();
  });

  it('should enforce UNIQUE constraint', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        email VARCHAR(100) UNIQUE
      )
    `);

    await pool.query(`INSERT INTO ${tableName} (email) VALUES ('test@example.com')`);

    await expect(
      pool.query(`INSERT INTO ${tableName} (email) VALUES ('test@example.com')`)
    ).rejects.toThrow();
  });

  it('should enforce CHECK constraint', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        age INTEGER CHECK (age >= 0 AND age <= 150)
      )
    `);

    await pool.query(`INSERT INTO ${tableName} (age) VALUES (25)`);

    await expect(
      pool.query(`INSERT INTO ${tableName} (age) VALUES (-1)`)
    ).rejects.toThrow();
  });

  it('should enforce PRIMARY KEY constraint', async () => {
    await pool.query(`
      CREATE TABLE ${tableName} (
        id INTEGER PRIMARY KEY,
        name VARCHAR(100)
      )
    `);

    await pool.query(`INSERT INTO ${tableName} (id, name) VALUES (1, 'Alice')`);

    await expect(
      pool.query(`INSERT INTO ${tableName} (id, name) VALUES (1, 'Bob')`)
    ).rejects.toThrow();
  });
});

describe('pg client - Prepared Statements', () => {
  let pool: pg.Pool;
  let tableName: string;

  beforeAll(async () => {
    pool = createPgPool();
  });

  afterAll(async () => {
    await pool.end();
  });

  beforeEach(async () => {
    tableName = generateTableName('prepared_test');
    await pool.query(`
      CREATE TABLE ${tableName} (
        id SERIAL PRIMARY KEY,
        name VARCHAR(100),
        value INTEGER
      )
    `);
  });

  afterEach(async () => {
    await dropTableIfExists(pool, tableName);
  });

  it('should execute parameterized queries', async () => {
    for (let i = 0; i < 10; i++) {
      await pool.query(
        `INSERT INTO ${tableName} (name, value) VALUES ($1, $2)`,
        [`item_${i}`, i * 10]
      );
    }

    const result = await pool.query(
      `SELECT * FROM ${tableName} WHERE value > $1 AND value < $2`,
      [20, 80]
    );

    expect(result.rows.length).toBe(5);
  });

  it('should handle NULL parameters', async () => {
    await pool.query(
      `INSERT INTO ${tableName} (name, value) VALUES ($1, $2)`,
      [null, null]
    );

    const result = await pool.query(
      `SELECT * FROM ${tableName} WHERE name IS NULL`
    );

    expect(result.rows.length).toBe(1);
  });
});
