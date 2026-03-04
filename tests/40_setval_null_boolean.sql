-- Regression: setval(seq, val, NULL::boolean) must return NULL per PG strict semantics (#1371).
-- PG's 3-arg setval is strict (proisstrict=true): NULL arg → NULL return, no side effect.

DROP SEQUENCE IF EXISTS seq_null_bool;
CREATE SEQUENCE seq_null_bool;

-- NULL boolean argument → strict NULL return, no mutation.
SELECT setval('seq_null_bool', 1, NULL::boolean);
-- Sequence untouched: nextval returns START value (1).
SELECT nextval('seq_null_bool');

-- Explicit true/false must still work.
SELECT setval('seq_null_bool', 10, true);
SELECT setval('seq_null_bool', 20, false);
SELECT nextval('seq_null_bool');

DROP SEQUENCE seq_null_bool;
