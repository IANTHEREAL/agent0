# 设计：Set-Returning Functions（`generate_series` / `unnest`）

**Status**: Draft  
**Priority（ORM 迁移）**: P2

## 背景与动机

在数据迁移/初始化脚本中，`generate_series` 与 `unnest` 非常常见：
- 批量生成测试数据
- 展开数组以做 join/过滤

当前 db9-server：
- `generate_series` 明确报错（`src/sql/expr.rs:1622`）
- `UNNEST` 在 join context 被当作 scalar 返回第一个元素（`src/sql/expr.rs:640`），不符合 PG 的 set-returning 语义
- `executor_join.rs` 已有 “FROM 子句函数调用” 的通道（`get_table_data` 中对 `table_name` 包含 `()` 的分支），可复用扩展为 SRF

## 目标（MVP：FROM 子句 SRF）

- 支持：
  - `SELECT * FROM generate_series(1, 3) AS t(n);`
  - `SELECT * FROM unnest(ARRAY[1,2,3]) AS t(x);`
- 仅支持 SRF 出现在 `FROM`/`JOIN` 位置（MVP）

## 非目标（MVP 不做）

- SRF 出现在 SELECT projection（`SELECT generate_series(1,3)`）的语义（PG 有历史行为差异）
- `generate_series(timestamp, timestamp, interval)` 等复杂重载

## 设计概览

### 1) 扩展 `executor_join.rs:get_table_data()` 的函数分支

当前逻辑（简化）：
- `FROM current_schema()` 等 => 返回单行单列虚拟表

扩展：
- 当识别到 `generate_series(...)`：
  - 解析参数（start, stop, step=1）
  - 生成 rows：每行一个 `Value::Int64/Int32`
  - schema：单列，列名按 PG 规则（通常为函数名或 alias）
- 当识别到 `unnest(array)`：
  - 参数求值为 `Value::Array`
  - 生成 rows：每个元素一行

实现细节：
- 参数来源：`TableFactor::Table { args: Some(..) }` 已可拿到 args（见 `executor_join.rs:243`）
- 需将 args 从 `Vec<FunctionArg>` 求值为 `Value`（可复用现有 `eval_expr`）

### 2) 类型推断/输出

schema 的列类型：
- generate_series：Int64（或按输入类型）
- unnest：按数组元素类型推断（若无法推断则 Text）

### 3) 与 planner/optimizer 的交互

SRF 表通常很小（迁移脚本常用），MVP 可直接物化 `Vec<Row>`。

## 测试计划

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/49_set_returning_functions.sql`：
- `SELECT * FROM generate_series(1,5) ORDER BY 1;`
- `SELECT * FROM unnest(ARRAY[3,1,2]) ORDER BY 1;`
- 与真实表 join：
  - `SELECT ... FROM generate_series(1,3) g(n) JOIN t ON t.id=g.n;`

### ORM 测试（可选）

ORM 迁移中偶尔会用 raw SQL 做数据 backfill。可在 `orm-tests/pg-client` 增加一条 raw query smoke test。

