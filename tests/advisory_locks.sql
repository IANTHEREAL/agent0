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

-- ============================================================
-- Mid-savepoint assertion tests (keys 5050–5069)
-- Probes lock state DURING the transaction using pg_advisory_unlock
-- return values and pg_try_advisory_xact_lock re-acquisition.
-- ============================================================

-- Case 7: Xact lock scope validation — pg_advisory_unlock returns false for xact locks
-- even when they are held, confirming scope separation.
BEGIN;
SELECT pg_advisory_xact_lock(5050);
SAVEPOINT s1;
SELECT pg_advisory_xact_lock(5051);
-- Mid-savepoint: pg_advisory_unlock returns false for xact-scoped locks
SELECT pg_advisory_unlock(5050);
SELECT pg_advisory_unlock(5051);
ROLLBACK TO SAVEPOINT s1;
-- After rollback: 5051 released, 5050 still held
-- Re-acquire 5051 to exercise the released-lock path
SELECT pg_try_advisory_xact_lock(5051);
-- pg_advisory_unlock still returns false (xact scope, not session)
SELECT pg_advisory_unlock(5050);
SELECT pg_advisory_unlock(5051);
COMMIT;
-- Post-commit: all released
BEGIN;
SELECT pg_try_advisory_xact_lock(5050);
SELECT pg_try_advisory_xact_lock(5051);
COMMIT;

-- Case 8: Session-level mid-savepoint probe via pg_advisory_unlock return value.
-- pg_advisory_unlock returns true when a session lock is held, false when not.
-- This provides a genuine observable assertion mid-transaction.
BEGIN;
SELECT pg_advisory_lock(5060);
SAVEPOINT s1;
SELECT pg_advisory_lock(5061);
-- Mid-savepoint: verify both session locks are held
SELECT pg_advisory_unlock(5060);
SELECT pg_advisory_lock(5060);
SELECT pg_advisory_unlock(5061);
SELECT pg_advisory_lock(5061);
ROLLBACK TO SAVEPOINT s1;
-- Session locks survive savepoint rollback — a buggy impl that releases
-- session locks on rollback would return false here
SELECT pg_advisory_unlock(5060);
SELECT pg_advisory_unlock(5061);
COMMIT;

-- Case 9: Mixed xact + session locks on same key — savepoint interaction
BEGIN;
SELECT pg_advisory_xact_lock(5070);
SELECT pg_advisory_lock(5070);
SAVEPOINT s1;
SELECT pg_advisory_xact_lock(5071);
SELECT pg_advisory_lock(5071);
-- Mid-savepoint: session locks for both keys are held
SELECT pg_advisory_unlock(5070);
SELECT pg_advisory_lock(5070);
SELECT pg_advisory_unlock(5071);
SELECT pg_advisory_lock(5071);
ROLLBACK TO SAVEPOINT s1;
-- After rollback: xact lock 5071 released, session locks survive
-- pg_advisory_unlock returns true for session lock on 5071 (still held)
SELECT pg_advisory_unlock(5071);
-- pg_advisory_unlock returns true for session lock on 5070 (still held)
SELECT pg_advisory_unlock(5070);
COMMIT;
-- Post-commit: xact locks released; session locks were cleaned up above
BEGIN;
SELECT pg_try_advisory_xact_lock(5070);
SELECT pg_try_advisory_xact_lock(5071);
COMMIT;
