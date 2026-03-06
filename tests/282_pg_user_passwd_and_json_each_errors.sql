-- Issues #1486, #1487: pg_user passwd masking + json_each error messages (PG parity).

-- #1486: pg_user.passwd must return '********' (not NULL)
SELECT 'a1_passwd=' || passwd FROM pg_user WHERE usename = (SELECT current_user);

-- #1487: error messages must match PG 17.7 exactly
-- jsonb variants: "cannot call <func> on a non-object" (same for array and scalar)
-- json variants:  "cannot deconstruct an array as an object" / "cannot deconstruct a scalar"

-- Table-function path (FROM clause)
SELECT * FROM jsonb_each_text('[1]'::jsonb);

SELECT * FROM json_each_text('[1]'::json);

-- Expression-eval path (SELECT list): array input
SELECT jsonb_each('[1]'::jsonb);

SELECT json_each('[1]'::json);

SELECT jsonb_each_text('[1]'::jsonb);

SELECT json_each_text('[1]'::json);

-- Expression-eval path (SELECT list): scalar input
SELECT jsonb_each('1'::jsonb);

SELECT json_each('1'::json);

SELECT jsonb_each_text('1'::jsonb);

SELECT json_each_text('1'::json);
