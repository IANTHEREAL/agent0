# TODO

## ORM Test Fixes (112 failing tests)

### P0: DISTINCT ON Crash [5 tests] - HIGH ROI
- [ ] Fix server crash when parsing `DISTINCT ON (col)` syntax
- [ ] Return proper "unsupported" error instead of connection termination

### P1: Prisma Extended Query Parameter Binding [70 tests] - HIGH ROI
- [ ] Fix `IncorrectNumberOfParameters { expected: 0, actual: N }` error
- [ ] Review `ExtendedQueryHandler::on_bind` in handler.rs
- [ ] Ensure prepared statements store correct parameter count
- [ ] Verify parameter binding type conversion

### P2: Scalar Subquery in SELECT [5 tests] - MEDIUM ROI
- [ ] Fix `(SELECT COUNT(*) FROM t)` returning 0 in SELECT list
- [ ] Check correlated subquery context passing

### P3: EXISTS Subquery [5 tests] - MEDIUM ROI
- [ ] Fix `WHERE EXISTS (SELECT ...)` returning empty results
- [ ] Debug EXISTS evaluation logic

### P4: VIEW Ordering [2 tests] - MEDIUM ROI
- [ ] Fix VIEW query result ordering (wrong order returned)
- [ ] Ensure ORDER BY propagates through VIEW execution

### P5: Undefined Value Issues [8 tests] - MEDIUM ROI
- [ ] Fix CTE query returning undefined
- [ ] Fix INSERT RETURNING undefined
- [ ] Fix aggregate query undefined cases

### P6: SAVEPOINT [3 tests] - LOW ROI (Future)
- [ ] Implement SAVEPOINT name
- [ ] Implement ROLLBACK TO name
- [ ] Extend session transaction management

---

## Tests and Coverage
- [x] Unify test entrypoint: `run_tests.sh` as primary with report generation
- [ ] Add target identity check: verify the server is pg-tikv before tests
- [ ] Establish Rust coverage tooling: prefer `cargo llvm-cov`
- [ ] ORM coverage: document `npm test -- --coverage`
- [x] Add CI workflow: `.github/workflows/orm-tests.yml`

## Test Enhancements (Future)
- [ ] ORM migration tests (TypeORM/Prisma/Sequelize)
- [ ] Concurrency/race tests (concurrent inserts, `SELECT FOR UPDATE`)
- [ ] Bulk operation performance tests

## SQL Feature Gaps (Future)
- [ ] LATERAL JOIN
- [ ] Composite types (CREATE TYPE)
- [ ] Advanced array ops (ANY/ALL, unnest, array_agg)
- [ ] Regex functions/operators
- [ ] Timezone handling (AT TIME ZONE)
