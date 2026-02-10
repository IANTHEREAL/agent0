-- GIN correctness tests: Int32/Int64 cross-type, tsquery NOT/OR, JSONB regression guard

-- Section 1: ARRAY Int32/Int64 cross-type containment (BUG 1 fix)
DROP TABLE IF EXISTS gin_array_types;
CREATE TABLE gin_array_types (
    id SERIAL PRIMARY KEY,
    tags INT[]
);
CREATE INDEX idx_array_types_gin ON gin_array_types USING GIN (tags);
INSERT INTO gin_array_types (tags) VALUES (ARRAY[1, 2, 3]);
INSERT INTO gin_array_types (tags) VALUES (ARRAY[4, 5, 6]);
SELECT id FROM gin_array_types WHERE tags @> ARRAY[1] ORDER BY id;
SELECT id FROM gin_array_types WHERE tags @> ARRAY[1, 2] ORDER BY id;
DROP TABLE gin_array_types;

-- Section 2: FTS tsquery with NOT and OR (BUG 5 fix)
DROP TABLE IF EXISTS gin_fts_not;
CREATE TABLE gin_fts_not (
    id SERIAL PRIMARY KEY,
    doc TSVECTOR
);
CREATE INDEX idx_fts_not_gin ON gin_fts_not USING GIN (doc);
INSERT INTO gin_fts_not (doc) VALUES (to_tsvector('hello world'));
INSERT INTO gin_fts_not (doc) VALUES (to_tsvector('hello rust'));
INSERT INTO gin_fts_not (doc) VALUES (to_tsvector('world rust'));
SELECT id FROM gin_fts_not WHERE doc @@ 'hello & !world'::tsquery ORDER BY id;
SELECT id FROM gin_fts_not WHERE doc @@ 'hello | world'::tsquery ORDER BY id;
DROP TABLE gin_fts_not;

-- Section 3: JSONB GIN basic (regression guard)
DROP TABLE IF EXISTS gin_jsonb_guard;
CREATE TABLE gin_jsonb_guard (
    id SERIAL PRIMARY KEY,
    data JSONB
);
CREATE INDEX idx_jsonb_guard_gin ON gin_jsonb_guard USING GIN (data);
INSERT INTO gin_jsonb_guard (data) VALUES ('{"a": 1}'), ('{"a": 2, "b": 1}'), ('{"c": 3}');
SELECT id FROM gin_jsonb_guard WHERE data @> '{"a": 1}' ORDER BY id;
SELECT id FROM gin_jsonb_guard WHERE data @> '{"b": 1}' ORDER BY id;
DROP TABLE gin_jsonb_guard;
