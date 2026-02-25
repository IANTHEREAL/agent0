-- DO $$ ... $$ anonymous PL/pgSQL blocks (Prisma migration compat, #1059)

-- Minimal block with RAISE NOTICE
DO $$ BEGIN RAISE NOTICE 'hello from DO'; END $$;

-- With DECLARE
DO $$ DECLARE x int := 42; BEGIN RAISE NOTICE 'x = %', x; END $$;

-- Tagged dollar-quote
DO $body$ BEGIN RAISE NOTICE 'tagged'; END $body$;

-- With explicit LANGUAGE
DO LANGUAGE plpgsql $$ BEGIN RAISE NOTICE 'explicit lang'; END $$;

-- Prisma pattern: conditional enum creation
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'do_test_enum') THEN
    CREATE TYPE "do_test_enum" AS ENUM ('A', 'B');
  END IF;
END $$;

-- Verify enum was created
SELECT typname FROM pg_type WHERE typname = 'do_test_enum';

-- Idempotent re-run (Prisma runs migrations multiple times)
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'do_test_enum') THEN
    CREATE TYPE "do_test_enum" AS ENUM ('A', 'B');
  END IF;
END $$;

-- Verify still exists (not duplicated)
SELECT typname FROM pg_type WHERE typname = 'do_test_enum';

-- Cleanup
DROP TYPE IF EXISTS "do_test_enum";
