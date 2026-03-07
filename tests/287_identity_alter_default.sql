-- Identity columns must reject ALTER COLUMN SET/DROP DEFAULT.
-- Markerless legacy identity shape (owned nextval default without marker)
-- is not distinguishable from SERIAL and is currently treated as alterable.

DROP TABLE IF EXISTS marker_identity_alter_default;
CREATE TABLE marker_identity_alter_default (id BIGINT GENERATED ALWAYS AS IDENTITY, payload TEXT);
INSERT INTO marker_identity_alter_default DEFAULT VALUES;

ALTER TABLE marker_identity_alter_default ALTER COLUMN id SET DEFAULT 42;
ALTER TABLE marker_identity_alter_default ALTER COLUMN id DROP DEFAULT;

INSERT INTO marker_identity_alter_default DEFAULT VALUES;
SELECT 'marker_ids=' || min(id)::text || ',' || max(id)::text AS probe
FROM marker_identity_alter_default;

DROP TABLE marker_identity_alter_default;

DROP TABLE IF EXISTS legacy_identity_alter_default;
DROP SEQUENCE IF EXISTS legacy_identity_alter_default_id_seq;

CREATE TABLE legacy_identity_alter_default (id INT);
CREATE SEQUENCE legacy_identity_alter_default_id_seq START WITH 1;
ALTER SEQUENCE legacy_identity_alter_default_id_seq OWNED BY legacy_identity_alter_default.id;
ALTER TABLE legacy_identity_alter_default
  ALTER COLUMN id
  SET DEFAULT nextval('legacy_identity_alter_default_id_seq'::regclass);

INSERT INTO legacy_identity_alter_default DEFAULT VALUES;

ALTER TABLE legacy_identity_alter_default ALTER COLUMN id SET DEFAULT 123;
ALTER TABLE legacy_identity_alter_default ALTER COLUMN id DROP DEFAULT;
SELECT 'legacy_after_drop_default_default_is_null' AS check_name,
       column_default IS NULL AS ok
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'legacy_identity_alter_default'
  AND column_name = 'id';

INSERT INTO legacy_identity_alter_default DEFAULT VALUES;
SELECT 'legacy_ids=' || string_agg(COALESCE(id::text, 'NULL'), ',' ORDER BY id NULLS LAST) AS probe
FROM legacy_identity_alter_default;

DROP TABLE legacy_identity_alter_default;
