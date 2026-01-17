# Code Review Summary

## Overall Assessment

This is a well-structured feature implementation that adds metadata-only support for partial indexes, expression indexes, and non-btree index methods (GIN/GIST). The changes follow the MVP scope outlined in the design document: DDL succeeds and introspection works, but the planner correctly ignores these unsupported index types.

---

## Issues Found

### 1. Fixed: `PG_GET_INDEXDEF` now respects its OID argument (Resolved)

**Location:** `src/sql/sequences.rs` (`replace_sequence_functions` / `replace_sequence_functions_join`)

**What changed:** `pg_get_indexdef(oid)` is now rewritten during the existing async expression rewrite phase (the same mechanism used for `nextval/currval/setval`):
- Extracts the OID from arg0 (`int32/int64/float/text(numeric)`).
- Fast path: if the current row contains both `indexrelid` and `indexdef` and `indexrelid == oid`, returns `indexdef` (avoids a catalog scan when selecting from `pg_index`).
- Fallback: looks up the index definition by scanning table schemas and mirroring the deterministic OID assignment used by `pg_catalog.pg_index` in `src/sql/information_schema.rs::get_pg_index_rows`.
- Rewrites to a literal string expression so `src/sql/expr.rs` evaluation remains sync.

**Regression coverage:** `tests/50_index_features.sql` now asserts `PG_GET_INDEXDEF_STANDALONE=...` works without a `pg_index` row context.

---

### 2. Observation: Duplicated logic for determining "should materialize" (Low Severity - Code Quality)

**Location:** `src/sql/executor_ddl_ops.rs` (lines 114-124) and `src/sql/ddl.rs` (lines 939-947)

Both files have nearly identical logic:

```rust
// executor_ddl_ops.rs:114-124
let is_btree = using
    .map(|u| u.value.eq_ignore_ascii_case("btree"))
    .unwrap_or(true);
let has_expr_columns = columns.iter().any(|c| { ... });
let should_materialize = is_btree && predicate.is_none() && !has_expr_columns;

// ddl.rs:939-947
let is_btree = new_index
    .method
    .as_deref()
    .map(|m| m.eq_ignore_ascii_case("btree"))
    .unwrap_or(true);
let should_materialize = is_btree
    && new_index.predicate.is_none()
    && new_index.expressions.is_empty()
    && !new_index.columns.is_empty();
```

The duplication works correctly but increases maintenance burden. The `executor_ddl_ops.rs` version determines whether to scan rows *before* calling `ddl::execute_create_index`, while `ddl.rs` re-checks to decide whether to materialize. The redundancy is intentional (to avoid scanning rows when unnecessary), but could be refactored to a shared helper.

---

### 3. Minor: `pg_am` rows missing from test assertion sort order (Cosmetic)

**Location:** `tests/50_index_features.assert` lines 5-6

The assertion expects:
```
AM=783:gist
AM=2742:gin
```

But `get_pg_am_rows()` returns them in OID order: 403 (btree), 405 (hash), 783 (gist), 2742 (gin), 4000 (spgist), 3580 (brin).

The test query uses `ORDER BY oid`, so the expected output appears incomplete (only showing gist and gin). This suggests the test may be filtering or the assertion only captures partial output. Not a bug, just potentially confusing if someone adds more assertions later.

---

## No Issues Found With:

1. **Type extension** (`IndexDef` with `#[serde(default)]`) - Backward compatible with existing serialized data
2. **Planner filtering** (`is_planner_usable_index`) - Correctly excludes unsupported indexes
3. **Index introspection** (`format_indexdef`) - Correctly formats all index types including predicates and expressions
4. **Access method OID mapping** - Standard PostgreSQL OIDs used
5. **Expression column handling** - Correctly uses `0` in `indkey` for expression columns per PostgreSQL semantics

---

## Summary

| Severity | Count | Summary |
|----------|-------|---------|
| **Medium** | 0 | — |
| **Low** | 1 | Duplicated materialization logic (code quality) |
| **Info** | 1 | Test assertion potentially incomplete for `pg_am` |

The implementation achieves its stated MVP goal and the tests pass. The main correctness gap identified during review (`pg_get_indexdef` argument handling) is now addressed; remaining items are code-quality/cosmetic follow-ups.
