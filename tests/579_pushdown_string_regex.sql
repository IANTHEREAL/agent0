-- DB9_DIVERGENCE(#2402): DB9 Cop pushdown is a db9-specific exact-pair contract.
-- DB9 cop pushdown: supported string projections push; regex expressions stay local on the exact current pair.

DROP TABLE IF EXISTS db9_cop_string_regex_smoke;
DROP TABLE IF EXISTS db9_cop_string_projection_on;
DROP TABLE IF EXISTS db9_cop_string_projection_off;
DROP TABLE IF EXISTS db9_cop_regex_operator_projection_on;
DROP TABLE IF EXISTS db9_cop_regex_operator_projection_off;
DROP TABLE IF EXISTS db9_cop_regex_function_on;
DROP TABLE IF EXISTS db9_cop_regex_function_off;

CREATE TABLE db9_cop_string_regex_smoke(
    id INT PRIMARY KEY,
    n INT NOT NULL,
    txt TEXT NOT NULL,
    txt2 TEXT NOT NULL,
    csv_txt TEXT NOT NULL,
    ident_txt TEXT NOT NULL,
    char_code INT NOT NULL,
    null_txt TEXT
);
CREATE INDEX db9_cop_string_regex_smoke_n_idx ON db9_cop_string_regex_smoke(n);
INSERT INTO db9_cop_string_regex_smoke VALUES
    (1, 10, 'before value', 'pq', 'u,v,w', 'plain_name', 66, 'fallback'),
    (2, 20, 'alpha beta', 'xy', 'aa,bb,cc', 'select', 65, NULL),
    (3, 30, 'omega zone', 'zz', 'dd,ee,ff', 'mixedCase', 67, 'tail');

SET db9.enable_cop_pushdown = on;
\o /tmp/579_string_projection_explain.txt
EXPLAIN SELECT
    left(txt, 5) AS left_out,
    right(txt, 4) AS right_out,
    reverse(txt2) AS reverse_out,
    ascii(txt2) AS ascii_out,
    chr(char_code) AS chr_out,
    position('b' IN txt) AS position_out,
    strpos(txt, null_txt) AS strpos_null_needle_out,
    split_part(csv_txt, ',', 2) AS split_part_out,
    md5(null_txt) AS md5_null_out,
    sha256(null_txt) AS sha256_null_out
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
\o /tmp/579_string_local_only_projection_explain.txt
EXPLAIN SELECT
    concat(txt, '-', txt2, '-', n) AS concat_out,
    concat_ws('/', txt, NULL, txt2, n) AS concat_ws_out,
    concat_ws(n, txt, txt2) AS concat_ws_int_separator_out,
    repeat(txt2, 2) AS repeat_out,
    initcap(txt) AS initcap_out,
    lpad(txt2, 4, '0') AS lpad_out,
    rpad(txt2, 4, '0') AS rpad_out,
    replace(txt, 'beta', 'BETA') AS replace_out,
    translate(txt, 'ab', 'AB') AS translate_out,
    quote_ident(ident_txt) AS quote_ident_out,
    quote_literal(txt2) AS quote_literal_out,
    quote_nullable(txt2) AS quote_nullable_out,
    overlay(txt placing txt2 from 7 for 4) AS overlay_out,
    format('%s/%L/%I', txt2, txt2, ident_txt) AS format_out
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
\o /tmp/579_regex_operator_projection_explain.txt
EXPLAIN SELECT
    txt ~ 'beta' AS regex_match_flag,
    txt !~ 'omega' AS regex_not_match_flag,
    txt ~ '\bbeta\b' AS regex_pg_b_escape_flag
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
\o /tmp/579_regex_imatch_projection_explain.txt
EXPLAIN SELECT
    txt ~* 'ALPHA' AS regex_imatch_flag,
    txt !~* 'OMEGA' AS regex_not_imatch_flag
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
\o /tmp/579_regex_function_explain.txt
EXPLAIN SELECT
    regexp_replace(txt, '[ae]', 'X', 'gi') AS regexp_replace_default_push,
    regexp_replace(txt, '^beta$', 'X', 'm') AS regexp_replace_multiline_push,
    regexp_split_to_array(txt, ' +') AS regex_split_push,
    regexp_match(txt, '(alpha)') AS regexp_match_push
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop Output: left_out, right_out, reverse_out, ascii_out, chr_out, position_out, strpos_null_needle_out, split_part_out, md5_null_out, sha256_null_out" /tmp/579_string_projection_explain.txt; then echo "string_projection_pushes|1"; else echo "string_projection_pushes|0"; fi
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/579_string_local_only_projection_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/579_string_local_only_projection_explain.txt; then echo "string_local_only_projection_stays_local|1"; else echo "string_local_only_projection_stays_local|0"; fi
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/579_regex_operator_projection_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/579_regex_operator_projection_explain.txt; then echo "regex_operator_projection_stays_local|1"; else echo "regex_operator_projection_stays_local|0"; fi
\! if grep -Fq "DB9 Cop Access: point (20)" /tmp/579_regex_imatch_projection_explain.txt && ! grep -Fq "DB9 Cop Output:" /tmp/579_regex_imatch_projection_explain.txt; then echo "regex_imatch_projection_stays_local|1"; else echo "regex_imatch_projection_stays_local|0"; fi
\! if grep -Fq "DB9 Cop Output:" /tmp/579_regex_function_explain.txt; then echo "regex_function_projection_stays_local|0"; else echo "regex_function_projection_stays_local|1"; fi
SELECT
    concat(txt, '-', txt2, '-', n) AS concat_out,
    concat_ws('/', txt, NULL, txt2, n) AS concat_ws_out,
    concat_ws(n, txt, txt2) AS concat_ws_int_separator_out,
    left(txt, 5) AS left_out,
    right(txt, 4) AS right_out,
    repeat(txt2, 2) AS repeat_out,
    reverse(txt2) AS reverse_out,
    initcap(txt) AS initcap_out,
    ascii(txt2) AS ascii_out,
    chr(char_code) AS chr_out,
    lpad(txt2, 4, '0') AS lpad_out,
    lpad(txt2, 4, null_txt) AS lpad_null_fill_out,
    rpad(txt2, 4, '0') AS rpad_out,
    rpad(txt2, 4, null_txt) AS rpad_null_fill_out,
    replace(txt, 'beta', 'BETA') AS replace_out,
    replace(txt, null_txt, 'BETA') AS replace_null_from_out,
    replace(txt, 'beta', null_txt) AS replace_null_to_out,
    translate(txt, 'ab', 'AB') AS translate_out,
    translate(txt, null_txt, 'AB') AS translate_null_from_out,
    translate(txt, 'ab', null_txt) AS translate_null_to_out,
    position('b' IN txt) AS position_out,
    strpos(txt, null_txt) AS strpos_null_needle_out,
    split_part(csv_txt, ',', 2) AS split_part_out,
    quote_ident(ident_txt) AS quote_ident_out,
    quote_literal(txt2) AS quote_literal_out,
    quote_nullable(txt2) AS quote_nullable_out,
    overlay(txt placing txt2 from 7 for 4) AS overlay_out,
    md5(null_txt) AS md5_null_out,
    sha256(null_txt) AS sha256_null_out,
    format('%s/%L/%I', txt2, txt2, ident_txt) AS format_out
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_string_projection_on AS
SELECT
    concat(txt, '-', txt2, '-', n) AS concat_out,
    concat_ws('/', txt, NULL, txt2, n) AS concat_ws_out,
    concat_ws(n, txt, txt2) AS concat_ws_int_separator_out,
    left(txt, 5) AS left_out,
    right(txt, 4) AS right_out,
    repeat(txt2, 2) AS repeat_out,
    reverse(txt2) AS reverse_out,
    initcap(txt) AS initcap_out,
    ascii(txt2) AS ascii_out,
    chr(char_code) AS chr_out,
    lpad(txt2, 4, '0') AS lpad_out,
    lpad(txt2, 4, null_txt) AS lpad_null_fill_out,
    rpad(txt2, 4, '0') AS rpad_out,
    rpad(txt2, 4, null_txt) AS rpad_null_fill_out,
    replace(txt, 'beta', 'BETA') AS replace_out,
    replace(txt, null_txt, 'BETA') AS replace_null_from_out,
    replace(txt, 'beta', null_txt) AS replace_null_to_out,
    translate(txt, 'ab', 'AB') AS translate_out,
    translate(txt, null_txt, 'AB') AS translate_null_from_out,
    translate(txt, 'ab', null_txt) AS translate_null_to_out,
    position('b' IN txt) AS position_out,
    strpos(txt, null_txt) AS strpos_null_needle_out,
    split_part(csv_txt, ',', 2) AS split_part_out,
    quote_ident(ident_txt) AS quote_ident_out,
    quote_literal(txt2) AS quote_literal_out,
    quote_nullable(txt2) AS quote_nullable_out,
    overlay(txt placing txt2 from 7 for 4) AS overlay_out,
    md5(null_txt) AS md5_null_out,
    sha256(null_txt) AS sha256_null_out,
    format('%s/%L/%I', txt2, txt2, ident_txt) AS format_out
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
SELECT
    txt ~ 'beta' AS regex_match_flag,
    txt ~* 'ALPHA' AS regex_imatch_flag,
    txt !~ 'omega' AS regex_not_match_flag,
    txt !~* 'OMEGA' AS regex_not_imatch_flag,
    txt ~ '\bbeta\b' AS regex_pg_b_escape_flag
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_regex_operator_projection_on AS
SELECT
    txt ~ 'beta' AS regex_match_flag,
    txt ~* 'ALPHA' AS regex_imatch_flag,
    txt !~ 'omega' AS regex_not_match_flag,
    txt !~* 'OMEGA' AS regex_not_imatch_flag,
    txt ~ '\bbeta\b' AS regex_pg_b_escape_flag
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
SELECT
    regexp_replace(txt, '[ae]', 'X', 'gi') AS regexp_replace_default_push,
    regexp_replace(txt, '^beta$', 'X', 'm') AS regexp_replace_multiline_push,
    regexp_split_to_array(txt, ' +') AS regex_split_push,
    regexp_match(txt, '(alpha)') AS regexp_match_push
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_regex_function_on AS
SELECT
    regexp_replace(txt, '[ae]', 'X', 'gi') AS regexp_replace_default_push,
    regexp_replace(txt, '^beta$', 'X', 'm') AS regexp_replace_multiline_push,
    regexp_split_to_array(txt, ' +') AS regex_split_push,
    regexp_match(txt, '(alpha)') AS regexp_match_push
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
CREATE TEMP TABLE db9_cop_string_projection_off AS
SELECT
    concat(txt, '-', txt2, '-', n) AS concat_out,
    concat_ws('/', txt, NULL, txt2, n) AS concat_ws_out,
    concat_ws(n, txt, txt2) AS concat_ws_int_separator_out,
    left(txt, 5) AS left_out,
    right(txt, 4) AS right_out,
    repeat(txt2, 2) AS repeat_out,
    reverse(txt2) AS reverse_out,
    initcap(txt) AS initcap_out,
    ascii(txt2) AS ascii_out,
    chr(char_code) AS chr_out,
    lpad(txt2, 4, '0') AS lpad_out,
    lpad(txt2, 4, null_txt) AS lpad_null_fill_out,
    rpad(txt2, 4, '0') AS rpad_out,
    rpad(txt2, 4, null_txt) AS rpad_null_fill_out,
    replace(txt, 'beta', 'BETA') AS replace_out,
    replace(txt, null_txt, 'BETA') AS replace_null_from_out,
    replace(txt, 'beta', null_txt) AS replace_null_to_out,
    translate(txt, 'ab', 'AB') AS translate_out,
    translate(txt, null_txt, 'AB') AS translate_null_from_out,
    translate(txt, 'ab', null_txt) AS translate_null_to_out,
    position('b' IN txt) AS position_out,
    strpos(txt, null_txt) AS strpos_null_needle_out,
    split_part(csv_txt, ',', 2) AS split_part_out,
    quote_ident(ident_txt) AS quote_ident_out,
    quote_literal(txt2) AS quote_literal_out,
    quote_nullable(txt2) AS quote_nullable_out,
    overlay(txt placing txt2 from 7 for 4) AS overlay_out,
    md5(null_txt) AS md5_null_out,
    sha256(null_txt) AS sha256_null_out,
    format('%s/%L/%I', txt2, txt2, ident_txt) AS format_out
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_regex_operator_projection_off AS
SELECT
    txt ~ 'beta' AS regex_match_flag,
    txt ~* 'ALPHA' AS regex_imatch_flag,
    txt !~ 'omega' AS regex_not_match_flag,
    txt !~* 'OMEGA' AS regex_not_imatch_flag,
    txt ~ '\bbeta\b' AS regex_pg_b_escape_flag
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
CREATE TEMP TABLE db9_cop_regex_function_off AS
SELECT
    regexp_replace(txt, '[ae]', 'X', 'gi') AS regexp_replace_default_push,
    regexp_replace(txt, '^beta$', 'X', 'm') AS regexp_replace_multiline_push,
    regexp_split_to_array(txt, ' +') AS regex_split_push,
    regexp_match(txt, '(alpha)') AS regexp_match_push
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;

SELECT 'string_projection_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_string_projection_on
            EXCEPT ALL
            SELECT * FROM db9_cop_string_projection_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_string_projection_off
            EXCEPT ALL
            SELECT * FROM db9_cop_string_projection_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'regex_operator_projection_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_regex_operator_projection_on
            EXCEPT ALL
            SELECT * FROM db9_cop_regex_operator_projection_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_regex_operator_projection_off
            EXCEPT ALL
            SELECT * FROM db9_cop_regex_operator_projection_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SELECT 'regex_function_parity' AS check_name, mismatch_count
FROM (
    SELECT (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_regex_function_on
            EXCEPT ALL
            SELECT * FROM db9_cop_regex_function_off
        ) AS left_only
    ) + (
        SELECT COUNT(*) FROM (
            SELECT * FROM db9_cop_regex_function_off
            EXCEPT ALL
            SELECT * FROM db9_cop_regex_function_on
        ) AS right_only
    ) AS mismatch_count
) AS parity;

SET db9.enable_cop_pushdown = on;
\o /tmp/579_like_escape_explain.txt
EXPLAIN SELECT
    txt LIKE 'a%' ESCAPE 'xx' AS like_invalid_escape
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
\o /tmp/579_ilike_escape_explain.txt
EXPLAIN SELECT
    txt ILIKE 'a%' ESCAPE 'xx' AS ilike_invalid_escape
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
\o
\! if grep -Fq "DB9 Cop" /tmp/579_like_escape_explain.txt; then echo "like_escape_pushes|1"; else echo "like_escape_pushes|0"; fi
\! if grep -Fq "DB9 Cop" /tmp/579_ilike_escape_explain.txt; then echo "ilike_escape_pushes|1"; else echo "ilike_escape_pushes|0"; fi
SELECT
    null_txt LIKE 'a%' ESCAPE 'xx' AS like_invalid_escape
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
SELECT
    null_txt ILIKE 'a%' ESCAPE 'xx' AS ilike_invalid_escape
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;

SET db9.enable_cop_pushdown = off;
SELECT
    null_txt LIKE 'a%' ESCAPE 'xx' AS like_invalid_escape
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;
SELECT
    null_txt ILIKE 'a%' ESCAPE 'xx' AS ilike_invalid_escape
FROM db9_cop_string_regex_smoke
WHERE n = 20
LIMIT 1;

DROP TABLE db9_cop_string_regex_smoke;
DROP TABLE db9_cop_string_projection_on;
DROP TABLE db9_cop_string_projection_off;
DROP TABLE db9_cop_regex_operator_projection_on;
DROP TABLE db9_cop_regex_operator_projection_off;
DROP TABLE db9_cop_regex_function_on;
DROP TABLE db9_cop_regex_function_off;
