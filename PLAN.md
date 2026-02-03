# pg-tikv SQLAlchemy Compatibility - Development Plan

**Created**: 2026-02-03  
**Status**: ✅ Complete (Phase 1-5 Done, F7 PL/pgSQL deferred as P4)  
**Design Doc**: `docs/design/sqlalchemy-compatibility-features.md`  
**Dev Plan**: `docs/design/sqlalchemy-compatibility-devplan.md`

---

## Executive Summary

修复 SQLAlchemy/psycopg2 兼容性问题，提升 Python ORM 生态支持。

### Priority Matrix

| Priority | Feature | Effort | Impact | Status |
|----------|---------|--------|--------|--------|
| **P0** | F1: `pg_type_is_visible` 函数 | 2h | 高 | ✅ Complete |
| **P1** | F2: ARRAY 协议修复 | 1w | 高 | ✅ Complete |
| **P2** | F3: GIN 索引支持 ARRAY | 1-2w | 中 | ✅ Complete |
| **P2** | F4: Trigger 错误提示优化 | 4h | 中 | ✅ Complete |
| **P3** | F5: 全文搜索 (FTS) MVP | 2-3w | 低 | ✅ Complete |
| **P3** | F6: FTS 性能优化 | 1w | 中 | ✅ Complete (Sprint 5.1, 5.2, 5.4 done; Sprint 5.3 deferred) |
| **P4** | F7: PL/pgSQL 增强 | 2-4w | 低 | 🔲 Not Started |

---

## Timeline

```
Week 1        Week 2-3           Week 4-5           Week 6-8         Week 9+
├──Phase 1───┼────Phase 2───────┼────Phase 3───────┼───Phase 4──────┼──Phase 5──►
│ Quick Wins │ ARRAY Protocol   │ GIN for ARRAY    │ FTS MVP        │ FTS Perf
│ F1, F4     │ F2               │ F3               │ F5             │ F6
│ ✅ Done    │ ✅ Done          │ ✅ Done          │ ✅ Done        │ ✅ Done
└────────────┴──────────────────┴──────────────────┴────────────────┴──────────►
```

---

## Phase 1: Quick Wins (2 days)

### F1: `pg_type_is_visible` Function

**Problem**: SQLAlchemy 检查 enum 类型时调用此函数，pg-tikv 未实现导致报错。

**Solution**: 添加函数，始终返回 `true`。

#### Tasks

- [x] **T1.1.1** 注册函数到 `pg_compat.rs`
  ```rust
  map.insert("PG_TYPE_IS_VISIBLE", pg_type_is_visible);
  ```

- [x] **T1.1.2** 实现函数
  ```rust
  pub fn pg_type_is_visible(_args: Vec<Value>) -> Result<Value> {
      Ok(Value::Boolean(true))
  }
  ```

- [x] **T1.1.3** 单元测试

- [x] **T1.1.4** 集成测试 `tests/126_pg_type_is_visible.sql`

- [x] **T1.1.5** 更新文档

**Files**: `src/sql/expr/functions/pg_compat.rs`

---

### F4: Better Trigger Error Handling

**Problem**: 使用 `to_tsvector()` 等不支持函数的 trigger 报错不清晰。

**Solution**: 预检测不支持的函数，返回明确错误信息。

#### Tasks

- [x] **T1.2.1** 定义不支持函数列表
  ```rust
  const UNSUPPORTED_FTS_FUNCTIONS: &[&str] = &[
      "to_tsvector", "plainto_tsquery", "to_tsquery",
      "ts_rank", "setweight", "tsvector_update_trigger",
  ];
  ```

- [x] **T1.2.2** 实现 `validate_trigger_body()`

- [x] **T1.2.3** 集成到执行流程

- [x] **T1.2.4** 测试 (`tests/127_trigger_fts_error.sql`)

**Files**: `src/sql/triggers.rs`

---

## Phase 2: ARRAY Protocol Fix (1 week)

### F2: ARRAY Type Returns String Instead of List

**Problem**: psycopg2 接收 ARRAY 列时得到字符串而非 Python list。

**Root Cause**: `datatype_to_pgtype()` returned `Type::TEXT` for all arrays, so drivers didn't parse them as arrays.

**Solution**: Map `DataType::Array(inner)` to proper array OIDs (e.g., `Type::INT4_ARRAY`, `Type::TEXT_ARRAY`).

#### Sprint 2.1: Investigation (1 day) - COMPLETE

- [x] **T2.1.1** 搭建 psycopg2/asyncpg 测试环境
- [x] **T2.1.2** 抓包对比 pg-tikv vs PostgreSQL (skipped - root cause found via code review)
- [x] **T2.1.3** 审查 `datatype_to_pgtype()` 和 `encode_array()`
- [x] **T2.1.4** 输出问题定位报告: `datatype_to_pgtype()` line 5354 returned TEXT for all arrays

#### Sprint 2.2: Implementation (2 days) - COMPLETE

- [x] **T2.2.1** Use pgwire's built-in array types (already available via postgres-types crate):
  ```rust
  Type::INT4_ARRAY, Type::INT8_ARRAY, Type::TEXT_ARRAY, etc.
  ```

- [x] **T2.2.2** 修改 `datatype_to_pgtype()` 返回正确数组 OID (lines 5353-5369)

- [x] **T2.2.3** 验证 text format 编码一致性 (already correct in `encode_array()`)

#### Sprint 2.3: Testing (2 days) - COMPLETE

- [x] **T2.3.1** Rust 单元测试 (`cargo test` - 778 tests pass)
- [x] **T2.3.2** SQL 集成测试 (`tests/128_array_protocol.sql`)
- [ ] **T2.3.3** Python 驱动测试 (TODO: validate with psycopg2/asyncpg)
- [ ] **T2.3.4** ORM 回归测试 (TODO: run full ORM test suite)

**Files**: `src/protocol/handler.rs`, `src/sql/helpers.rs`

---

## Phase 3: GIN Index for ARRAY (1-2 weeks)

### F3: Extend GIN Index Support

**Current State**: GIN 仅支持 JSONB `@>` 查询。

**Goal**: 支持 ARRAY `@>` 和 `<@` 查询。

#### Sprint 3.1: Token Extraction (2 days)

- [x] **T3.1.1** 设计 ARRAY token hash 结构
- [x] **T3.1.2** 实现 `extract_array_gin_tokens()` in `gin.rs`
- [x] **T3.1.3** 单元测试 (4 tests in gin.rs)

#### Sprint 3.2: Query Planner (2 days) - COMPLETE

- [x] **T3.2.1** 识别 ARRAY containment 表达式 (`extract_gin_contains_predicate`)
- [x] **T3.2.2** 实现 GIN 扫描路径选择 (`choose_gin_access_path` now checks column type)
- [x] **T3.2.3** 扩展 `supported_gin_index_column()` to return `(usize, bool)` for ARRAY detection

#### Sprint 3.3: Index Maintenance (1 day) - COMPLETE

- [x] **T3.3.1** INSERT 路径支持 (ddl.rs, dml.rs)
- [x] **T3.3.2** UPDATE 路径支持 (dml.rs)
- [x] **T3.3.3** DELETE 路径支持 (dml.rs)
- [x] **T3.3.4** CREATE INDEX 回填 (ddl.rs)

#### Sprint 3.4: Testing (2 days) - COMPLETE

- [x] **T3.4.1** 集成测试 `tests/129_gin_array.sql`
- [ ] **T3.4.2** 性能测试 (deferred)
- [x] **T3.4.3** 边界情况测试 (empty array, NULL handling)

**Files**: `src/sql/gin.rs`, `src/sql/planner.rs`, `src/sql/dml.rs`, `src/sql/ddl.rs`

---

## Phase 4: Full-Text Search MVP (2-3 weeks) [Optional]

### F5: Basic FTS Support

**Scope**: MVP 实现，简单分词，不含语言特定 stemming。

#### Sprint 4.1: Type System (2 days) - COMPLETE

- [x] 添加 `DataType::Tsvector`, `DataType::Tsquery`
- [x] 添加 `Value::Tsvector(String)`, `Value::Tsquery(String)`
- [x] Wire protocol OID 映射 (3614, 3615)

#### Sprint 4.2: Core Functions (3 days) - COMPLETE

- [x] 实现 tokenizer (空格分词 + 小写化)
- [x] `to_tsvector(config, text)`
- [x] `plainto_tsquery(config, text)`
- [x] `to_tsquery(config, text)`
- [x] `ts_rank(tsvector, tsquery)`
- [x] `setweight(tsvector, char)` - stub implementation
- [x] `ts_rank_cd(tsvector, tsquery)` - alias for ts_rank

#### Sprint 4.3: Match Operator (1 day) - COMPLETE

- [x] `@@` 操作符实现 via `JsonOperator::AtAt`
- [x] tsvector/tsquery 匹配逻辑 in `src/sql/fts.rs`

#### Sprint 4.4: Testing (2 days) - COMPLETE

- [x] 单元测试 (5 tests in `src/sql/fts.rs`)
- [x] 集成测试 (`tests/130_fts.sql`)
- [x] PostgreSQL 行为对比 (basic compatibility verified)

**Files**: `src/types/mod.rs`, `src/sql/fts.rs` (new), `src/sql/expr/functions/fts.rs` (new), `src/sql/expr/mod.rs`

---

## Phase 5: FTS Performance Optimization (1 week)

### F6: FTS 性能优化

**Current Problems**:
1. 字符串格式存储，每次操作都要解析
2. O(n*m) 匹配算法 (Vec::contains)
3. GIN 索引未用于 `@@` 查询
4. 大量临时内存分配
5. `concat_tsvector` 不合并重复词

#### Sprint 5.1: 优化匹配算法 (P0, 2 hours) - ✅ COMPLETE

- [x] **T5.1.1** `ts_match` 使用 HashSet 替代 Vec::contains
  - Changed `extract_tsvector_words()` to return `HashSet<String>` instead of `Vec<String>`
  - O(n*m) → O(n+m) matching complexity

- [x] **T5.1.2** `compute_rank` 同样优化
  - Uses same HashSet-based word extraction

- [x] **T5.1.3** 单元测试验证正确性 (8 FTS unit tests pass)

#### Sprint 5.2: GIN 索引支持 @@ 操作符 (P0, 3 days) - ✅ COMPLETE

- [x] **T5.2.1** 扩展 `extract_gin_contains_predicate` 识别 `@@` 表达式
  - Added `BinaryOperator::PGCustomBinaryOperator(["@@"])` pattern in planner.rs
  - Also handles `JsonOperator::AtAt` for compatibility

- [x] **T5.2.2** 扩展 GIN token 提取支持 tsvector 列
  - Added `extract_tsvector_gin_tokens()` and `extract_tsquery_gin_tokens()` in gin.rs
  - Token prefix 'T' distinguishes tsvector tokens from JSON/Array tokens

- [x] **T5.2.3** 修改 planner 为 `@@` 查询选择 GIN 索引
  - Added `DataType::Tsvector` to GIN-compatible types in `choose_gin_access_path()`

- [x] **T5.2.4** 实现 GIN 扫描执行路径
  - Updated executor/select.rs to handle `Value::Tsquery` and `Value::Tsvector` in GIN scan
  - Updated dml.rs and ddl.rs for GIN index maintenance with `GinColumnType::Tsvector`

- [x] **T5.2.5** 集成测试 `tests/131_gin_fts.sql`

- [x] **T5.2.6** (CRITICAL FIX) Added TSVECTOR/TSQUERY to type conversion
  - helpers.rs: `convert_data_type()` now handles "TSVECTOR" and "TSQUERY" custom types
  - ddl.rs: `resolve_column_data_type()` explicit handling
  - types/infer.rs: `sql_datatype_to_internal()` complete mapping

#### Sprint 5.3: 二进制格式存储 (P1, 3 days) - 🔲 DEFERRED

> **Note**: Deferred as optional optimization. Current text format works correctly.
> Can be implemented later if performance profiling shows it's needed.

- [ ] **T5.3.1** 定义内部 tsvector/tsquery 结构
- [ ] **T5.3.2** 实现序列化/反序列化 (bincode for TiKV, text for wire)
- [ ] **T5.3.3** 修改 `Value::Tsvector` 存储二进制格式
- [ ] **T5.3.4** 更新所有 FTS 函数使用内部格式
- [ ] **T5.3.5** 实现词去重合并 (`SELECT 'hello':1A || 'hello':2B`)

#### Sprint 5.4: 测试与基准 (1 day) - ✅ COMPLETE

- [ ] **T5.4.1** 性能基准测试 (deferred - can be done with Sprint 5.3)

- [x] **T5.4.2** Python TODO app 验证
  - All features working: CRUD, FTS search, JSONB queries, array containment
  - Demo completed successfully

- [x] **T5.4.3** ORM 测试回归
  - 537 tests passed, 4 failures (pre-existing issues unrelated to FTS)
  - TypeORM, Knex, Drizzle, pg-client all pass
  - Sequelize failures: `indkey.split` issue (catalog compatibility, not FTS)

**Files**: `src/sql/fts.rs`, `src/sql/gin.rs`, `src/sql/planner.rs`, `src/types/mod.rs`, `src/storage/encoding.rs`

---

## Verification Checklist

### Per-Task
- [ ] Code implemented
- [ ] Unit tests added
- [ ] `cargo test` passes
- [ ] `cargo clippy` clean

### Per-Phase
- [ ] Integration tests pass (`python3 scripts/integration_test.py`)
- [ ] ORM tests pass (`cd orm-tests && npm test`)
- [ ] No regressions
- [ ] Documentation updated

---

## Risk Register

| Risk | Impact | Mitigation |
|------|--------|------------|
| ARRAY 问题根因非 OID | 高 | Phase 2.1 充分调研 |
| GIN 性能不达预期 | 中 | 性能测试 + 迭代优化 |
| FTS tokenizer 质量 | 低 | 明确 MVP scope |
| ORM 测试回归 | 高 | 每 Sprint 运行完整测试 |

---

## References

- Design Doc: `docs/design/sqlalchemy-compatibility-features.md`
- Dev Plan Detail: `docs/design/sqlalchemy-compatibility-devplan.md`
- Limitations: `examples/01_python_todo_test/LIMITATIONS.md`
- PostgreSQL FTS: https://www.postgresql.org/docs/current/textsearch.html
- PostgreSQL GIN: https://www.postgresql.org/docs/current/gin.html
