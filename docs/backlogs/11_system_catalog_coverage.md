# 设计：pg_catalog / information_schema（迁移与 ORM introspection 覆盖）

**Status**: Draft  
**Priority（ORM 迁移）**: P0

## 背景与动机

ORM 与迁移工具会大量依赖系统表做 introspection（表/列/索引/约束/类型/序列等）。pg-tikv 已实现了一部分虚拟系统表（`src/sql/information_schema.rs`），但仍存在“空表/字段不足/关联关系不稳定”的风险点：

- `pg_proc` 目前返回空（`src/sql/information_schema.rs:1687`）
- 缺少 enum/sequence 等相关系统表（如 `pg_enum`、`pg_sequence`）
- OID 体系需要稳定且可 join（Sequelize/TypeORM 会 join 多张 pg_catalog）

即使 ORM 测试当前大部分通过，迁移场景对 pg_catalog 的覆盖面通常更广（尤其是 schema/type/sequence 相关）。

## 目标（MVP）

- 提供一组“足够真实、可 join、字段稳定”的 pg_catalog/information_schema 视图，覆盖迁移/ORM 常见查询：
  - schemas/tables/columns
  - constraints（pk/unique/fk/check）
  - indexes（pg_index/pg_class/pg_attribute/pg_indexes）
  - types（含 enum、vector、自定义类型）
  - sequences（pg_class relkind='S' + pg_sequence/pg_attrdef/默认值）
- 关键属性：OID 生成策略稳定（同一对象跨重启/跨查询保持一致），join 关系正确

## 非目标（MVP 不做）

- 完整系统表字段与权限模型（先满足 ORMs 的查询子集）
- 性能统计/统计信息系统（pg_stat_*）

## 设计概览

### 1) 稳定 OID 策略

为所有可 join 的对象生成稳定 OID（避免每次 list 时递增导致不稳定）：

建议规则（示例）：
- namespace：
  - `pg_catalog` = 11（PG 常量）
  - `information_schema` = 13222（PG 常量）
  - `public` = 2200（PG 常量）
  - user schema：使用 hash 或基于创建顺序的持久化分配（存到 schema catalog 中）
- table/class：
  - `pg_class.oid = 10000 + table_id`（table_id 已是稳定分配）
- index：
  - `pg_class.oid = 20000 + (table_id << 16) + index_id`（稳定且可逆）
- type：
  - builtin：使用 PG 常见 OID（已有部分）
  - user-defined：使用持久化分配（存到 type catalog）

该策略要保证：
- 不同对象不冲突
- 可由对象元数据推导（或从 catalog 读取）得到稳定值

### 2) 补齐关键表（按 ORM 需求）

在现有基础上增量补齐：
- `pg_enum`：enum labels（配合 `docs/design/03_user_defined_types_enum_composite.md`）
- `pg_sequence`：sequence 元数据（配合 `docs/design/04_sequences.md`）
- `pg_attrdef`：列默认值（用于 introspection default_expr/nextval）
- `pg_tables` / `pg_views`（部分 ORM 会用）
- 必要时补 `pg_depend`（用于 owned-by/依赖关系）

### 3) `pg_proc`：返回最小集合的内建函数

即使我们不完全实现函数执行，ORM 经常只要求 `pg_proc` 可查询并能 join：
- 至少列出常用的系统函数名：`current_schema`、`current_database`、`version`、`pg_get_constraintdef`、`pg_get_indexdef`、`format_type` 等
- 字段按 ORM 查询子集提供（proname/prorettype/proargtypes/pronamespace 等）

### 4) information_schema 输出增强

增强点：
- `schemata`：动态包含 user schema
- `columns`：
  - date/numeric/enum 等类型字段输出正确（配合相应设计文档）
  - `column_default` 对 serial/identity 使用可解析的 nextval(...)

## 实现步骤（建议）

1. 固化 OID 策略（先改现有 pg_class/pg_index/pg_attribute 输出）
2. 引入 schema/type/sequence catalog 后，补齐对应系统表
3. 填充 `pg_proc` 最小集合
4. 用真实 ORM introspection query 做回归测试

## 测试计划

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/45_pg_catalog_introspection.sql`，收录（或直接复制）常见 ORM introspection 语句片段：
- Sequelize `showIndex`/`describeTable` 相关 join
- TypeORM schema builder 查询（pg_class/pg_attribute/pg_constraint join）
- enum/sequence 的 introspection 查询

并验证：
- 查询不报错
- 关键列非 NULL 且 join 行数符合预期

### ORM 测试

在 `orm-tests/typeorm/schema.test.ts` / `sequelize/` 增加：
- 创建包含 enum/sequence/自定义 schema 的结构
- 触发 ORM introspection（sync/migrations）并确保通过

