# 设计：用户自定义类型（CREATE TYPE AS ENUM / composite） [DONE]

**Status**: Draft  
**Priority（ORM 迁移）**: P0（Enum），P2（Composite）

## 背景与动机

ORM 迁移里最常见的自定义类型是 **Enum**：
- Prisma：`CREATE TYPE "Role" AS ENUM ('USER','ADMIN')`
- TypeORM：在 `enum`/`simple-enum` 等映射下也会生成 `CREATE TYPE ... AS ENUM`

当前 pg-tikv：
- `CREATE TYPE` 在执行器里是 no-op（`src/sql/executor.rs` 对 `Statement::CreateType` 直接 `Ok(Empty)`）
- `CREATE TYPE AS ENUM` 明确被标记为 unsupported（`src/sql/helpers.rs`）

这会导致 ORM 迁移无法落地。

## 目标（MVP：Enum）

- 支持：
  - `CREATE TYPE <name> AS ENUM (...)`
  - `DROP TYPE [IF EXISTS] <name> [CASCADE|RESTRICT]`（先支持 RESTRICT 语义）
  - 在 `CREATE TABLE` / `ALTER TABLE ADD COLUMN` 中引用 enum 类型
- DML 语义：
  - enum 列存储为文本，但必须校验值属于 enum label 集合
- 元数据可被 introspection 查询到（至少满足常见 ORM 的 pg_catalog 查询）

## 非目标（MVP 不做）

- 完整 PostgreSQL enum 的 OID/二进制编码/排序规则
- `ALTER TYPE ... ADD VALUE` 等 enum 演进
- 复合类型（composite）在列中的读写与表达式求值（可先做到“可创建/可 introspect，但不可作为列类型使用”）

## 设计概览

### 1) 存储层：新增 Type Catalog

在 TiKV metadata 中存储用户定义类型：

- Key：`_sys_type_{schema}.{name}`（建议 schema 前置，与 schema/search_path 设计一致）
- Value：序列化结构 `UserTypeDef`

```rust
enum UserTypeKind {
  Enum { labels: Vec<String> },
  Composite { fields: Vec<(String, DataType)> },
}

struct UserTypeDef {
  schema: String,
  name: String,
  kind: UserTypeKind,
  owner: String, // 可选
}
```

并提供：
- `create_type(txn, def)`
- `drop_type(txn, full_name)`
- `get_type(txn, full_name) -> Option<UserTypeDef>`
- `list_types(txn) -> Vec<UserTypeDef>`（用于 pg_catalog 输出）

### 2) DDL：执行 CREATE/DROP TYPE

在 `Executor::execute_statement_on_txn()` 中：
- 实现 `Statement::CreateType`（并移除/放宽 `get_unsupported_reason()` 对 enum 的拦截）
- `Statement::Drop` 增加 `ObjectType::Type` 分支，调用 `drop_type`

### 3) 表列类型绑定：保留 UDT 名称

**关键点**：迁移/ORM introspection 需要知道列的“声明类型名”（udt_name），不能简单把 enum 变成 TEXT 丢失信息。

建议做法（兼顾现有 `DataType` 与兼容性）：
- 在 `ColumnDef` 里新增字段（带 `#[serde(default)]`）：
  - `udt_name: Option<String>`：例如 `"public.role"` 或 `"role"`（建议存 full name）
- 对 enum 列：
  - `ColumnDef.data_type` 仍使用 `DataType::Text`（wire/output 以 text 输出即可）
  - `ColumnDef.udt_name = Some("...")`

解析路径：
- 在 `ddl.rs` 解析 `SqlDataType::Custom` 时，不再直接 `convert_data_type()` 结束：
  - 若是内建 custom（例如 `VECTOR`）按现有逻辑处理
  - 否则查询 type catalog：
    - 命中 enum：设置 `data_type=Text` + `udt_name=...`
    - 未命中：按现有策略（Text 或 error，取决于兼容性策略）

### 4) DML：enum 值校验

在 INSERT/UPDATE 写入前（建议在 `dml.rs` 组装 row 完成、写入前的校验阶段）：
- 对每个 `udt_name.is_some()` 的列：
  - 若值为 NULL：允许（PG 语义）
  - 否则要求值为 `Value::Text`，且在 enum labels 集合中

性能策略：
- 每条语句/每个表只加载一次相关 enum 定义，构建 `HashMap<udt_name, HashSet<label>>`
- 仅当表 schema 中存在 `udt_name` 列时才触发加载

### 5) pg_catalog / information_schema 输出

为了让 ORM introspection 通过，需要至少补齐：
- `pg_type`：为 enum 类型生成一行（`typtype='e'`、`typcategory='E'` 等可按 PG 近似）
- `pg_enum`：列出 enum labels（按 sort order）
- `information_schema.columns`：
  - `udt_name` 返回 enum 名
  - `data_type` 可返回 `USER-DEFINED` 或 `character varying`（兼容为先，建议 USER-DEFINED）

## 实现步骤（建议）

1. 先落地 enum 类型的存储与 DDL（create/drop/list/get）
2. 再落地列绑定（`udt_name`）与 DML 校验
3. 最后补齐 pg_catalog（pg_type/pg_enum）与 information_schema 输出
4. composite 类型作为后续（先支持 create/drop + pg_type 输出，但禁止用作列类型）

## 测试计划

### 单元测试（`cargo test`）

- `UserTypeDef` 的序列化/反序列化（bincode/serde）
- enum 校验逻辑：NULL 允许；大小写敏感；非法值报错信息稳定

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/39_enum_types.sql`：
- `CREATE TYPE role AS ENUM ('USER','ADMIN');`
- `CREATE TABLE t (id INT PRIMARY KEY, r role);`
- 插入合法/非法值：
  - `INSERT ... ('USER')` 成功
  - `INSERT ... ('INVALID')` 报错
- introspection：
  - 查询 `pg_catalog.pg_type` / `pg_catalog.pg_enum` / `information_schema.columns` 能看到该类型与列声明
- `DROP TYPE role`：
  - RESTRICT：若仍被表引用应报错
  - IF EXISTS：不存在时不报错

### ORM 测试

由于 `./run_tests.sh` 当前不跑 Prisma（脚本注释说明），建议两条路径：
- 在 `orm-tests/typeorm/` 增加 enum schema/migration 测试（TypeORM 会发 pg_catalog 查询）
- 若未来恢复 Prisma 测试：增加一个最小 Prisma schema 仅包含 enum，跑一次 migrate/sync

