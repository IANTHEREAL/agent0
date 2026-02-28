-- fs9 basic: self-contained fixtures
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT CASE WHEN fs9_exists('/tmp/db9-fs9-test/') THEN fs9_remove('/tmp/db9-fs9-test/', true) ELSE 0 END;
SELECT fs9_mkdir('/tmp/db9-fs9-test/sales', true);
SELECT fs9_mkdir('/tmp/db9-fs9-test/subdir', true);
SELECT fs9_write('/tmp/db9-fs9-test/hello.txt', E'Hello World\nThis is line two\nThird line here\n');
SELECT fs9_write('/tmp/db9-fs9-test/users.csv', E'name,age,city\nAlice,30,Beijing\nBob,25,Shanghai\nCharlie,35,Shenzhen\n');
SELECT fs9_write('/tmp/db9-fs9-test/data.tsv', E'id\tvalue\n1\talpha\n2\tbeta\n');
SELECT fs9_write('/tmp/db9-fs9-test/logs.jsonl', E'{"level":"INFO","message":"started"}\n{"level":"WARN","message":"slow query"}\n{"level":"ERROR","message":"connection lost"}\n');
SELECT fs9_write('/tmp/db9-fs9-test/empty.csv', '');
SELECT fs9_write('/tmp/db9-fs9-test/header_only.csv', E'name,age,city\n');
SELECT fs9_write('/tmp/db9-fs9-test/subdir/nested.txt', E'nested content\n');
SELECT fs9_write('/tmp/db9-fs9-test/sales/jan.csv', E'product,amount\nWidget,100\nGadget,200\n');
SELECT fs9_write('/tmp/db9-fs9-test/sales/feb.csv', E'product,amount\nWidget,150\nGadget,250\n');

-- Directory listing (deterministic columns only)
SELECT path, type FROM extensions.fs9('/tmp/db9-fs9-test/') ORDER BY path;

-- Raw text file
SELECT _line_number, line FROM extensions.fs9('/tmp/db9-fs9-test/hello.txt') ORDER BY _line_number;

DROP EXTENSION IF EXISTS fs9;
