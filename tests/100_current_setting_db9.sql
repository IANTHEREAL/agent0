-- db9-specific current_setting() extensions (not PG-validated)

-- db9 alias: transaction.isolation.level → transaction_isolation
SELECT current_setting('transaction.isolation.level') AS txn_iso;

-- Surface contract: tableless current_setting() masks sensitive GUCs too.
SET embedding.api_key = 'sk-secret-1234';
SELECT current_setting('embedding.api_key') AS masked_api_key;
