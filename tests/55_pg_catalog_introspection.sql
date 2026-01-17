-- ORM-style pg_catalog introspection coverage (system catalogs must be joinable + stable)

DROP VIEW IF EXISTS cat_users_view;
DROP TABLE IF EXISTS cat_posts;
DROP TABLE IF EXISTS cat_users;
DROP TYPE IF EXISTS mood;

CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');

CREATE TABLE cat_users (
  id SERIAL PRIMARY KEY,
  email TEXT UNIQUE,
  mood mood,
  flag BOOLEAN DEFAULT true,
  created_at TIMESTAMP DEFAULT NOW()
);

CREATE TABLE cat_posts (
  id INT PRIMARY KEY,
  user_id INT REFERENCES cat_users(id)
);

CREATE VIEW cat_users_view AS
SELECT id, email FROM cat_users;

-- pg_proc should include minimal builtins used by ORMs.
SELECT 'builtin_pg_proc=' || count(*)
FROM pg_catalog.pg_proc p
JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = 'pg_catalog' AND p.proname = 'format_type';

-- pg_index / pg_class join should work (and boolean flags should be boolean).
SELECT 'index_count=' || count(*)
FROM pg_catalog.pg_class t
JOIN pg_catalog.pg_index ix ON t.oid = ix.indrelid
JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
WHERE n.nspname = 'public' AND t.relname = 'cat_users';

-- pg_get_indexdef() should return a real definition.
SELECT 'indexdef=' || lower(pg_get_indexdef(ix.indexrelid))
FROM pg_catalog.pg_class t
JOIN pg_catalog.pg_index ix ON t.oid = ix.indrelid
JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid
JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
WHERE n.nspname = 'public' AND t.relname = 'cat_users'
ORDER BY i.relname;

-- Defaults: pg_attrdef + pg_get_expr(adbin, adrelid).
SELECT 'attrdef_count=' || count(*)
FROM pg_catalog.pg_attrdef d
JOIN pg_catalog.pg_class c ON c.oid = d.adrelid
WHERE c.relname = 'cat_users';

SELECT 'default=' || coalesce(lower(pg_get_expr(d.adbin, d.adrelid)), '')
FROM pg_catalog.pg_attrdef d
JOIN pg_catalog.pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum
JOIN pg_catalog.pg_class c ON c.oid = d.adrelid
WHERE c.relname = 'cat_users'
ORDER BY a.attnum;

-- Sequences: pg_class relkind='S' joinable to pg_sequence via seqrelid.
SELECT 'pg_sequence_row=' || count(*)
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_sequence s ON s.seqrelid = c.oid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'public' AND c.relname = 'cat_users_id_seq';

-- Owned-by dependencies: pg_depend should link the implicit SERIAL sequence to the owning table column.
SELECT 'pg_depend_row=' || count(*)
FROM pg_catalog.pg_depend d
JOIN pg_catalog.pg_class seq ON seq.oid = d.objid
JOIN pg_catalog.pg_class tbl ON tbl.oid = d.refobjid
JOIN pg_catalog.pg_namespace ns ON ns.oid = seq.relnamespace
JOIN pg_catalog.pg_namespace nt ON nt.oid = tbl.relnamespace
WHERE ns.nspname = 'public'
  AND nt.nspname = 'public'
  AND seq.relname = 'cat_users_id_seq'
  AND tbl.relname = 'cat_users';

-- Enums: pg_type join pg_enum.
SELECT 'enum_typ=' || t.typname || ',label=' || e.enumlabel
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON t.oid = e.enumtypid
WHERE t.typname = 'mood'
ORDER BY e.enumsortorder;

-- format_type() should resolve user-defined types when pg_type is joined in the same row context.
SELECT 'format_type=' || format_type(a.atttypid, a.atttypmod)
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class c ON a.attrelid = c.oid
JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
WHERE c.relname = 'cat_users' AND a.attname = 'mood';

-- pg_tables / pg_views are common introspection entry points.
SELECT 'pg_tables=' || schemaname || '.' || tablename
FROM pg_catalog.pg_tables
WHERE tablename IN ('cat_users', 'cat_posts')
ORDER BY tablename;

SELECT 'pg_views=' || schemaname || '.' || viewname
FROM pg_catalog.pg_views
WHERE viewname = 'cat_users_view';

DROP VIEW cat_users_view;
DROP TABLE cat_posts;
DROP TABLE cat_users;
DROP TYPE mood;
