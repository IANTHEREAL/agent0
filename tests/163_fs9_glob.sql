-- fs9 glob over multiple files (self-contained)
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT CASE WHEN fs9_exists('/tmp/db9-fs9-test/') THEN fs9_remove('/tmp/db9-fs9-test/', true) ELSE 0 END;
SELECT fs9_mkdir('/tmp/db9-fs9-test/sales', true);
SELECT fs9_write('/tmp/db9-fs9-test/sales/jan.csv', E'product,amount\nWidget,100\nGadget,200\n');
SELECT fs9_write('/tmp/db9-fs9-test/sales/feb.csv', E'product,amount\nWidget,150\nGadget,250\n');

SELECT _path, product, amount
FROM extensions.fs9('/tmp/db9-fs9-test/sales/*.csv', format => 'csv', header => true)
ORDER BY _path, product;

SELECT * FROM extensions.fs9('/tmp/db9-fs9-test/nonexistent-*.csv');

DROP EXTENSION IF EXISTS fs9;
