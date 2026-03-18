-- #1529: db9-specific guardrail for unsupported DEFERRABLE GUCs.
-- This is intentionally not PostgreSQL-parity behavior: PostgreSQL can mutate
-- these settings, but db9 must reject them because SERIALIZABLE/DEFERRABLE is
-- not implemented.

SELECT 'initial' AS phase,
       current_setting('transaction_deferrable') AS transaction_deferrable,
       current_setting('default_transaction_deferrable') AS default_transaction_deferrable;

-- All write paths must be rejected.
SET transaction_deferrable = on;
SET default_transaction_deferrable = on;
SET LOCAL transaction_deferrable = on;
SET LOCAL default_transaction_deferrable = on;
SELECT set_config('transaction_deferrable', 'on', false);
SELECT set_config('default_transaction_deferrable', 'on', false);

SELECT 'final' AS phase,
       current_setting('transaction_deferrable') AS transaction_deferrable,
       current_setting('default_transaction_deferrable') AS default_transaction_deferrable;
