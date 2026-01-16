# 设计：EXPLAIN ANALYZE

**Status**: Draft  
**Priority（ORM 迁移）**: P3

## 背景与动机

pg-tikv 已支持 `EXPLAIN`（见 `src/sql/executor.rs` + `src/sql/explain.rs`），但 `ANALYZE/VERBOSE` 目前被忽略：
- `Executor::execute_explain(..., _analyze, _verbose)` 参数未使用

虽然这不是迁移必需，但对生产排障/性能分析很有价值。

## 目标（MVP）

- 支持 `EXPLAIN (ANALYZE)` 对 **SELECT** 语句输出：
  - 计划树（已有）
  - 执行耗时（总耗时 + 节点级可选）
  - 实际行数（至少顶层）

## 非目标（MVP 不做）

- DML 的 EXPLAIN ANALYZE（有副作用，需谨慎）
- 统计信息系统与节点级精确 timing（可后续逐步细化）

## 设计概览

### 1) 仅对 SELECT 启用 ANALYZE

当 `analyze=true`：
- 若 statement 是 `SELECT/WITH`：实际执行一次查询，计时，并把结果行数/耗时写入 explain 输出
- 否则返回 error 或降级为普通 EXPLAIN（需明确策略）

### 2) 采集指标

最小集合：
- 总耗时：`Instant::now()` 前后差
- 实际行数：`rows.len()`

增强（后续）：
- 在执行器各阶段埋点（scan/filter/join/aggregate/window），形成节点级 timing/rows

## 测试计划

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/48_explain_analyze.sql`：
- `EXPLAIN (ANALYZE) SELECT ...` 返回多行文本
- 包含关键字（例如 `Execution Time`）与行数

