-- Explicit type mismatch must be rejected at PREPARE time.
-- PostgreSQL 17.9: ERROR: function lower(integer) does not exist
PREPARE prepare_bad(int) AS SELECT lower($1);
