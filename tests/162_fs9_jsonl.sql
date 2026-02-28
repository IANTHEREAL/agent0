-- fs9 jsonl decoder (self-contained)
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT CASE WHEN fs9_exists('/tmp/db9-fs9-test/') THEN fs9_remove('/tmp/db9-fs9-test/', true) ELSE 0 END;
SELECT fs9_mkdir('/tmp/db9-fs9-test', true);
SELECT fs9_write('/tmp/db9-fs9-test/logs.jsonl', E'{"level":"INFO","message":"started"}\n{"level":"WARN","message":"slow query"}\n{"level":"ERROR","message":"connection lost"}\n');

SELECT _line_number, line::text AS line FROM extensions.fs9('/tmp/db9-fs9-test/logs.jsonl') ORDER BY _line_number;

DROP EXTENSION IF EXISTS fs9;
