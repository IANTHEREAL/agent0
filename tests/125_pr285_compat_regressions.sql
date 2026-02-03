-- Regression coverage for PR #285 (Fix Dify/GORM compatibility issues)
--
-- Note: #283 (binary Type::UNKNOWN parameters) is covered by Rust unit tests in
-- `src/protocol/handler.rs` because it requires pgwire extended-query binary parameters.

DROP TABLE IF EXISTS pr285_anyop;
CREATE TABLE pr285_anyop (relname TEXT, relkind TEXT);
INSERT INTO pr285_anyop VALUES ('other', 'x'), ('actor', 'r');

-- #284: AnyOp must be treated as boolean in AND short-circuit validation.
SELECT relname
FROM pr285_anyop
WHERE relname = 'actor' AND relkind = ANY(ARRAY['r', 'p'])
ORDER BY relname;

-- Also cover AllOp.
SELECT relname
FROM pr285_anyop
WHERE relname = 'actor' AND relkind = ALL(ARRAY['r', 'r'])
ORDER BY relname;

DROP TABLE pr285_anyop;

-- Schema-qualified boolean columns (schema.table.column) must validate as boolean inside JOIN
-- contexts (JoinEvalContext.column_type only handles 2-part identifiers).
DROP TABLE IF EXISTS pr285_a;
DROP TABLE IF EXISTS pr285_b;
CREATE TABLE pr285_a (id INT, flag BOOLEAN);
CREATE TABLE pr285_b (id INT);

INSERT INTO pr285_a VALUES (1, true), (2, false);
INSERT INTO pr285_b VALUES (1), (2);

SELECT public.pr285_b.id AS id
FROM public.pr285_b
JOIN public.pr285_a ON public.pr285_a.id = public.pr285_b.id
WHERE public.pr285_b.id = 1 AND public.pr285_a.flag
ORDER BY public.pr285_b.id;

DROP TABLE pr285_a;
DROP TABLE pr285_b;

-- Boolean function typing via the registry should allow boolean contexts to validate.
SELECT (false AND pg_table_is_visible(1)) AS ok;
