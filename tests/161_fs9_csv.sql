-- fs9 csv/tsv decoding (self-contained)
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT CASE WHEN fs9_exists('/tmp/db9-fs9-test/') THEN fs9_remove('/tmp/db9-fs9-test/', true) ELSE 0 END;
SELECT fs9_mkdir('/tmp/db9-fs9-test', true);
SELECT fs9_write('/tmp/db9-fs9-test/users.csv', E'name,age,city\nAlice,30,Beijing\nBob,25,Shanghai\nCharlie,35,Shenzhen\n');
SELECT fs9_write('/tmp/db9-fs9-test/data.tsv', E'id\tvalue\n1\talpha\n2\tbeta\n');
SELECT fs9_write('/tmp/db9-fs9-test/empty.csv', '');
SELECT fs9_write('/tmp/db9-fs9-test/header_only.csv', E'name,age,city\n');

-- CSV with header
SELECT name, age, city FROM extensions.fs9('/tmp/db9-fs9-test/users.csv') ORDER BY name;

-- TSV with header
SELECT id, value FROM extensions.fs9('/tmp/db9-fs9-test/data.tsv') ORDER BY id;

-- Empty CSV
SELECT * FROM extensions.fs9('/tmp/db9-fs9-test/empty.csv');

-- Header-only CSV
SELECT * FROM extensions.fs9('/tmp/db9-fs9-test/header_only.csv');

DROP EXTENSION IF EXISTS fs9;
