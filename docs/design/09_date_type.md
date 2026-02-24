# 设计：DATE 类型（不带时区的日期）[done]

**Status**: Draft  
**Priority（ORM 迁移）**: P0

## 背景与动机

当前 db9-server 把 `DATE` 映射为 `TIMESTAMP`（`src/sql/helpers.rs:363`），这会导致：
- 类型语义不一致：DATE 没有时间部分，但 TIMESTAMP 有
- ORM 类型映射/迁移差异：很多 ORM 会区分 `date` 与 `timestamp`
- introspection 输出不准确：`information_schema.columns.data_type` 等会误报

## 目标（MVP）

- 支持 `DATE` 列类型（DDL/DML/SELECT 输出）
- 支持 `DATE` 的解析、比较、排序
- pgwire OID 映射为 `DATE`（OID 1082）
- `information_schema.columns` 正确输出 date 类型

## 非目标（MVP 不做）

- 完整的 date 算术（`date + interval` / `age(date, date)` 等）
- 时区相关语义（DATE 本身不涉及时区，但与 timestamp 转换时会涉及；先做最小集合）

## 设计概览

### 1) 类型表示

新增：
- `DataType::Date`
- `Value::Date(i32)`：表示自 1970-01-01 起的天数（与很多 DB 的内部表示一致）

原则：
- 只追加 enum 变体，不重排（保证 bincode 向后兼容）

### 2) 解析与写入

在 `coerce_value_for_column()` 增加：
- `Value::Text("YYYY-MM-DD")` -> `Value::Date(days)`
- 允许 `Value::Timestamp` cast 到 `Date`（截断到日期，按 UTC；如需与 PG 更一致可后续扩展）

### 3) 输出与 pgwire

在 `protocol/handler.rs`：
- `datatype_to_pgtype(Some(DataType::Date)) => Type::DATE`
- `encode_value(Value::Date(days))` 输出 `YYYY-MM-DD`

### 4) 表达式与比较

- `compare_values()` 支持 `Date` 与 `Date`
- 若需要与 `Timestamp` 比较：MVP 可先要求类型一致，否则报错（与当前系统的类型宽松策略协调后再增强）

## 实现步骤（建议）

1. 扩展 `DataType/Value` + encode/decode（storage/pgwire）
2. 扩展 `convert_data_type()`：`SqlDataType::Date => DataType::Date`
3. 扩展 `coerce_value_for_column()`：写入/更新时解析 date
4. 补齐比较/排序与信息模式输出

## 测试计划

### 单元测试（`cargo test`）

- date 解析：`1970-01-01` => 0，闰年/跨月正确
- 输出 round-trip：`Value::Date -> text -> parse -> Value::Date` 一致

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/44_date_type.sql`：
- `CREATE TABLE t (id INT PRIMARY KEY, d DATE);`
- 插入：
  - `INSERT ... ('2024-01-02');`
  - `INSERT ... (DATE '2024-01-03');`（若 sqlparser 支持）
- 查询：
  - `SELECT d FROM t ORDER BY d;` 输出 `YYYY-MM-DD`
  - `WHERE d >= '2024-01-02'`
- `information_schema.columns`/`pg_type` 中 date 类型可见

### ORM 测试

- TypeORM：`@Column({ type: 'date' })` round-trip
- Knex：`table.date('d')` 创建后 introspection 不误报为 timestamp

