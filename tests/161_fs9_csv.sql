-- fs9 csv/tsv decoding
CREATE EXTENSION IF NOT EXISTS fs9;

-- CSV with header
SELECT name, age, city FROM extensions.fs9('/tmp/db9-fs9-test/users.csv') ORDER BY name;

-- TSV with header
SELECT id, value FROM extensions.fs9('/tmp/db9-fs9-test/data.tsv') ORDER BY id;

-- Empty CSV
SELECT * FROM extensions.fs9('/tmp/db9-fs9-test/empty.csv');

-- Header-only CSV
SELECT * FROM extensions.fs9('/tmp/db9-fs9-test/header_only.csv');

DROP EXTENSION IF EXISTS fs9;
