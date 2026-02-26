-- Prisma upsert: ON CONFLICT (target) DO UPDATE must respect conflict target.
-- Validates that DO UPDATE only triggers for conflicts on the specified target
-- columns, not on any unique constraint.

DROP TABLE IF EXISTS t_prisma_upsert CASCADE;

CREATE TABLE t_prisma_upsert (
    id SERIAL PRIMARY KEY,
    email VARCHAR(255) UNIQUE NOT NULL,
    name VARCHAR(100) NOT NULL,
    age INT DEFAULT 0
);

-- Seed row.
INSERT INTO t_prisma_upsert (email, name, age) VALUES ('alice@example.com', 'Alice', 25);

-- Upsert on email conflict → DO UPDATE.
INSERT INTO t_prisma_upsert (email, name, age)
VALUES ('alice@example.com', 'Alice Updated', 30)
ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name, age = EXCLUDED.age;

SELECT email, name, age FROM t_prisma_upsert ORDER BY id;

-- Upsert with no conflict → plain INSERT.
INSERT INTO t_prisma_upsert (email, name, age)
VALUES ('bob@example.com', 'Bob', 35)
ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name, age = EXCLUDED.age;

SELECT email, name, age FROM t_prisma_upsert ORDER BY id;

-- Upsert with RETURNING.
INSERT INTO t_prisma_upsert (email, name, age)
VALUES ('alice@example.com', 'Alice Returned', 40)
ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name, age = EXCLUDED.age
RETURNING id, email, name, age;

DROP TABLE t_prisma_upsert CASCADE;
