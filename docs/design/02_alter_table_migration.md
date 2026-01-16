# 设计：ALTER TABLE（ORM 迁移兼容） [DONE]

**Status**: Draft  
**Priority（ORM 迁移）**: P0

## 背景与动机

ORM 的 migration 引擎高度依赖 `ALTER TABLE` 的一组常见子操作（rename/drop constraint/alter column 等）。当前 pg-tikv 的 `ALTER TABLE` 支持面偏窄，会直接阻断迁移落地。

现状可见 `src/sql/ddl.rs` 的 `execute_alter_table()`（从 `725` 行附近开始）：
- ✅ `ADD COLUMN`、`DROP COLUMN`、`RENAME COLUMN`
- ⚠️ `ADD CONSTRAINT`：仅部分处理，`CHECK` 被忽略（`TableConstraint::Check` 分支为空）
- ⚠️ `ADD CONSTRAINT FOREIGN KEY`：仅写入 schema 元数据，不验证既有数据（PG 默认会验证，除非 `NOT VALID`）
- ❌ `DROP CONSTRAINT`：忽略（`AlterTableOperation::DropConstraint` 分支为空）
- ❌ `RENAME TO`：忽略（`AlterTableOperation::RenameTable` 分支为空）
- ❌ `RENAME CONSTRAINT`：未实现（`AlterTableOperation::RenameConstraint` 未覆盖）
- ❌ 绝大多数 `ALTER COLUMN ...`：直接 `Unsupported ALTER`（`AlterTableOperation::AlterColumn` 未覆盖）

此外还有几处“语义正确性/一致性”缺口会影响迁移稳定性（即使对应 operation 看似已支持）：
- `RENAME COLUMN` 仅更新 `columns/indexes`，未同步更新 `foreign_keys.columns`，也未处理 `check_constraints.expr` 中的列名引用；会导致后续 DML 触发 FK/CHECK 校验时报错或行为偏离。
- `DROP COLUMN` 仅检查 PK/index 依赖，未检查/处理 FK/CHECK 依赖；并且当前实现通过 `store.upsert()` 回写新 row，存在对“drop 列位于 PK 列之前（复合 PK）”场景的潜在错误风险（PK index 偏移）。

## 目标（以迁移语句覆盖为准）

MVP 优先覆盖 ORM migration 常见模式：
- `ALTER TABLE ... RENAME TO ...`
- `ALTER TABLE ... DROP CONSTRAINT ...`
- `ALTER TABLE ... ADD CONSTRAINT ... CHECK (...)`
- `ALTER TABLE ... RENAME CONSTRAINT ... TO ...`（迁移里偶发，但实现成本低）
- `ALTER TABLE ... ALTER COLUMN ... SET/DROP DEFAULT`
- `ALTER TABLE ... ALTER COLUMN ... SET/DROP NOT NULL`
- `ALTER TABLE ... ALTER COLUMN ... TYPE ...`（限制在可安全转换且**不涉及 PK 变更**的类型集合；`USING` 暂不支持）

并补齐现有 operation 的“元数据一致性”：
- `RENAME COLUMN`：同步更新 FK/CHECK/INDEX 元数据，避免产生“悬挂约束”。
- `DROP COLUMN`：默认按 PG 的 RESTRICT 语义处理依赖（FK/CHECK/INDEX/PK），不支持 `CASCADE`（MVP）。

并确保：
- schema 元数据更新一致
- 相关索引/约束的副作用正确（尤其是 UNIQUE/FK/CHECK）

## 非目标（MVP 不做）

- `ALTER TABLE ... OWNER TO`（当前明确 unsupported，见 `src/sql/helpers.rs`）
- `ALTER TABLE ... SET TABLESPACE` 等存储级语义
- 复杂 `ALTER COLUMN TYPE ... USING <expr>`（可作为 follow-up）
- 自动重写 view/matview/procedure 的 SQL 定义以适配 rename（可选最佳努力）
- `ALTER TABLE ... DROP COLUMN/CONSTRAINT ... CASCADE`（先按 RESTRICT 实现）
- `ALTER COLUMN TYPE` 涉及 PK 列（会触发行 key 迁移与 FK 级联影响，复杂度显著更高）

## 设计概览

### 0) AST 范围（sqlparser 0.40）

本项目使用 `sqlparser 0.40`，`ALTER TABLE` 的关键 AST 形态（见 `sqlparser::ast::AlterTableOperation`）：
- `RenameTable { table_name }` → `RENAME TO <table_name>`
- `RenameColumn { old_column_name, new_column_name }` → `RENAME [COLUMN] ...`
- `RenameConstraint { old_name, new_name }` → `RENAME CONSTRAINT ... TO ...`
- `DropConstraint { if_exists, name, cascade }`
- `AlterColumn { column_name, op: AlterColumnOperation }`，其中：
  - `SetNotNull` / `DropNotNull`
  - `SetDefault { value }` / `DropDefault`
  - `SetDataType { data_type, using }`（`using` 为 PG 特有）

MVP 仅覆盖上述分支，且：
- `cascade=true`：统一返回 “Unsupported”（先按 RESTRICT 实现）
- `using.is_some()`：统一返回 “Unsupported”

### 1) 名称解析与 schema 名称空间（与“schema/search_path”设计配合）

迁移语句经常带 schema 限定（`public.table` 或自定义 schema）。建议与 `docs/design/05_schemas_search_path.md` 一起落地：
- 统一将对象名解析为 `(schema, name)`，内部 full name 形如 `schema.name`
- 所有 DDL/DML 都用解析后的 full name 操作 TiKV schema key

在 `05` 未落地前，保持现状：`ObjectName` 只取最后一段 ident（`name.0.last()`），等价于只支持单 schema/默认 schema。

### 2) `RENAME TO`（表重命名）

#### 行为
- `ALTER TABLE old RENAME TO new`：
  - `new` 不存在则更新成功
  - `new` 已存在则 error（PG 行为）

#### 存储层变更
现有数据 key/index key 基于 `table_id`，与表名无关；重命名主要是 schema 元数据 key 的迁移：
- 增加 `TikvStore::rename_table_schema(txn, old_full, new_full)`：
  - 读取 old schema
  - 删除 old schema key
  - 写入 new schema key（更新 `TableSchema.name`）

#### 依赖更新（FK 引用）
`ForeignKeyConstraint.ref_table` 存储的是字符串表名。重命名父表需要更新所有引用它的外键定义：
- 扫描当前 keyspace 所有 table schema（`store.list_tables + get_schema`）
- 对每个 schema 的 `foreign_keys`，若 `ref_table == old_full` 则更新为 `new_full`

（可选）对视图/物化视图/存储过程：现有实现存 SQL 字符串（`_sys_view_*` 等），rename 后引用可能失效。MVP 建议不自动重写，但在文档中明确行为差异。

### 3) `RENAME COLUMN`（列重命名的元数据一致性）

当前实现已能更新 `columns/indexes`，但为了迁移稳定性，必须同步：
- `schema.foreign_keys[*].columns`：将引用旧列名的项替换为新列名（本表内 FK 约束）
- `schema.check_constraints[*].expr`：将 CHECK 表达式中出现的列标识符重写为新列名

CHECK 表达式重写策略（避免“字符串替换误伤”）：
1. 用 `sqlparser` 解析 `check.expr` 为 `Expr`
2. 使用 `visitor`（本仓库启用 `sqlparser` 的 `visitor` feature）遍历 AST，将 `Identifier/CompoundIdentifier` 的最后一段等于 `old_name` 的替换为 `new_name`
3. 将改写后的 AST `to_string()` 回写到 `check.expr`

备注：本项目的 FK 校验逻辑当前不使用 `ref_columns`（只按 `ref_table + pk values` 查找），因此“其他表引用本表列名”的同步属于 information_schema 正确性增强，MVP 可不做。

### 4) `DROP COLUMN`（RESTRICT + 原地重写）

#### 行为

- 默认按 RESTRICT：若列存在依赖（PK/INDEX/FK/CHECK），返回 error；不支持 `CASCADE`（MVP）
- `IF EXISTS`：列不存在则 no-op

#### 依赖校验（RESTRICT）

- PK：禁止 drop `pk_indices` 覆盖的列
- INDEX：若任一 `schema.indexes[*].columns` 包含该列，禁止 drop（先 drop index/constraint）
- FK（本表作为引用方）：若任一 `schema.foreign_keys[*].columns` 包含该列，禁止 drop（先 drop constraint）
- CHECK：若任一 `schema.check_constraints[*].expr` 引用该列，禁止 drop（先 drop/rename constraint）

备注：本项目当前 FK 校验不使用 `ref_columns`，因此“其他表引用本表列”的依赖校验属于增强项，MVP 可不做。

#### 数据重写

由于 row 采用“按列顺序存储 Vec<Value>”的物理格式，drop 中间列必须重写每行：
- 基于 data key range（`encode_table_data_range(table_id)`）做 `txn.scan()` 流式迭代
- `deserialize_row(pair.value())` 后先对旧 schema 做 `fill_row_defaults()`（补齐 add column 造成的短 row）
- `row.values.remove(col_idx)`，再 `serialize_row()` 并对 `pair.key()` 原地 `txn_put()`

最后更新 schema：
- `schema.columns.remove(col_idx)` + 调整 `pk_indices` 中大于 `col_idx` 的项
- `schema.version += 1`，`store.update_schema()`

### 5) `DROP CONSTRAINT`

pg-tikv 的约束在 schema 中分别存储：
- PK：`pk_indices` + `columns[].primary_key`
- UNIQUE：通过 `indexes[]`（unique=true）表达（且可能由约束名决定 index 名）
- CHECK：`check_constraints[]`
- FK：`foreign_keys[]`

因此 `DROP CONSTRAINT <name>` 的实现需要按名字匹配并做对应清理：
- `IF EXISTS`：未命中则 no-op
- `CASCADE`：MVP 不支持（等价于 RESTRICT）

#### FK
- 在 `schema.foreign_keys` 里按 `name` 精确匹配并 remove

#### CHECK
- 在 `schema.check_constraints` 里按 `name` 匹配并 remove
- 对于未命名的 CHECK（`name=None`），我们需要在 create/alter add constraint 时生成并保存稳定默认名，保证后续可 `DROP CONSTRAINT`（现有 `information_schema.check_constraints` 会用 `"{table}_check{i+1}"` 作为兜底展示名）

#### UNIQUE
- 约束名通常等价于 index 名（至少在 pg-tikv 内部可以这样做，减少歧义）
- drop 时：
  - 从 `schema.indexes` 移除对应 `IndexDef`
  - 清理已有 index entries：
    - 推荐实现：按 index key 前缀扫描并删除（不依赖逐行计算 index key，避免 materialize rows）
    - 复用现有实现也可：扫描 rows + `delete_index_entry`（参考 `execute_drop_index`，但大表迁移风险更高）

#### PRIMARY KEY
是否允许 drop PK 需要明确：
- 若系统要求每表必须有 PK，则 `DROP CONSTRAINT <pk>` 返回 error（迁移中较少见）。
- 若允许 drop，则需要定义“无 PK 表”的 key 编码/更新删除语义（风险大，不建议在 MVP 做）。

### 6) `RENAME CONSTRAINT`

`ALTER TABLE ... RENAME CONSTRAINT old TO new` 只涉及元数据：
- FK：更新 `foreign_keys[*].name`
- CHECK：更新 `check_constraints[*].name`（若原本 `None`，则只有当 `old` 命中默认生成名时才允许 rename）
- UNIQUE：更新 `indexes[*].name`（底层 index entries 以 `index_id` 编码，不需要搬迁 KV）

MVP 不支持重命名 PK 约束名（当前 schema 未保存 pk constraint name；若后续要支持，需要在 `TableSchema` 中补充 `pk_constraint_name: Option<String>` 并做 serde default 兼容）。

### 7) `ADD CONSTRAINT ... CHECK (...)`

把 CHECK 作为 schema 元数据的一等公民：
- `schema.check_constraints.push(CheckConstraint { name, expr })`

并在添加时做验证（PG 默认 `VALIDATE`）：
- 扫描现有 rows
- **解析一次、复用 AST**：先把 `expr` parse 成 `sqlparser::ast::Expr`，然后对每行调用 `eval_expr`
- 任一行为 false 或 NULL => error

（可选优化）支持 `NOT VALID`：先存元数据不验证，后续 `VALIDATE CONSTRAINT` 再验证。

### 8) `ALTER COLUMN`（DEFAULT / NOT NULL）

#### SET/DROP DEFAULT
- 仅更新 `ColumnDef.default_expr`（字符串形式），无需重写数据
- 需影响 `information_schema.columns.column_default`

#### SET NOT NULL
- 更新 `ColumnDef.nullable=false`
- 更新前必须验证现有数据：扫描 rows，若该列出现 NULL 则 error

#### DROP NOT NULL
- 直接更新 `ColumnDef.nullable=true`

### 9) `ALTER COLUMN TYPE`

迁移中常见的类型变更（比如 int4->int8、text->jsonb、varchar->text 等）。建议 MVP 只做“可安全转换”的集合：
- `INT4 <-> INT8`
- `TEXT <-> JSONB`（语义：TEXT 到 JSONB 需要 parse，否则 error）
- `TEXT <-> UUID`（parse 失败则 error）
- 以及其它可直接复用 `coerce_value_for_column()` 的转换

实现方式：
1. 更新 schema 中该列的 `data_type`
2. 要求 `using.is_none()`，且该列不在 `pk_indices` 中（MVP）
3. 扫描全表 rows，逐行把旧值转换为新值并回写（同 key 原地更新）
4. 对涉及该列的索引：
   - 建议“先清空 index key 前缀，再按新值重建 index entries”（避免依赖旧值逐行删除）

注意：这是一次性全表重写，迁移可接受但要在文档中明确成本。

## 性能与正确性注意（MVP 必须遵守）

### 避免全表 materialize

当前 DDL/索引相关路径普遍会通过 `scan_and_fill()` 把整表加载到 `Vec<Row>`，这对大表迁移不具备生产可用性（内存占用不可控）。

本设计要求：凡是需要遍历全表的 ALTER 分支，改为基于 TiKV `txn.scan()` 的流式迭代，逐行处理并回写，避免把所有 row 放入内存。

### 回写策略：优先原地更新 row value

对不改变 PK 的操作（`DROP COLUMN`、`SET/DROP DEFAULT`、`SET/DROP NOT NULL`、非 PK 的 `ALTER COLUMN TYPE`），row key 不变：
- 直接使用 `txn.scan()` 返回的 `pair.key()` 作为 data key
- `deserialize_row(pair.value())` → 变换 → `serialize_row()` → `txn_put(txn, data_key, bytes)`

这样可以避免依赖 `store.upsert()` 的“根据 schema 重新计算 PK → encode key”路径，降低错误风险与额外开销。

## 实现步骤（建议）

1. 先补齐 `RenameTable` / `DropConstraint` / `RenameConstraint` / `ALTER COLUMN DEFAULT/NULL`（不涉及数据重写，或仅需轻量验证）
2. 修正 `RENAME COLUMN` / `DROP COLUMN` 的元数据一致性与依赖校验（FK/CHECK）
3. 再补齐受控的 `ALTER COLUMN TYPE`（非 PK、无 USING，含索引重建）
4. 最后处理 rename 对 view/matview/procedure 的依赖更新（可选增强）

## 测试计划

### 单元测试（`cargo test`）

建议新增纯逻辑测试（不依赖 TiKV）：
- 约束名解析：给定 schema + constraint name，能定位到 FK/CHECK/UNIQUE 的正确分支
- 生成默认 constraint 名的一致性（未命名 CHECK）
- CHECK 表达式重写：`RENAME COLUMN` 后 `check_constraints.expr` 能正确替换列标识符（不误伤字符串字面量）

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/38_alter_table_migration.sql`，覆盖：
- `RENAME TO`：重命名后可查询，且原名不可见
- `RENAME COLUMN`：含 FK/CHECK 的表重命名列后仍能正常插入/更新（约束不悬挂）
- `RENAME CONSTRAINT`：FK/UNIQUE/CHECK rename 后仍生效，且 `information_schema` 名称更新
- `DROP COLUMN`：
  - RESTRICT：列被 FK/CHECK/INDEX 引用时应报错
  - 无依赖时能正确移除中间列（包含 drop 列位于复合 PK 列之前的场景）
- `DROP CONSTRAINT`：
  - UNIQUE：drop 后允许插入重复值
  - FK：drop 后允许插入不存在的引用（或至少不再报 FK error）
  - CHECK：drop 后允许插入违规数据
- `ADD CONSTRAINT CHECK`：添加时验证现有数据；违规则报错
- `ALTER COLUMN SET NOT NULL`：现有 NULL -> 报错；无 NULL -> 生效
- `ALTER COLUMN SET/DROP DEFAULT`：插入缺省列时默认值生效/不生效
- `ALTER COLUMN TYPE`：受控类型集合内转换成功；不合法值报错且不部分写入

### ORM 测试（`./run_tests.sh` 的 ORM 阶段）

补齐/新增 migration 覆盖：
- TypeORM：`orm-tests/typeorm/schema.test.ts`（或新增文件）执行一组 rename/drop constraint/alter column 迁移 SQL
- Sequelize/Knex：验证其 schema builder 生成的 ALTER TABLE 语句能跑通（至少覆盖 rename/drop constraint/default/not null）
