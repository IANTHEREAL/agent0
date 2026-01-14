# TODO

## Test Status (2025-01-12)
- ✅ Integration tests: **6/6 passed**
- ⚠️ ORM tests: **428 passed, 11 failed** (down from ~100 failures)

---

## COMPLETED ✅

### P1: ArrayAgg support - DONE
- [x] Added `ArrayAgg` to Aggregator enum
- [x] Updated executor to detect and handle `Expr::ArrayAgg`
- [x] Added `pg_get_indexdef` stub function

### DISTINCT ON - DONE
- [x] Fixed DISTINCT ON executing after projection

### Scalar Subquery - DONE
- [x] Fixed correlated scalar subquery substitution
- [x] Fixed COUNT(*) returning empty for no-match
- [x] Fixed ORDER BY on correlated subquery aliases

### Prisma - DONE
- [x] Extended Query parameter binding works

---

## HIGH ROI - Fix These First

### P1b: Sequelize pg_catalog tables [~38 tests] ⭐⭐⭐
**Impact**: ALL Sequelize tests fail during sync
**Error**: `Cannot read properties of null (reading '1')`
**Cause**: Sequelize's `showIndex` query joins pg_class/pg_index/pg_attribute, expects real data
**Fix**: Improve pg_catalog virtual tables to return index metadata

### P2: TypeORM information_schema column resolution [~42 tests] ⭐⭐⭐
**Impact**: TypeORM error/query/relation/types suites fail
**Error**: `Column 'columns.table_name' not found`
**Cause**: TypeORM queries `information_schema.columns` with LEFT JOIN to `pg_catalog.pg_attribute`
**Fix**: Ensure column alias resolution works in nested subquery context

### P3: VIEW Ordering [3 tests] ⭐⭐
**Impact**: Knex/Sequelize/Drizzle view tests
**Error**: `expected 'Sales' to be 'Engineering'`
**Cause**: VIEW query ORDER BY not being applied correctly

### P4: ON CONFLICT (upsert) [1 test] ⭐⭐
**Impact**: Knex upsert test
**Error**: `expected 'Original' to be 'Updated'`
**Cause**: `INSERT ... ON CONFLICT ... DO UPDATE` not updating correctly

---

## MEDIUM ROI

### P5: CTE query issues [2 tests]
**Impact**: pg-client/Knex CTE tests
**Error**: `Unsupported select item` / undefined results
**Cause**: Complex CTE with multiple WITH clauses

### P6: Knex connection database name [1 test]
**Error**: `expected undefined to be 'postgres'`
**Cause**: Database name not returned in connection info

### P7: vector_dims return type [1 test]
**Error**: `expected '3' to be 3`
**Cause**: `vector_dims()` returns string instead of integer

---

## LOW ROI (Future)

### P8: SAVEPOINT [3 tests]
**Impact**: TypeORM/Drizzle nested transaction tests
**Error**: `Unsupported statement: Savepoint`
- [ ] Implement SAVEPOINT name
- [ ] Implement ROLLBACK TO SAVEPOINT name
- [ ] Implement RELEASE SAVEPOINT name

### P9: TIME data type [1 test]
**Error**: `Unsupported data type: Time(None, None)`
- [ ] Add TIME type to DataType enum
- [ ] Add parsing and storage support

---

## Tests and Coverage
- [x] Unify test entrypoint: `run_tests.sh`
- [x] Add CI workflow: `.github/workflows/orm-tests.yml`
- [ ] Add target identity check: verify server is pg-tikv before tests
- [ ] Establish Rust coverage: `cargo llvm-cov`

## Future SQL Features
- [ ] LATERAL JOIN
- [ ] Composite types (CREATE TYPE)
- [ ] Advanced array ops (ANY/ALL, unnest)
- [ ] Regex functions/operators
- [ ] Timezone handling (AT TIME ZONE)
