-- Advisory lock functions (PostgreSQL parity)

-- Basic session lock acquire/release
SELECT pg_advisory_lock(12345);
SELECT pg_advisory_unlock(12345);

-- Try variants return boolean
SELECT pg_try_advisory_lock(12345);
SELECT pg_advisory_unlock(12345);

-- Two-int overload
SELECT pg_advisory_lock(1, 2);
SELECT pg_advisory_unlock(1, 2);

-- Stacking: must unlock same number of times
SELECT pg_advisory_lock(100);
SELECT pg_advisory_lock(100);
SELECT pg_advisory_unlock(100);
SELECT pg_advisory_unlock(100);

-- Unlock non-held returns false
SELECT pg_advisory_unlock(99999);

-- Transaction-level locks auto-release on COMMIT
BEGIN;
SELECT pg_advisory_xact_lock(200);
COMMIT;

-- Transaction-level shared locks auto-release on COMMIT
BEGIN;
SELECT pg_advisory_xact_lock_shared(201);
COMMIT;

-- Transaction-level locks auto-release on ROLLBACK
BEGIN;
SELECT pg_advisory_xact_lock(202);
ROLLBACK;

-- Try xact variants
BEGIN;
SELECT pg_try_advisory_xact_lock(203);
SELECT pg_try_advisory_xact_lock_shared(204);
COMMIT;

-- pg_advisory_unlock_all releases all session locks
SELECT pg_advisory_lock(1000);
SELECT pg_advisory_lock(1001);
SELECT pg_advisory_unlock_all();
SELECT pg_advisory_unlock(1000);

-- NULL handling: void functions return empty, bool functions return NULL
SELECT pg_advisory_lock(NULL::bigint);
SELECT pg_try_advisory_lock(NULL::bigint);
SELECT pg_advisory_unlock(NULL::bigint);

-- Shared lock compatibility (same session)
SELECT pg_advisory_lock_shared(300);
SELECT pg_advisory_lock_shared(300);
SELECT pg_advisory_unlock_shared(300);
SELECT pg_advisory_unlock_shared(300);

-- Same-session mixed mode: exclusive then shared
SELECT pg_advisory_lock(400);
SELECT pg_try_advisory_lock_shared(400);
SELECT pg_advisory_unlock(400);
SELECT pg_advisory_unlock_shared(400);

-- Same-session mixed mode: shared then exclusive
SELECT pg_advisory_lock_shared(401);
SELECT pg_try_advisory_lock(401);
SELECT pg_advisory_unlock_shared(401);
SELECT pg_advisory_unlock(401);

-- Prisma migration pattern
SELECT pg_advisory_lock(72707369);
SELECT pg_advisory_unlock(72707369);

-- ============================================================
-- ROLLBACK TO SAVEPOINT + advisory lock interaction tests
-- (single-session indirect smoke checks; keys 5000–5040)
-- ============================================================

-- Case 1: xact lock acquired after savepoint is released on ROLLBACK TO SAVEPOINT
BEGIN;
SAVEPOINT s1;
SELECT pg_advisory_xact_lock(5000);
ROLLBACK TO SAVEPOINT s1;
-- Lock 5000 was released by rollback-to-savepoint; commit should succeed cleanly
COMMIT;
-- Verify no leaked state: new txn can acquire same key
BEGIN;
SELECT pg_try_advisory_xact_lock(5000);
COMMIT;

-- Case 2: Lock acquired BEFORE savepoint survives ROLLBACK TO SAVEPOINT
BEGIN;
SELECT pg_advisory_xact_lock(5001);
SAVEPOINT s1;
SELECT pg_advisory_xact_lock(5002);
ROLLBACK TO SAVEPOINT s1;
-- 5001 still held (acquired before savepoint), 5002 released
-- Both are cleaned up on COMMIT
COMMIT;
-- Verify both keys are free after commit
BEGIN;
SELECT pg_try_advisory_xact_lock(5001);
SELECT pg_try_advisory_xact_lock(5002);
COMMIT;

-- Case 3: Nested savepoints — rollback to outer releases locks from both
BEGIN;
SAVEPOINT s1;
SELECT pg_advisory_xact_lock(5010);
SAVEPOINT s2;
SELECT pg_advisory_xact_lock(5011);
ROLLBACK TO SAVEPOINT s1;
-- Both 5010 and 5011 released (5010 was in s1's frame, 5011 in s2's)
COMMIT;
BEGIN;
SELECT pg_try_advisory_xact_lock(5010);
SELECT pg_try_advisory_xact_lock(5011);
COMMIT;

-- Case 4: Shared xact lock and try variant also released on ROLLBACK TO SAVEPOINT
BEGIN;
SAVEPOINT s1;
SELECT pg_advisory_xact_lock_shared(5020);
SELECT pg_try_advisory_xact_lock(5021);
ROLLBACK TO SAVEPOINT s1;
COMMIT;
BEGIN;
SELECT pg_try_advisory_xact_lock(5020);
SELECT pg_try_advisory_xact_lock(5021);
COMMIT;

-- Case 5: RELEASE SAVEPOINT does NOT release locks (they merge to parent scope)
BEGIN;
SAVEPOINT s1;
SELECT pg_advisory_xact_lock(5030);
RELEASE SAVEPOINT s1;
-- Lock 5030 is still held (merged to transaction scope)
COMMIT;
-- After commit, lock is released
BEGIN;
SELECT pg_try_advisory_xact_lock(5030);
COMMIT;

-- Case 6: Session-level locks are NOT affected by ROLLBACK TO SAVEPOINT
BEGIN;
SAVEPOINT s1;
SELECT pg_advisory_lock(5040);
ROLLBACK TO SAVEPOINT s1;
-- Session lock 5040 survives savepoint rollback (session locks ignore txn semantics)
SELECT pg_advisory_unlock(5040);
COMMIT;
