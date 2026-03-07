-- PostgreSQL compatibility: named table-function arguments with := should parse.
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT CASE
  WHEN fs9_exists('/tmp/db9-fs9-295/') THEN fs9_remove('/tmp/db9-fs9-295/', true)
  ELSE 0
END;
SELECT fs9_mkdir('/tmp/db9-fs9-295', true);
SELECT fs9_write('/tmp/db9-fs9-295/users.psv', E'id|name\n1|alice\n2|bob\n');

SELECT id, name
FROM extensions.fs9(
  '/tmp/db9-fs9-295/users.psv',
  format := 'csv',
  delimiter := '|',
  header := true
)
ORDER BY id;

-- Keep one runtime error so this test can validate .assert + .errors together.
SELECT *
FROM extensions.fs9(
  '/tmp/db9-fs9-295/users.psv',
  format := 'csv',
  bogus := 'x'
);

SELECT CASE
  WHEN fs9_exists('/tmp/db9-fs9-295/') THEN fs9_remove('/tmp/db9-fs9-295/', true)
  ELSE 0
END;
DROP EXTENSION IF EXISTS fs9;
