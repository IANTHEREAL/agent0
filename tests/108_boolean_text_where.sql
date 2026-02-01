DROP TABLE IF EXISTS bool_text_predicate;

CREATE TABLE bool_text_predicate(x TEXT);

INSERT INTO bool_text_predicate VALUES ('true'), ('false'), ('notabool');

-- Must be rejected: WHERE requires a boolean expression, not TEXT.
SELECT * FROM bool_text_predicate WHERE x;

DROP TABLE bool_text_predicate;

