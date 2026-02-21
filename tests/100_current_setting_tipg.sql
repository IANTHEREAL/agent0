-- tipg-specific current_setting() extensions (not PG-validated)

-- tipg alias: transaction.isolation.level → transaction_isolation
SELECT current_setting('transaction.isolation.level') AS txn_iso;
