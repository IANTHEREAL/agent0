# 设计：pgwire OID（Array / Vector）

**Status**: Draft  
**Priority（ORM 迁移）**: P1（Array），P2（Vector OID）

## 背景与动机

pg-tikv 的 SQL 层已经支持数组与向量值：
- `Value::Array` 在 pgwire 输出为 `{...}` 形式（`src/protocol/handler.rs:2156`）
- `Value::Vector` 输出为 `[...]`（`src/protocol/handler.rs:2169`）

但 pgwire 的类型映射目前把 `Array/Vector` 一律当作 `TEXT`（`datatype_to_pgtype`：`src/protocol/handler.rs:1861`）。

这会影响：
- 某些驱动/ORM 会依据 OID 选择解码器（特别是数组）
- pgvector 生态通常依赖自定义类型 OID（如 16385）与 `pg_type`/`format_type` 的一致性

## 目标（MVP）

- Array：
  - 对常见元素类型数组（`int4[]/int8[]/text[]/bool[]`）在 Describe/结果集中返回正确 OID
  - 继续使用 text format 输出（无需 binary）
- Vector：
  - 至少在 pg_catalog 中暴露 vector type（当前已有 `pg_type` 行：`src/sql/information_schema.rs:1667`）
  - 若 pgwire 支持自定义 OID，则为 `vector` 列返回 16385；否则保持 TEXT，但保证 introspection 可发现

## 非目标（MVP 不做）

- binary format 的 array/vector 编解码
- 任意嵌套数组/多维数组完整对齐

## 设计概览

### 1) Array OID 映射

扩展 `datatype_to_pgtype()`：
- `DataType::Array(inner)`：
  - inner=Int32 => `INT4_ARRAY`
  - inner=Int64 => `INT8_ARRAY`
  - inner=Text => `TEXT_ARRAY`（或 `VARCHAR_ARRAY`）
  - inner=Boolean => `BOOL_ARRAY`
  - 其它 => `TEXT`（降级）

同时修正 Describe（`infer_result_fields_from_query`）对 array 列的推断。

输出格式保持现有 `{...}` 文本形式（需要确保与 PG 的 text array 格式兼容；当前实现对文本元素加引号，NULL 为 `NULL`，属于可接受子集）。

### 2) Vector OID 映射（两档）

#### 档位 A（兼容优先，低改动）

- pgwire 仍返回 `Type::TEXT`
- 依赖 `pg_catalog.pg_type` 中的 vector 类型行（已存在）供 ORM 发现
- 驱动侧若需要 vector，可通过自定义 parser 处理 text（例如 `'[1,2,3]'`）

#### 档位 B（更接近 PG，需 pgwire 支持自定义 OID）

若 pgwire crate 支持 “从 OID 构造 Type/FieldInfo”（例如 `Type::from_oid` 或自定义 Type），则：
- `DataType::Vector(_)` -> OID 16385
- `FieldInfo` 使用该 Type

需要评估 pgwire API 能力；若不支持则保持档位 A。

## 实现步骤（建议）

1. 先实现 array 的 OID 映射（最能提升 ORM 兼容）
2. 评估 pgwire 是否支持自定义 OID，再决定 vector 的档位 B 是否可做
3. 增加 Describe/结果集的回归测试

## 测试计划

### 单元测试（`cargo test`）

- `datatype_to_pgtype(DataType::Array(Int32))` => INT4_ARRAY 等
- array text 输出格式包含引号/NULL 的一致性（稳定输出）

### 集成测试（ORM/驱动）

在 `orm-tests/pg-client` 增加测试：
- 建表包含 `int[]`（如果 SQL 层支持声明与存储）
- `SELECT` 返回的 `fields[i].dataTypeID`（node-postgres 可获取）应为正确数组 OID

Vector：
- 若维持 TEXT OID：验证 pg_type/format_type 能发现 vector
- 若做 OID：验证 `fields[i].dataTypeID == 16385`

