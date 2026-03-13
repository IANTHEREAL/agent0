-- DB9_DIVERGENCE(#1806): fs9 SQL scalar contract for strict UTF-8 text reads and bytea-safe reads.
CREATE EXTENSION IF NOT EXISTS fs9;

SELECT
    CASE
        WHEN fs9_exists('/test_fs9_sql_scalar_contract.bin') THEN
            fs9_remove('/test_fs9_sql_scalar_contract.bin')
        ELSE 0
    END AS setup_cleanup_existing;

SELECT
    fs9_write(
        '/test_fs9_sql_scalar_contract.bin',
        decode('00ff6162', 'hex')
    ) AS write_binary;

SELECT
    encode(fs9_read_bytea('/test_fs9_sql_scalar_contract.bin'), 'hex') AS read_binary_hex;

SELECT
    encode(fs9_read_at_bytea('/test_fs9_sql_scalar_contract.bin', 1, 2), 'hex')
        AS read_binary_slice_hex;

SELECT fs9_read('/test_fs9_sql_scalar_contract.bin');

SELECT fs9_read_at('/test_fs9_sql_scalar_contract.bin', 1, 2);

SELECT
    CASE
        WHEN fs9_exists('/test_fs9_sql_scalar_contract.bin') THEN
            fs9_remove('/test_fs9_sql_scalar_contract.bin')
        ELSE 0
    END AS cleanup_after;
