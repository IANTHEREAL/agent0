-- SQL PREPARE should infer parameter count from placeholders even when the
-- optional explicit type list is omitted.

PREPARE prepare_infer_text AS SELECT $1::int + 1 AS v;
EXECUTE prepare_infer_text(41);
DEALLOCATE prepare_infer_text;

-- Generic registry-based parameter inference: ordinary single-signature
-- functions should type $N params from their declared arg_types.
-- PostgreSQL 17.9: PREPARE q AS SELECT lower($1); succeeds (infers text).
PREPARE prepare_lower AS SELECT lower($1) AS v;
EXECUTE prepare_lower('HELLO');
DEALLOCATE prepare_lower;

PREPARE prepare_length AS SELECT length($1) AS v;
EXECUTE prepare_length('test');
DEALLOCATE prepare_length;

-- Numeric widening: int → float8 is allowed (PG accepts this).
PREPARE prepare_sqrt AS SELECT sqrt($1) AS v;
EXECUTE prepare_sqrt(16);
DEALLOCATE prepare_sqrt;
