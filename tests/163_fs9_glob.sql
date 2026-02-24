-- fs9 glob over multiple files
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT _path, product, amount
FROM extensions.fs9('/tmp/db9-fs9-test/sales/*.csv')
ORDER BY _path, product;

SELECT * FROM extensions.fs9('/tmp/db9-fs9-test/nonexistent-*.csv');

DROP EXTENSION IF EXISTS fs9;
