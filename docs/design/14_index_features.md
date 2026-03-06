# 设计：高级索引（partial / expression / GIN/GiST 兼容）

**Status**: Draft  
**Priority（ORM 迁移）**: P1（DDL 兼容），P2（查询加速）

> **Draft / non-SoT note**
>
> This document is a design draft, not a current-behavior contract.
> Validate current behavior against `docs/sot/**`, `docs/ARCHITECTURE.md`, and the implementation under `src/**` before using it for product or compatibility decisions.

## 背景与动机

PostgreSQL 迁移里常出现更复杂的索引形式：
- partial index：`CREATE INDEX ... ON t(col) WHERE ...`
- expression index：`CREATE INDEX ... ON t ((lower(col)))`
- GIN（常用于 jsonb/array）：`CREATE INDEX ... USING gin (data)`

当前 db9-server 的限制：
- `CREATE INDEX` 仅允许列标识符，表达式直接报错（`src/sql/ddl.rs:482`）
- `USING GIST` 被明确标记不支持（`src/sql/helpers.rs:842`）

即使短期不实现所有加速能力，迁移 DDL 本身也需要能“落地并可 introspect”，否则 ORM 迁移会失败。

## 目标（分两层）

### 层 1：迁移 DDL 兼容（MVP）

- 允许创建并持久化以下索引元数据：
  - partial index（保存 predicate）
  - expression index（保存表达式）
  - `USING gin/gist/...`（保存 method）
- 对不支持的 method：
  - 迁移 DDL 仍可成功
  - 查询规划阶段不使用该索引（退化为全表扫描/普通索引）

### 层 2：查询加速（增强）

- 对 partial/expression index：在写入时生成对应 index entries，查询时可用
- GIN(jsonb)：至少支持 `@>` 的键/值包含查询（可选）

## 非目标（MVP 不做）

- 全量支持所有 index method 与 operator class
- 统计信息/代价模型完善（先保证功能正确）

## 设计概览

### 1) 扩展 `IndexDef`

为保持向后兼容，新增字段都加 `#[serde(default)]`：

```rust
struct IndexDef {
  name: String,
  id: u64,
  columns: Vec<String>,          // 现有列索引
  unique: bool,
  method: Option<String>,        // btree/gin/gist...
  predicate: Option<String>,     // WHERE ...
  expressions: Vec<String>,      // 表达式索引列（序列化为 SQL 字符串）
}
```

约定：
- `columns` 与 `expressions` 二选一或混用（按 sqlparser AST 表达能力决定）
- `method=None` 等价 btree

### 2) DDL：放宽 `CREATE INDEX` 的 AST 处理

`ddl.rs:execute_create_index()` 当前要求 `OrderByExpr.expr` 必须是 `Expr::Identifier`。

改造方向：
- 如果是标识符：进入 `columns`
- 如果是表达式：保存 `expr.to_string()` 进入 `expressions`
- 解析 `WHERE` 子句（sqlparser 的 CreateIndex 通常带 predicate/where 字段）
- 解析 `USING` method（例如 gin）

MVP：只保证 DDL 成功并更新 schema 元数据；并在 pg_indexes/indexdef 中可见。

### 3) 写入路径：生成 index entry（层 2）

当要实现 partial/expression index 的加速能力时：
- partial：
  - 对每行写入时 eval predicate；为 true 才写 index entry
- expression：
  - 解析 expressions（字符串 -> AST，建议在 schema cache 中编译一次）
  - eval 得到 index key value

对 GIN：
- MVP 可不生成 entries；或先把整段 jsonb 文本作为单 key（仅能等值查询，意义有限）
- 真正 GIN 需要拆 token（复杂，后续单独设计）

## 测试计划

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/47_index_advanced.sql`：
- partial index：DDL 成功，`pg_indexes`/`pg_get_indexdef` 可见 predicate
- expression index：DDL 成功，`pg_indexes` 可见表达式
- `USING gin`：DDL 不失败（但提示“不用于规划”或仅存元数据）

（层 2）若实现加速：
- 对 partial index：验证过滤条件命中索引扫描（EXPLAIN 中可见）或至少结果正确

### ORM 测试

在 `orm-tests/knex` 或 `typeorm` 增加：
- 使用 schema builder 创建 partial/GIN 的用例（如果 ORM 支持），确保迁移不失败
