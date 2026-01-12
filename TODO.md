# TODO

## Tests and Coverage
- [ ] Unify test entrypoint: make `scripts/integration_test.py` primary; fix or retire `run_tests.sh` (port/env/comments inconsistent)
- [ ] Add target identity check: verify the server is pg-tikv before tests (avoid hitting local PostgreSQL)
- [ ] Establish Rust coverage tooling: prefer `cargo llvm-cov`, with `cargo tarpaulin` / `grcov` as alternatives
- [ ] ORM coverage: document `npm test -- --coverage` and publish `orm-tests/coverage/`
- [ ] Add CI workflow (no `.github/workflows` in repo): `cargo test` + SQL integration + ORM tests + coverage reports
- [ ] Fix docs: `orm-tests/README.md` claims `.github/workflows/orm-tests.yml` exists but it does not

## Test Enhancements (High Priority)
- [ ] ORM migration tests (TypeORM/Prisma/Sequelize)
- [ ] Concurrency/race tests (concurrent inserts, `SELECT FOR UPDATE`, lost updates)
- [ ] Bulk operation performance tests (bulk insert/update)

## SQL Test Enhancements (Medium Priority)
- [ ] LATERAL JOIN
- [ ] Composite types (CREATE TYPE)
- [ ] Advanced array ops (ANY/ALL, unnest, array_agg)
- [ ] Advanced indexes (partial/expression)
- [ ] Regex functions/operators
- [ ] Timezone handling (TIMESTAMPTZ/AT TIME ZONE)
- [ ] Generated columns (GENERATED ALWAYS AS)

## Performance/Stress Tests
- [ ] Large dataset tests (100k+ rows)
- [ ] Transaction throughput benchmarks (TPS)
