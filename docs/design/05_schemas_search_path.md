# 设计：Schemas & `search_path`（ORM 迁移兼容）

**Status**: Draft  
**Priority（ORM 迁移）**: P0

## 背景与动机

PostgreSQL 的 schema（`public`、自定义 schema）与 `search_path` 在 ORM 生态里非常常见：
- TypeORM/Sequelize/Knex/Drizzle 都支持 `schema` / `withSchema()` 等，并会生成 `schema.table` 形式的 DDL/DML。
- 迁移脚本常见 `CREATE SCHEMA`、`SET search_path`、`DROP SCHEMA`。

当前 db9-server 的对象名处理大多直接取 `ObjectName` 的最后一段（`name.0.last()`），等价于“忽略 schema”，只能可靠支持单一 `public` 逻辑 schema。

## 目标（MVP）

- 支持：
  - `CREATE SCHEMA [IF NOT EXISTS] <schema>`
  - `DROP SCHEMA [IF EXISTS] <schema> [CASCADE|RESTRICT]`（先支持 RESTRICT）
  - schema-qualified 对象名：`schema.table` / `schema.view` / `schema.sequence` / `schema.type`
  - `SET search_path TO ...`（至少支持 `public` + 一个自定义 schema）
- 默认行为与 PG 对齐：
  - 默认 schema 为 `public`
  - 未显式指定 schema 时按 `search_path` 解析
- 信息模式：
  - `information_schema.schemata/tables/columns` 能列出自定义 schema

## 非目标（MVP 不做）

- 多数据库（`CREATE DATABASE`）语义（当前明确 skip）
- 复杂 `search_path` 规则（如 `$user`、临时 schema）全部对齐
- schema 级权限/ownership 的完整语义（先把迁移跑通）

## 设计概览

### 1) 统一对象名解析：`ResolvedName`

新增一个统一的解析入口（建议放 `src/sql/helpers.rs` 或新模块）：

```text
ResolvedName { schema: String, name: String, full: String } // full = "{schema}.{name}"
```

解析规则：
- `ObjectName` 有两段（`schema.table`）：直接使用（都走 normalize_ident）
- 只有一段（`table`）：
  - DDL（CREATE TABLE/VIEW/SEQUENCE/TYPE）：默认写入到 `search_path[0]`（默认 public）
  - DML/Query：按 `search_path` 依次尝试“存在性”（`table_exists/get_schema/get_view/...`）找到第一个命中的 full name
    - 如果都不存在：回退到 `search_path[0]`（用于“即将创建”的对象）或直接报错（按具体语句）

性能注意：
- 解析发生在 statement 级，避免在行级循环中重复解析。
- 对存在性检查可通过一次性加载 schema cache（本 statement 内）减少多次 RPC。

### 2) 存储层 key 命名：以 `schema.name` 作为对象名

当前 schema key 形如 `_sys_schema_{table_name}`，其中 `{table_name}` 是一段字符串。

MVP 做法：
- 将 `{table_name}` 统一改为 full name（`public.users`、`app.users`）
- 同理 view/matview/procedure/sequence/type 的元数据 key 也采用 full name

这样：
- 无需修改数据 key/index key（它们基于 `table_id`）
- schema 的“命名空间隔离”由对象名字符串实现，简单直接

约束/风险：
- 不支持对象名中包含 `.`（理论上 PG 允许用 quoted ident 包含点，但 ORM 基本不会这么做）。MVP 明确不支持。

### 3) Schema Catalog

新增元数据：
- `_sys_schema_{schema}` -> `SchemaDef { name, owner, created_at... }`
- 提供：
  - `create_schema / drop_schema / list_schemas`

并在 `information_schema.schemata` 输出中动态包含这些 schema（除 `pg_catalog`/`information_schema` 外）。

### 4) `search_path`：会话级变量

扩展 `Session`：
- `search_path: Vec<String>`（默认 `["public"]`）

实现 `SET search_path`：
- 在 `Executor` statement dispatch 中对 `Statement::SetVariable` 做特判：
  - 只处理 `search_path`
  - 更新 session.search_path
  - 其它 `SET` 仍保持 no-op（兼容现状）

同时修正内建函数：
- `CURRENT_SCHEMA()` 目前固定返回 `public`（`src/sql/expr.rs`），应返回 `session.search_path[0]` 或 current schema
- `CURRENT_DATABASE()` 固定返回 `postgres`，可保持（单库）

注意：`eval_function` 当前不接收 session，上下文改造需与 executor 传参方式配合（可先在 FROM 侧特殊处理的 `CURRENT_SCHEMA()` 上做最小修补）。

## 实现步骤（建议）

1. 引入 `ResolvedName` + 统一解析函数，并先改 DDL（CREATE/DROP/ALTER）使用 full name 存储 schema
2. 改 SELECT/INSERT/UPDATE/DELETE 的表解析逻辑，支持显式 schema 与 search_path 查找
3. 增加 `CREATE/DROP SCHEMA` 与 schema catalog
4. 修正 `information_schema` 输出与 `CURRENT_SCHEMA()` 语义

## 测试计划

### 单元测试（`cargo test`）

- `resolve_object_name()`：覆盖 schema-qualified/unqualified + search_path 多项
- schema 名规范化（大小写、引号）与 full name 拼接一致性

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/41_schemas.sql`：
- `CREATE SCHEMA app;`
- `CREATE TABLE app.users (...); INSERT ...; SELECT ...;`
- `SET search_path TO app, public;`
  - `CREATE TABLE t1 ...` => 实际落到 `app.t1`
  - `SELECT * FROM users` => 命中 `app.users`
- `information_schema.schemata/tables/columns` 能列出 `app`
- `DROP SCHEMA app`：RESTRICT 下若 schema 非空报错；清空后可 drop

### ORM 测试

新增/增强：
- Knex：`knex.schema.withSchema('app')...`（表创建/列变更）
- Sequelize：`Model.schema('app')` 或迁移中 `schema` 选项
- TypeORM：DataSource 配置 schema 后同步/迁移一轮

