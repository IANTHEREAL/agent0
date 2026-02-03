# SQLAlchemy Compatibility - Development Plan

**Date**: 2026-02-03  
**Version**: 1.0  
**Related Design Doc**: `docs/design/sqlalchemy-compatibility-features.md`

---

## Overview

本开发计划涵盖 6 个功能特性的实现，按优先级和依赖关系组织成 4 个阶段。

### Timeline Summary

```
Week 1      Week 2      Week 3      Week 4      Week 5      Week 6
├──Phase 1──┼──Phase 2──────────────┼──Phase 3──────────────┼──Phase 4──►
│ Quick     │ ARRAY Protocol Fix   │ GIN for ARRAY         │ FTS MVP
│ Wins      │                      │                       │
└───────────┴──────────────────────┴───────────────────────┴──────────►
```

---

## Phase 1: Quick Wins (Week 1)

**目标**: 快速修复高价值低成本问题，立即改善 ORM 兼容性

### Sprint 1.1: `pg_type_is_visible` Function

| 属性 | 值 |
|------|-----|
| **优先级** | P0 |
| **预估工时** | 2 小时 |
| **负责人** | TBD |
| **前置依赖** | 无 |

#### Tasks

- [ ] **T1.1.1** 在 `pg_compat.rs` 添加函数注册 (15 min)
  ```rust
  map.insert("PG_TYPE_IS_VISIBLE", pg_type_is_visible);
  ```

- [ ] **T1.1.2** 实现函数体 (15 min)
  ```rust
  pub fn pg_type_is_visible(_args: Vec<Value>) -> Result<Value> {
      Ok(Value::Boolean(true))
  }
  ```

- [ ] **T1.1.3** 添加单元测试 (30 min)
  - 测试基本调用
  - 测试 NULL 参数
  - 测试不同 OID 值

- [ ] **T1.1.4** 添加集成测试 (30 min)
  - 创建 `tests/XX_pg_type_is_visible.sql`
  - 测试与 enum 类型结合使用

- [ ] **T1.1.5** 更新 AGENTS.md 文档 (15 min)

#### 验收标准

- [x] `SELECT pg_type_is_visible(12345)` 返回 `true`
- [ ] SQLAlchemy enum 类型检查不再报错
- [ ] 所有现有测试通过

---

### Sprint 1.2: Better Trigger Error Handling

| 属性 | 值 |
|------|-----|
| **优先级** | P2 |
| **预估工时** | 4 小时 |
| **负责人** | TBD |
| **前置依赖** | 无 |

#### Tasks

- [ ] **T1.2.1** 定义不支持的函数列表 (30 min)
  ```rust
  const UNSUPPORTED_TRIGGER_FUNCTIONS: &[&str] = &[
      "to_tsvector", "plainto_tsquery", "to_tsquery",
      "ts_rank", "ts_rank_cd", "setweight",
      "tsvector_update_trigger",
  ];
  ```

- [ ] **T1.2.2** 实现 `validate_trigger_body()` (1 hour)
  - 扫描 trigger body
  - 检测不支持的函数
  - 生成清晰的错误消息

- [ ] **T1.2.3** 集成到 trigger 执行流程 (30 min)
  - 在 `execute_trigger_body()` 开始时调用验证

- [ ] **T1.2.4** 添加测试 (1 hour)
  - 测试检测 `to_tsvector`
  - 测试错误消息格式
  - 测试正常 trigger 不受影响

- [ ] **T1.2.5** 更新文档 (30 min)

#### 验收标准

- [ ] 使用 `to_tsvector` 的 trigger 返回清晰错误
- [ ] 错误消息包含函数名和建议
- [ ] 正常 trigger 功能不受影响

---

### Phase 1 Deliverables

| Deliverable | 完成标准 |
|-------------|---------|
| `pg_type_is_visible` 函数 | 单元测试 + 集成测试通过 |
| Trigger 错误处理 | 错误消息清晰可读 |
| 文档更新 | AGENTS.md 更新 |

---

## Phase 2: ARRAY Protocol Fix (Week 2-3)

**目标**: 修复 ARRAY 类型在 Python 驱动中返回字符串的问题

### Sprint 2.1: Investigation & Diagnosis

| 属性 | 值 |
|------|-----|
| **优先级** | P1 |
| **预估工时** | 1 天 |
| **负责人** | TBD |
| **前置依赖** | Phase 1 完成 |

#### Tasks

- [ ] **T2.1.1** 搭建测试环境 (2 hours)
  - 准备 psycopg2 测试脚本
  - 准备 asyncpg 测试脚本
  - 准备 pg8000 测试脚本

- [ ] **T2.1.2** 抓包分析 (2 hours)
  - 对比 pg-tikv 和 PostgreSQL 的 wire protocol
  - 记录 OID 差异
  - 记录 RowDescription 差异

- [ ] **T2.1.3** 代码审查 (2 hours)
  - 审查 `datatype_to_pgtype()` 
  - 审查 `infer_result_fields_from_query()`
  - 审查 `encode_array()`

- [ ] **T2.1.4** 问题定位报告 (2 hours)
  - 记录根因
  - 提出解决方案

#### 输出

- 问题根因分析文档
- 修复方案确认

---

### Sprint 2.2: OID Fix Implementation

| 属性 | 值 |
|------|-----|
| **预估工时** | 2 天 |
| **前置依赖** | T2.1 完成 |

#### Tasks

- [ ] **T2.2.1** 添加 ARRAY OID 常量 (1 hour)
  ```rust
  // src/protocol/handler.rs 或 types/mod.rs
  pub const INT4_ARRAY_OID: u32 = 1007;
  pub const TEXT_ARRAY_OID: u32 = 1009;
  pub const INT8_ARRAY_OID: u32 = 1016;
  pub const BOOL_ARRAY_OID: u32 = 1000;
  pub const FLOAT8_ARRAY_OID: u32 = 1022;
  pub const UUID_ARRAY_OID: u32 = 2951;
  pub const TIMESTAMP_ARRAY_OID: u32 = 1115;
  pub const JSONB_ARRAY_OID: u32 = 3807;
  ```

- [ ] **T2.2.2** 修改 `datatype_to_pgtype()` (2 hours)
  - 为 `DataType::Array(elem)` 返回正确的数组 OID
  - 处理嵌套数组情况

- [ ] **T2.2.3** 修改类型推断 (2 hours)
  - 确保 `ARRAY[1,2,3]` 推断为 `INT4[]`
  - 确保 `ARRAY['a','b']` 推断为 `TEXT[]`

- [ ] **T2.2.4** 验证 text format 编码 (2 hours)
  - 确保 `encode_array()` 输出与 PostgreSQL 完全一致
  - 特别关注 NULL、空字符串、特殊字符

#### Code Changes

**File**: `src/protocol/handler.rs`

```rust
fn datatype_to_pgtype(dt: &DataType) -> Type {
    match dt {
        DataType::Array(elem_type) => {
            let elem_oid = datatype_to_pgtype(elem_type);
            match elem_type.as_ref() {
                DataType::Int32 => Type::INT4_ARRAY,
                DataType::Int64 => Type::INT8_ARRAY,
                DataType::Text => Type::TEXT_ARRAY,
                DataType::Boolean => Type::BOOL_ARRAY,
                DataType::Float64 => Type::FLOAT8_ARRAY,
                DataType::Uuid => Type::new("uuid[]".into(), 2951),
                DataType::Timestamp => Type::new("timestamp[]".into(), 1115),
                DataType::Jsonb => Type::new("jsonb[]".into(), 3807),
                _ => Type::TEXT_ARRAY, // Fallback
            }
        }
        // ... existing cases
    }
}
```

---

### Sprint 2.3: Testing & Validation

| 属性 | 值 |
|------|-----|
| **预估工时** | 2 天 |
| **前置依赖** | T2.2 完成 |

#### Tasks

- [ ] **T2.3.1** Rust 单元测试 (3 hours)
  - 测试 `encode_array()` 各种边界情况
  - 测试 OID 映射正确性

- [ ] **T2.3.2** SQL 集成测试 (3 hours)
  ```sql
  -- tests/XX_array_types.sql
  SELECT ARRAY[1, 2, 3];
  SELECT ARRAY['a', 'b', 'c'];
  SELECT ARRAY[1, NULL, 3];
  SELECT ARRAY[ARRAY[1,2], ARRAY[3,4]];
  
  CREATE TABLE arr_test (
      id SERIAL,
      int_arr INT[],
      text_arr TEXT[]
  );
  INSERT INTO arr_test (int_arr, text_arr) 
  VALUES (ARRAY[1,2,3], ARRAY['a','b']);
  SELECT * FROM arr_test;
  DROP TABLE arr_test;
  ```

- [ ] **T2.3.3** Python 驱动测试 (4 hours)
  ```python
  # tests/python/test_array_protocol.py
  import psycopg2
  
  def test_int_array_returns_list():
      cursor.execute("SELECT ARRAY[1, 2, 3]")
      result = cursor.fetchone()[0]
      assert isinstance(result, list), f"Expected list, got {type(result)}"
      assert result == [1, 2, 3]
  
  def test_text_array_returns_list():
      cursor.execute("SELECT ARRAY['hello', 'world']")
      result = cursor.fetchone()[0]
      assert isinstance(result, list)
      assert result == ['hello', 'world']
  
  def test_array_with_null():
      cursor.execute("SELECT ARRAY[1, NULL, 3]")
      result = cursor.fetchone()[0]
      assert result == [1, None, 3]
  
  def test_table_array_column():
      cursor.execute("""
          CREATE TABLE test_arr (id SERIAL, tags TEXT[]);
          INSERT INTO test_arr (tags) VALUES (ARRAY['a', 'b']);
          SELECT tags FROM test_arr;
      """)
      result = cursor.fetchone()[0]
      assert isinstance(result, list)
      assert result == ['a', 'b']
  ```

- [ ] **T2.3.4** ORM 测试验证 (2 hours)
  - 运行完整 ORM 测试套件
  - 确保无回归

---

### Phase 2 Deliverables

| Deliverable | 完成标准 |
|-------------|---------|
| ARRAY OID 映射 | 所有常用类型返回正确 OID |
| Text format 编码 | 与 PostgreSQL 输出一致 |
| Python 测试 | psycopg2/asyncpg 正确解析数组 |
| ORM 测试 | 600+ 测试全部通过 |

---

## Phase 3: GIN Index for ARRAY (Week 4-5)

**目标**: 扩展 GIN 索引支持 ARRAY 类型的 containment 查询

### Sprint 3.1: Token Extraction

| 属性 | 值 |
|------|-----|
| **预估工时** | 2 天 |
| **前置依赖** | Phase 2 完成 |

#### Tasks

- [ ] **T3.1.1** 设计 ARRAY token 结构 (2 hours)
  - 确定 hash 算法（复用 FNV-1a）
  - 确定 token 前缀区分 JSONB/ARRAY

- [ ] **T3.1.2** 实现 `extract_array_gin_tokens()` (4 hours)
  ```rust
  // src/sql/gin.rs
  pub(crate) fn extract_array_gin_tokens(arr: &[Value]) -> Vec<u64> {
      let mut tokens = Vec::with_capacity(arr.len());
      for elem in arr {
          if !matches!(elem, Value::Null) {
              tokens.push(hash_array_element(elem));
          }
      }
      tokens.sort_unstable();
      tokens.dedup();
      tokens
  }
  ```

- [ ] **T3.1.3** 添加单元测试 (2 hours)
  - 测试基本数组
  - 测试包含 NULL
  - 测试重复元素（应去重）
  - 测试 hash 一致性

---

### Sprint 3.2: Query Planner Integration

| 属性 | 值 |
|------|-----|
| **预估工时** | 2 天 |
| **前置依赖** | T3.1 完成 |

#### Tasks

- [ ] **T3.2.1** 识别 ARRAY containment 表达式 (3 hours)
  ```rust
  // src/sql/planner.rs
  fn is_array_contains_predicate(expr: &Expr, schema: &TableSchema) 
      -> Option<ArrayContainsInfo> 
  {
      match expr {
          Expr::BinaryOp { 
              left, 
              op: BinaryOperator::AtAt,  // @> operator
              right 
          } => {
              // Check if left is array column and right is array literal
              ...
          }
          _ => None,
      }
  }
  ```

- [ ] **T3.2.2** 实现 GIN 扫描路径选择 (3 hours)
  ```rust
  fn choose_gin_access_path_for_array(
      schema: &TableSchema,
      contains_info: &ArrayContainsInfo,
      estimated_rows: u64,
  ) -> Option<AccessPath> {
      // Find GIN index on the column
      let index = schema.indexes.iter().find(|idx| {
          idx.method.as_ref().map(|m| m.eq_ignore_ascii_case("gin")).unwrap_or(false)
              && idx.columns.len() == 1
              && idx.columns[0] == contains_info.column_name
      })?;
      
      // Calculate cost
      let token_count = contains_info.search_values.len();
      let cost = estimate_gin_scan_cost(token_count, estimated_rows);
      
      Some(AccessPath::GinIndexScan {
          index_name: index.name.clone(),
          tokens: extract_array_gin_tokens(&contains_info.search_values),
          cost,
      })
  }
  ```

- [ ] **T3.2.3** 扩展 `supported_gin_index_column()` (2 hours)
  - 支持 `DataType::Array(_)` 列

---

### Sprint 3.3: Index Maintenance

| 属性 | 值 |
|------|-----|
| **预估工时** | 1 天 |
| **前置依赖** | T3.2 完成 |

#### Tasks

- [ ] **T3.3.1** 修改 INSERT 路径 (2 hours)
  - `src/sql/dml.rs` - `execute_insert()`
  - 为 ARRAY 列生成 GIN tokens

- [ ] **T3.3.2** 修改 UPDATE 路径 (2 hours)
  - 删除旧 tokens
  - 插入新 tokens

- [ ] **T3.3.3** 修改 DELETE 路径 (1 hour)
  - 删除对应 tokens

- [ ] **T3.3.4** 修改 CREATE INDEX 路径 (2 hours)
  - 为已有数据构建 GIN 条目

---

### Sprint 3.4: Testing

| 属性 | 值 |
|------|-----|
| **预估工时** | 2 天 |
| **前置依赖** | T3.3 完成 |

#### Tasks

- [ ] **T3.4.1** 集成测试 (4 hours)
  ```sql
  -- tests/XX_gin_array_index.sql
  CREATE TABLE products (
      id SERIAL PRIMARY KEY,
      name TEXT,
      tags TEXT[]
  );
  
  CREATE INDEX idx_products_tags ON products USING GIN (tags);
  
  INSERT INTO products (name, tags) VALUES
      ('Laptop', ARRAY['electronics', 'computer']),
      ('Phone', ARRAY['electronics', 'mobile']),
      ('Book', ARRAY['reading', 'education']);
  
  -- Should use GIN index
  EXPLAIN SELECT * FROM products WHERE tags @> ARRAY['electronics'];
  
  -- Functional test
  SELECT name FROM products WHERE tags @> ARRAY['electronics'] ORDER BY name;
  -- Expected: Laptop, Phone
  
  SELECT name FROM products WHERE tags @> ARRAY['electronics', 'mobile'];
  -- Expected: Phone
  
  DROP TABLE products;
  ```

- [ ] **T3.4.2** 性能测试 (4 hours)
  - 创建 10000 行测试数据
  - 对比 GIN scan vs full table scan
  - 记录性能基准

- [ ] **T3.4.3** 边界情况测试 (4 hours)
  - 空数组
  - NULL 元素
  - 大数组（100+ 元素）
  - 更新/删除后索引一致性

---

### Phase 3 Deliverables

| Deliverable | 完成标准 |
|-------------|---------|
| ARRAY token extraction | 单元测试覆盖 |
| Query planner 识别 | `EXPLAIN` 显示 GIN scan |
| Index maintenance | INSERT/UPDATE/DELETE 正确维护索引 |
| 性能验证 | GIN scan 明显快于 full scan |

---

## Phase 4: Full-Text Search MVP (Week 5-6+)

**目标**: 实现基础全文搜索功能

> ⚠️ 此阶段为可选，优先级 P3，可根据实际需求调整

### Sprint 4.1: Type System

| 属性 | 值 |
|------|-----|
| **预估工时** | 2 天 |
| **前置依赖** | 无 |

#### Tasks

- [ ] **T4.1.1** 添加 `DataType::Tsvector` 和 `DataType::Tsquery`
- [ ] **T4.1.2** 添加 `Value::Tsvector(String)` 和 `Value::Tsquery(String)`
- [ ] **T4.1.3** 实现 wire protocol OID 映射 (tsvector=3614, tsquery=3615)
- [ ] **T4.1.4** 实现 Value 序列化/反序列化
- [ ] **T4.1.5** 添加类型推断规则

---

### Sprint 4.2: Core Functions

| 属性 | 值 |
|------|-----|
| **预估工时** | 3 天 |
| **前置依赖** | T4.1 完成 |

#### Tasks

- [ ] **T4.2.1** 创建 `src/sql/expr/functions/fts.rs`
- [ ] **T4.2.2** 实现 tokenizer (简单空格分词)
  ```rust
  fn tokenize(text: &str) -> Vec<String> {
      text.to_lowercase()
          .split(|c: char| !c.is_alphanumeric())
          .filter(|s| s.len() >= 2)
          .map(String::from)
          .collect::<BTreeSet<_>>()
          .into_iter()
          .collect()
  }
  ```
- [ ] **T4.2.3** 实现 `to_tsvector()`
- [ ] **T4.2.4** 实现 `plainto_tsquery()`
- [ ] **T4.2.5** 实现 `to_tsquery()`
- [ ] **T4.2.6** 实现 `ts_rank()`

---

### Sprint 4.3: Match Operator

| 属性 | 值 |
|------|-----|
| **预估工时** | 1 天 |
| **前置依赖** | T4.2 完成 |

#### Tasks

- [ ] **T4.3.1** 在 `operators.rs` 添加 `@@` 操作符处理
- [ ] **T4.3.2** 实现 tsvector/tsquery 匹配逻辑
- [ ] **T4.3.3** 支持 `tsquery @@ tsvector` 和 `tsvector @@ tsquery`

---

### Sprint 4.4: Testing

| 属性 | 值 |
|------|-----|
| **预估工时** | 2 天 |
| **前置依赖** | T4.3 完成 |

#### Tasks

- [ ] **T4.4.1** 单元测试
  - tokenizer 测试
  - 各函数测试
  - 匹配逻辑测试

- [ ] **T4.4.2** 集成测试
  ```sql
  -- tests/XX_fts_basic.sql
  SELECT to_tsvector('english', 'The quick brown fox');
  SELECT plainto_tsquery('english', 'quick fox');
  SELECT to_tsvector('quick brown fox') @@ plainto_tsquery('quick fox');
  SELECT ts_rank(to_tsvector('quick brown fox'), plainto_tsquery('quick'));
  ```

- [ ] **T4.4.3** 与 PostgreSQL 行为对比测试

---

### Phase 4 Deliverables

| Deliverable | 完成标准 |
|-------------|---------|
| TSVECTOR/TSQUERY 类型 | 存储和传输正确 |
| 核心函数 | 基本功能工作 |
| @@ 操作符 | 匹配逻辑正确 |
| 文档 | 说明 MVP 限制 |

---

## Risk Register

| ID | 风险 | 影响 | 概率 | 缓解措施 |
|----|------|------|------|---------|
| R1 | ARRAY 问题根因不是 OID | 高 | 中 | Phase 2.1 充分调研 |
| R2 | GIN 性能不佳 | 中 | 低 | 性能测试 + 优化迭代 |
| R3 | FTS tokenizer 质量差 | 低 | 高 | 明确 MVP scope，后续迭代 |
| R4 | 回归 ORM 测试 | 高 | 低 | 每个 Sprint 运行完整测试 |

---

## Testing Strategy

### Continuous Testing

每个 PR 必须：
1. ✅ `cargo test` 通过
2. ✅ `cargo clippy` 无警告
3. ✅ 集成测试通过 (`python3 scripts/integration_test.py`)

### Phase Gate Testing

每个 Phase 结束前：
1. ✅ 完整 ORM 测试 (`cd orm-tests && npm test`)
2. ✅ Python 驱动兼容性测试
3. ✅ 性能基准测试（如适用）

---

## Definition of Done

### Feature Level
- [ ] 代码实现完成
- [ ] 单元测试覆盖 ≥ 80%
- [ ] 集成测试通过
- [ ] 文档更新 (AGENTS.md)
- [ ] PR 审核通过

### Phase Level
- [ ] 所有 Feature DoD 完成
- [ ] ORM 测试套件通过
- [ ] 无已知回归
- [ ] 发布说明准备

---

## Communication Plan

| 事件 | 频率 | 形式 |
|------|------|------|
| 进度更新 | 每日 | 更新 TODO.md |
| 阻塞问题 | 立即 | 提 Issue |
| Phase 完成 | 每 Phase | 发布 Changelog |

---

## Appendix: Task ID Reference

```
Phase 1:
  T1.1.1 - T1.1.5: pg_type_is_visible
  T1.2.1 - T1.2.5: Trigger error handling

Phase 2:
  T2.1.1 - T2.1.4: Investigation
  T2.2.1 - T2.2.4: OID fix
  T2.3.1 - T2.3.4: Testing

Phase 3:
  T3.1.1 - T3.1.3: Token extraction
  T3.2.1 - T3.2.3: Query planner
  T3.3.1 - T3.3.4: Index maintenance
  T3.4.1 - T3.4.3: Testing

Phase 4:
  T4.1.1 - T4.1.5: Type system
  T4.2.1 - T4.2.6: Core functions
  T4.3.1 - T4.3.3: Match operator
  T4.4.1 - T4.4.3: Testing
```
