# SQL Integration Tests

This directory contains SQL integration tests executed by `scripts/integration_test.py`.

## Output checking

- `*.expected`: exact output match (after normalization).
- `*.assert`: substring assertions (each non-comment line must appear in output).
- `*.errors`: allow-listed error substrings (for negative tests).

## Dify-lite compatibility gate

This repo includes a small, hermetic **Dify-lite** workload derived from upstream Dify behavior:

- `tests/96_dify_schema.sql`: restore smoke for a real Dify `pg_dump` schema into an isolated database (`dify_compat_96`).
- `tests/127_dify_lite_workload.sql`: runs deterministic inserts + representative queries on that restored schema and then drops `dify_compat_96` to keep the shared test environment clean.

Covered patterns (see issue #332 Coverage Map for upstream refs):
- Tenant/account membership joins (`accounts`, `tenants`, `tenant_account_joins`)
- Timezone day bucketing (`DATE(DATE_TRUNC('day', ts AT TIME ZONE 'UTC' AT TIME ZONE tz))`)
- Keyword search across JSON arrays (`jsonb_array_elements_text(...)` + `ILIKE ... ESCAPE`)
- Plugin daemon init migration shape (`ALTER COLUMN ... DROP NOT NULL` + `information_schema` introspection)

To run just the Dify-lite pair:

```bash
python3 scripts/integration_test.py --dsn "$PG_DSN" tests/96_dify_schema.sql tests/127_dify_lite_workload.sql
```

## DB9 cop pushdown regression pack

The DB9 cop pushdown golden pack currently includes `tests/550_pushdown_seq_scan.sql`
through `tests/557_pushdown_prefix_access.sql`, plus `tests/566_pushdown_datetime_types.sql`
through `tests/577_pushdown_update_delete_txn_visibility.sql`.

It covers:

- seq scan pushdown
- exact secondary-index lookup
- in-list secondary-index lookup
- projection pruning
- bounded range + pushed limit
- function-whitelist projection parity
- unsupported-expression fallback parity
- composite-index prefix access detail
- composite-index prefix + bounded-range access detail
- verbose and non-verbose PG-shaped EXPLAIN annotations
- secondary-index row fetch with DB9 Cop access/filter/output detail
- same-transaction UPDATE/DELETE dirty-table visibility under pushdown

Run the full pack with:

```bash
bash scripts/db9_cop_pushdown_regression.sh
```

To run the same pack through the repository's fast-gate wrapper:

```bash
bash scripts/db9_cop_pushdown_gate.sh
```

The main `scripts/regression_gate.sh` wrapper skips these pushdown-specific cases
unless `DB9_RUN_COP_PUSHDOWN_TESTS=1` is explicitly set.
