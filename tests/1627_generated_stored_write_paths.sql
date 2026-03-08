DROP TABLE IF EXISTS gen_write_paths;

CREATE TABLE gen_write_paths (
    id INT PRIMARY KEY,
    b INT,
    c INT GENERATED ALWAYS AS (b + 1) STORED
);

INSERT INTO gen_write_paths (id, b) VALUES (1, 10);
INSERT INTO gen_write_paths (id, b) VALUES (1, 20)
ON CONFLICT (id) DO UPDATE SET b = EXCLUDED.b;
SELECT 'upsert=' || id::text || ',' || b::text || ',' || c::text AS probe
FROM gen_write_paths;

COPY gen_write_paths (id, b) FROM STDIN;
2	30
3	40
\.

SELECT 'copy=' || id::text || ',' || b::text || ',' || c::text AS probe
FROM gen_write_paths
WHERE id IN (2, 3)
ORDER BY id;

COPY gen_write_paths (id, b, c) FROM STDIN;
