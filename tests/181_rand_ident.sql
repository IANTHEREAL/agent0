-- PostgreSQL compatible tests from rand_ident
-- 12 tests

CREATE SCHEMA IF NOT EXISTS crdb_internal;

-- CockroachDB exposes crdb_internal.gen_rand_ident(); implement a small,
-- deterministic Postgres equivalent for these compatibility tests.
CREATE OR REPLACE FUNCTION crdb_internal.gen_rand_ident(
  base TEXT,
  n INT,
  opts JSONB DEFAULT '{}'::JSONB
)
RETURNS SETOF TEXT
LANGUAGE plpgsql
AS $$
DECLARE
  i INT;
  j INT;
  seed INT := COALESCE((opts->>'seed')::INT, 0);
  suffix BOOL := COALESCE((opts->>'suffix')::BOOL, TRUE);
  noise BOOL := COALESCE((opts->>'noise')::BOOL, TRUE);
  punctuate INT := COALESCE((opts->>'punctuate')::INT, 0);
  quote INT := COALESCE((opts->>'quote')::INT, 0);
  space INT := COALESCE((opts->>'space')::INT, 0);
  whitespace INT := COALESCE((opts->>'whitespace')::INT, 0);
  emote INT := COALESCE((opts->>'emote')::INT, 0);
  diacritics INT := COALESCE((opts->>'diacritics')::INT, 0);
  diacritic_depth INT := COALESCE((opts->>'diacritic_depth')::INT, 1);
  zalgo BOOL := COALESCE((opts->>'zalgo')::BOOL, FALSE);
  h TEXT;
  bytes BYTEA;
  base_len INT;
  core_len INT;
  out TEXT;
  marks TEXT;
BEGIN
  IF n < 0 THEN
    RAISE EXCEPTION 'n must be >= 0';
  END IF;

  base_len := GREATEST(LENGTH(base), 1);
  core_len := LEAST(base_len, 10);

  FOR i IN 1..n LOOP
    h := md5(seed::TEXT || ':' || base || ':' || i::TEXT);
    bytes := decode(h, 'hex');

    out := '';
    FOR j IN 0..core_len-1 LOOP
      out := out || substr(base, (get_byte(bytes, j) % base_len) + 1, 1);
    END LOOP;

    IF suffix THEN
      out := out || '_' || substr(h, 1, 4);
    END IF;
    IF noise THEN
      out := out || substr(h, 5, 2);
    END IF;

    IF punctuate > 0 THEN
      out := substr(out, 1, 2) || '-' || substr(out, 3);
    END IF;
    IF quote > 0 THEN
      out := substr(out, 1, 1) || '"' || substr(out, 2);
    END IF;
    IF space > 0 THEN
      out := substr(out, 1, 3) || ' ' || substr(out, 4);
    END IF;
    IF whitespace > 0 THEN
      out := substr(out, 1, 4) || chr(9) || substr(out, 5);
    END IF;

    IF emote <> 0 THEN
      IF emote > 0 THEN
        out := out || chr(128512);
      ELSE
        out := chr(128512) || out;
      END IF;
    END IF;

    IF diacritics > 0 THEN
      marks := '';
      FOR j IN 1..GREATEST(diacritic_depth, 1) LOOP
        CASE ((j - 1) % LEAST(diacritics, 4))
          WHEN 0 THEN marks := marks || chr(769);  -- U+0301 combining acute
          WHEN 1 THEN marks := marks || chr(768);  -- U+0300 combining grave
          WHEN 2 THEN marks := marks || chr(776);  -- U+0308 combining diaeresis
          ELSE marks := marks || chr(770);         -- U+0302 combining circumflex
        END CASE;
      END LOOP;
      out := regexp_replace(out, '([[:alpha:]])', E'\\1' || marks, 'g');
    END IF;

    IF zalgo THEN
      marks := chr(769) || chr(776) || chr(822);  -- a small "zalgo-ish" stack
      out := regexp_replace(out, '([[:alpha:]])', E'\\1' || marks, 'g');
    END IF;

    RETURN NEXT out;
  END LOOP;
END;
$$;

-- Test 1: query (line 3)
SELECT count(*) FROM crdb_internal.gen_rand_ident('hello', 10);

-- Test 2: query (line 11)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123}');

-- Test 3: query (line 25)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"suffix":false}');

-- Test 4: query (line 40)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"noise":true,"emote":-1}');

-- Test 5: query (line 55)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"noise":false}');

-- Test 6: query (line 69)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"noise":false,"punctuate":1}');

-- Test 7: query (line 83)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"noise":false,"quote":1}');

-- Test 8: query (line 97)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"noise":false,"space":1}');

-- Test 9: query (line 111)
SELECT quote_ident(crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"noise":false,"whitespace":1}'));

-- Test 10: query (line 129)
SELECT crdb_internal.gen_rand_ident('helloworld', 10, '{"seed":123,"noise":false,"emote":1}');

-- Test 11: query (line 143)
SELECT crdb_internal.gen_rand_ident('aeaeiao', 10, '{"seed":123,"noise":false,"diacritics":3,"diacritic_depth":4}');

-- Test 12: query (line 157)
SELECT crdb_internal.gen_rand_ident('aeaeiao', 10, '{"seed":123,"noise":false,"zalgo":true}');
