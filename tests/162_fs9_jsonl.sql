-- fs9 jsonl decoder
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT _line_number, line::text AS line FROM extensions.fs9('/tmp/db9-fs9-test/logs.jsonl') ORDER BY _line_number;

DROP EXTENSION IF EXISTS fs9;
