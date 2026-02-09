CREATE EXTENSION IF NOT EXISTS fs9;

SELECT _path, product, amount
FROM extensions.fs9('/tmp/pgtikv-fs9-test/sales/*.csv', exclude => '*jan.csv')
ORDER BY _path, product;

SELECT *
FROM extensions.fs9('/tmp/pgtikv-fs9-test/sales/*.csv', exclude => '*jan.csv,*feb.csv');

DROP EXTENSION IF EXISTS fs9;
