-- Issue #605: Parameterized LIMIT/OFFSET must work in all execution paths,
-- including the deferred-limit path (FOR UPDATE/SHARE).

DROP TABLE IF EXISTS param_limit_605;
CREATE TABLE param_limit_605 (
    id INT PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO param_limit_605 (id, name)
VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e');

-- 1) Simple parameterized LIMIT (optimizer path).
PREPARE p_limit(int) AS
SELECT id FROM param_limit_605 ORDER BY id LIMIT $1;
EXECUTE p_limit(3);
DEALLOCATE p_limit;

-- 2) Simple parameterized OFFSET (optimizer path).
PREPARE p_offset(int, int) AS
SELECT id FROM param_limit_605 ORDER BY id LIMIT $1 OFFSET $2;
EXECUTE p_offset(2, 2);
DEALLOCATE p_offset;

-- 3) Parameterized LIMIT with FOR UPDATE (deferred-limit path).
PREPARE p_for_update(int) AS
SELECT id FROM param_limit_605 ORDER BY id LIMIT $1 FOR UPDATE;
EXECUTE p_for_update(2);
DEALLOCATE p_for_update;

-- 4) Parameterized LIMIT + OFFSET with FOR UPDATE (deferred-limit path).
PREPARE p_for_update_both(int, int) AS
SELECT id FROM param_limit_605 ORDER BY id LIMIT $1 OFFSET $2 FOR UPDATE;
EXECUTE p_for_update_both(2, 1);
DEALLOCATE p_for_update_both;

-- 5) Parameterized LIMIT with FOR UPDATE SKIP LOCKED (deferred skip-locked path).
PREPARE p_skip_locked(int) AS
SELECT id FROM param_limit_605 ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED;
EXECUTE p_skip_locked(3);
DEALLOCATE p_skip_locked;

-- 6) NULL parameter means no bound (PG parity).
PREPARE p_null_limit(int) AS
SELECT id FROM param_limit_605 ORDER BY id LIMIT $1;
EXECUTE p_null_limit(NULL);
DEALLOCATE p_null_limit;

-- 7) Async projection + parameterized LIMIT (deferred async-projection path).
-- to_regtype() is catalog-dependent → triggers has_async_projection → passthrough + deferred LIMIT.
DROP TABLE IF EXISTS param_limit_async_605;
CREATE TABLE param_limit_async_605 (
    id INT PRIMARY KEY,
    type_name TEXT NOT NULL
);
INSERT INTO param_limit_async_605 (id, type_name)
VALUES (1, 'int8'), (2, 'int4'), (3, 'int2'), (4, 'text'), (5, 'boolean');

PREPARE p_async_proj(int, int) AS
SELECT id, to_regtype(type_name) FROM param_limit_async_605 ORDER BY id LIMIT $1 OFFSET $2;
EXECUTE p_async_proj(3, 1);
DEALLOCATE p_async_proj;

-- 8) Async ORDER BY + parameterized LIMIT + OFFSET (deferred order-by + deferred LIMIT path).
PREPARE p_async_order(int, int) AS
SELECT id, type_name FROM param_limit_async_605
ORDER BY to_regtype(type_name), id LIMIT $1 OFFSET $2;
EXECUTE p_async_order(3, 1);
DEALLOCATE p_async_order;

DROP TABLE param_limit_async_605;

DROP TABLE param_limit_605;
