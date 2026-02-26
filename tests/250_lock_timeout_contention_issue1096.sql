-- Regression coverage for issue #1096 / PR #1119:
-- lock_timeout must fire (55P03) on contended SELECT ... FOR UPDATE.

DROP TABLE IF EXISTS lock_timeout_contended_1096;
CREATE TABLE lock_timeout_contended_1096 (id INT PRIMARY KEY, v INT);
INSERT INTO lock_timeout_contended_1096 VALUES (1, 10);

-- Session B pre-locks the advisory gate so Session A starts deterministically.
SELECT pg_advisory_lock(91096);

-- Session A (separate connection):
-- BEGIN; SELECT ... FOR UPDATE; hold lock; ROLLBACK.
\! psql -X -q -h ${DB9_TEST_HOST:-127.0.0.1} -p ${DB9_TEST_PORT:-5433} -U ${DB9_TEST_USER:-admin} -d ${DB9_TEST_DB:-postgres} -c "BEGIN; SELECT pg_advisory_lock(91096); SELECT * FROM lock_timeout_contended_1096 WHERE id = 1 FOR UPDATE; SELECT pg_advisory_unlock(91096); SELECT pg_sleep(3); ROLLBACK;" >/dev/null 2>&1 &

-- Release the gate and wait until Session A has held-and-released it, which
-- only happens after FOR UPDATE lock acquisition.
SELECT pg_advisory_unlock(91096);
DO $$
DECLARE
    saw_session_a BOOLEAN := FALSE;
BEGIN
    FOR i IN 1..1000 LOOP
        IF pg_try_advisory_lock(91096) THEN
            PERFORM pg_advisory_unlock(91096);
            IF saw_session_a THEN
                RETURN;
            END IF;
        ELSE
            saw_session_a := TRUE;
        END IF;
        PERFORM pg_sleep(0.01);
    END LOOP;
    RAISE EXCEPTION 'session A did not reach FOR UPDATE lock stage in time';
END $$;

-- Session B: lock_timeout + BEGIN + contended FOR UPDATE.
SET lock_timeout = '100ms';
BEGIN;
DO $$
BEGIN
    BEGIN
        PERFORM 1 FROM lock_timeout_contended_1096 WHERE id = 1 FOR UPDATE;
        RAISE EXCEPTION 'expected lock timeout on contended FOR UPDATE';
    EXCEPTION
        WHEN lock_not_available THEN
            IF SQLSTATE <> '55P03' THEN
                RAISE EXCEPTION 'unexpected SQLSTATE: %', SQLSTATE;
            END IF;
            IF SQLERRM <> 'canceling statement due to lock timeout' THEN
                RAISE EXCEPTION 'unexpected lock timeout message: %', SQLERRM;
            END IF;
    END;
END $$;
ROLLBACK;
RESET lock_timeout;

DROP TABLE lock_timeout_contended_1096;
