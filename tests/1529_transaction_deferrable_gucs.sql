-- #1529: transaction_deferrable / default_transaction_deferrable must not be faked.
-- db9 does not support SERIALIZABLE, so deferrable is always "off" and SET is rejected.

-- SHOW returns "off" for both GUCs.
SHOW transaction_deferrable;
SHOW default_transaction_deferrable;

-- current_setting also returns "off".
SELECT current_setting('transaction_deferrable') AS transaction_deferrable;
SELECT current_setting('default_transaction_deferrable') AS default_transaction_deferrable;

-- SET must be rejected (SQLSTATE 55P02).
SET transaction_deferrable = on;
SET default_transaction_deferrable = on;

-- SET LOCAL must also be rejected.
SET LOCAL transaction_deferrable = on;
SET LOCAL default_transaction_deferrable = on;

-- set_config must also be rejected.
SELECT set_config('transaction_deferrable', 'on', false);
SELECT set_config('default_transaction_deferrable', 'on', false);

-- After all rejections, SHOW must still return "off".
SHOW transaction_deferrable;
SHOW default_transaction_deferrable;
