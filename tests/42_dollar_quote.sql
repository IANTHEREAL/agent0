-- Dollar-quoted string literals (MVP): $$...$$ and $tag$...$tag$

DROP TABLE IF EXISTS dq_t;
CREATE TABLE dq_t (id INT PRIMARY KEY, v TEXT);

INSERT INTO dq_t (id, v) VALUES (1, $$hello$$);
INSERT INTO dq_t (id, v) VALUES (2, $tag$world$tag$);

SELECT 'ROW=' || id || ':' || v FROM dq_t ORDER BY id;
SELECT 'BODY=' || $$ $1 $$;

DROP TABLE dq_t;

