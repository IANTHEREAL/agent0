# SQL Module

SQL parsing, planning, and execution.

This directory was refactored into submodules to keep responsibilities clearer:

## Layout (high level)

| Path | Purpose |
|------|---------|
| `src/sql/executor/` | Statement execution (SELECT/DDL/DML/CTE/subquery plumbing) |
| `src/sql/expr/` | Expression evaluation (contexts, operators, built-in functions) |
| `src/sql/operators/` | Physical operators (scan/filter/project/sort/aggregate/joins) |
| `src/sql/types/` | Type inference + coercion helpers |
| `src/sql/planner.rs` | Lightweight planning (hash join selection, access paths) |
| `src/sql/wildcard.rs` | Shared `SELECT *` expansion for USING/NATURAL joins |

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `src/sql/expr/functions/` + type inference in `src/sql/types/infer.rs` |
| Add expression/operator eval | `src/sql/expr/evaluator.rs` + `src/sql/expr/operators.rs` |
| Modify SELECT / JOIN behavior | `src/sql/executor/select/` + `src/sql/operators/` |
| Hash join planning/execution | `src/sql/planner.rs` + `src/sql/operators/hash_join.rs` |
| USING/NATURAL `SELECT *` shaping | `src/sql/wildcard.rs` + `src/sql/executor/select/` |
| DDL/DML behavior | `src/sql/ddl.rs` / `src/sql/dml.rs` (and `src/sql/executor/` wrappers) |

## Tests

| Kind | Location |
|------|----------|
| Unit tests | `src/sql/**/tests.rs` and module `#[cfg(test)]` blocks |
| SQL regressions | `tests/*.sql` + `tests/*.expected` |

## Commands

```bash
cargo test
./scripts/regression_gate.sh --skip-orm
```
