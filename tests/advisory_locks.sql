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
