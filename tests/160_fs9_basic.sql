-- fs9 basic: directory listing and raw text
CREATE EXTENSION IF NOT EXISTS fs9;

-- Directory listing (deterministic columns only)
SELECT path, type FROM extensions.fs9('/tmp/pgtikv-fs9-test/') ORDER BY path;

-- Raw text file
SELECT _line_number, line FROM extensions.fs9('/tmp/pgtikv-fs9-test/hello.txt') ORDER BY _line_number;

DROP EXTENSION IF EXISTS fs9;
