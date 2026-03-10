-- Regression: quote_ident() must reject non-text arguments (PG parity).
-- Issue: https://github.com/c4pt0r/db9-server/issues/1673

-- OK: text literal
SELECT quote_ident('hello');

-- OK: text expression
SELECT quote_ident('has space'::text);

-- ERROR: integer argument must be rejected (PG SQLSTATE 42883)
SELECT quote_ident(42);

-- ERROR: boolean argument must be rejected
SELECT quote_ident(true);

-- ERROR: float argument must be rejected
SELECT quote_ident(3.14);

-- OK: bare NULL resolves to text, returns NULL (PG parity)
SELECT quote_ident(NULL);

-- ERROR: NULL::int is typed as integer, must be rejected (PG parity)
SELECT quote_ident(NULL::int);
