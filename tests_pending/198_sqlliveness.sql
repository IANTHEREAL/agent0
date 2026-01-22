-- PostgreSQL compatible tests from sqlliveness
-- 4 tests

-- CockroachDB exposes SQL liveness via system tables/functions. PostgreSQL does
-- not have an equivalent subsystem, so model the minimal behavior this test
-- needs (alive iff a row exists and has not expired).
CREATE SCHEMA IF NOT EXISTS crdb_internal;
CREATE SCHEMA IF NOT EXISTS system;

CREATE TABLE system.sqlliveness (
  session_id bytea PRIMARY KEY,
  expires_at timestamptz NOT NULL
);

INSERT INTO system.sqlliveness (session_id, expires_at) VALUES
  (decode('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'hex'), 'infinity'),
  (decode('bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'hex'), 'epoch');

CREATE OR REPLACE FUNCTION crdb_internal.sql_liveness_is_alive(id bytea)
RETURNS boolean
LANGUAGE SQL
STABLE
AS $$
  SELECT EXISTS (
    SELECT 1
    FROM system.sqlliveness s
    WHERE s.session_id = id
      AND s.expires_at > now()
  );
$$;

-- Test 1: query (line 6)
select crdb_internal.sql_liveness_is_alive(decode('1f915e98f96145a5baa9f3a42c378eb6', 'hex'));

-- Test 2: query (line 12)
select crdb_internal.sql_liveness_is_alive(decode('deadbeef', 'hex'));

-- Test 3: query (line 23)
SELECT count(*) FROM system.sqlliveness WHERE crdb_internal.sql_liveness_is_alive(session_id) = false;

-- Test 4: query (line 28)
SELECT count(*) > 0 FROM system.sqlliveness WHERE crdb_internal.sql_liveness_is_alive(session_id) = true;
