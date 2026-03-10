-- SET LOCAL reserved pseudo-GUC outside transaction: warning + error ordering (PG parity)
-- PostgreSQL emits WARNING before ERROR for SET LOCAL outside a transaction block.

-- 1) SET LOCAL is_superuser = 'on' outside txn: WARNING then ERROR
SET LOCAL is_superuser = 'on';

-- 2) SET LOCAL is_superuser TO DEFAULT outside txn: WARNING then ERROR
SET LOCAL is_superuser TO DEFAULT;

-- 3) SET LOCAL session_authorization = 'evil' outside txn: WARNING then ERROR
SET LOCAL session_authorization = 'evil_user';

-- 4) SET LOCAL session_authorization TO DEFAULT outside txn: WARNING + SET (reset allowed)
SET LOCAL session_authorization TO DEFAULT;
