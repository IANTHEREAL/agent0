# 设计：CREATE FUNCTION / TRIGGER（迁移兼容优先）

**Status**: Draft  
**Priority（ORM 迁移）**: P1（允许迁移落地），P2（触发器执行语义）

## 背景与动机

在真实工程迁移中，经常会出现函数与触发器：
- 自动更新时间戳（`updated_at`）的 trigger
- 审计/软删除等业务 trigger
- 一些迁移工具会创建辅助函数（即使最终不调用）

当前 db9-server：
- `CREATE FUNCTION` 在执行器里是 no-op（`src/sql/executor.rs:260`）
- `CREATE TRIGGER` 被明确标记为 unsupported（`src/sql/helpers.rs:803`）

这会导致迁移脚本直接失败（即使应用不依赖触发器执行语义）。

## 目标（分两阶段）

### 阶段 1（迁移 unblock，MVP）

- 支持解析并接受：
  - `CREATE FUNCTION ...` / `DROP FUNCTION ...`
  - `CREATE TRIGGER ...` / `DROP TRIGGER ...`
- 将定义存入 TiKV 元数据，支持 introspection（最小集合）
- **不要求**触发器实际执行（先保证迁移可跑通）

### 阶段 2（触发器生效，增强）

- 对有限子集触发器实现执行：
  - `BEFORE INSERT/UPDATE` 触发器，做列赋值类逻辑（例如设置 updated_at=now）
- 仅支持 `LANGUAGE SQL` 或内建表达式类 trigger function（避免 PL/pgSQL 引擎）

## 非目标（至少在阶段 1 不做）

- PL/pgSQL 引擎
- 复杂触发器执行上下文（transition tables、statement-level triggers）

## 设计概览

### 1) 元数据存储

新增：
- `_sys_func_{schema}.{name}` -> `FunctionDef`
- `_sys_trigger_{schema}.{name}` -> `TriggerDef`

建议结构（先覆盖迁移所需字段）：

```rust
struct FunctionDef {
  schema: String,
  name: String,
  arg_types: Vec<DataType>,
  return_type: DataType,
  language: String,      // "sql" / "plpgsql"（先存）
  body: String,          // 原始 SQL（可能是 $$...$$）
}

struct TriggerDef {
  schema: String,
  name: String,
  table: String,         // full table name
  timing: String,        // BEFORE/AFTER
  events: Vec<String>,   // INSERT/UPDATE/DELETE
  function: String,      // full function name
}
```

### 2) 执行器：从 no-op/unsupported 变为“存储定义”

- `CREATE FUNCTION`：
  - 解析 statement，提取 name/args/returns/language/body
  - 写入 `_sys_func_*`
- `DROP FUNCTION`：
  - 删除 `_sys_func_*`
- `CREATE TRIGGER`：
  - 校验引用的 function 存在（阶段 1 可弱校验）
  - 写入 `_sys_trigger_*`
- `DROP TRIGGER`：
  - 删除 `_sys_trigger_*`

### 3) 阶段 2：触发器执行（受限）

若要让触发器“真的生效”，建议限定为可用现有表达式求值器实现的子集：
- Trigger function 不是任意 SQL，而是预定义的“赋值列表”或 `UPDATE` 模板
- 或提供少量内建 trigger function（例如 `set_updated_at(column_name)`）

执行点：
- 在 DML 写入前（insert/update），查表的 trigger 列表并应用对 row 的变换
- 性能：触发器列表可在 schema cache 中按表缓存（session/connection 级）

## 测试计划

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/46_functions_triggers_ddl.sql`（阶段 1）：
- `CREATE FUNCTION ... $$...$$ ...` 不报错且可在 pg_catalog 查询到（若实现 introspection）
- `CREATE TRIGGER ...` 不报错
- `DROP TRIGGER/FUNCTION` 可执行

（阶段 2）新增行为测试：
- 创建一个 `updated_at` 触发器
- `INSERT/UPDATE` 后 `updated_at` 被自动设置/更新

### ORM 测试

若某 ORM 迁移脚本包含 trigger/function（可用最小样例模拟）：
- 执行迁移 SQL 不失败（阶段 1）
- 若启用阶段 2：行为一致

