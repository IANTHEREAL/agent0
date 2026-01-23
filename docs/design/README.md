# 设计文档索引（ORM 迁移优先）

本目录收录 pg-tikv 在“PostgreSQL 应用开发者 / ORM 迁移”视角下的关键缺口设计与测试计划。每份文档都包含：
- **设计**：MVP 范围、语义、存储/执行/协议改动点、性能注意事项
- **测试计划**：单元测试、SQL 集成测试（`./run_tests.sh`）、ORM 测试建议

## P0（优先保障迁移可跑通）

- `docs/design/01_savepoints.md`：SAVEPOINT / ROLLBACK TO / RELEASE
- `docs/design/02_alter_table_migration.md`：ALTER TABLE 常用子集（rename/drop constraint/alter column）
- `docs/design/03_user_defined_types_enum_composite.md`：CREATE TYPE AS ENUM（迁移高频）
- `docs/design/04_sequences.md`：SEQUENCE + nextval/currval/setval + SERIAL 兼容
- `docs/design/05_schemas_search_path.md`：schema + search_path
- `docs/design/07_dollar_quoted_strings.md`：`$$...$$` / `$tag$...$tag$`
- `docs/design/08_numeric_decimal.md`：NUMERIC/DECIMAL 精确小数
- `docs/design/09_date_type.md`：DATE 类型
- `docs/design/11_system_catalog_coverage.md`：pg_catalog / information_schema 覆盖

## P1（迁移生态/工具链常用）

- `docs/design/06_copy_to_and_options.md`：COPY TO / 选项（pg_dump/restore）
- `docs/design/10_pgwire_array_vector_oids.md`：Array/Vector 的 pgwire OID
- `docs/design/12_functions_and_triggers.md`：允许 CREATE FUNCTION/TRIGGER 落地（先存储定义）
- `docs/design/14_index_features.md`：partial/expression/GIN/GiST（先 DDL 兼容）

## P2/P3（非迁移硬依赖，按需增强）

- `docs/design/17_extensions_framework_http.md`：扩展机制 + HTTP 扩展（Supabase 风格）
- `docs/design/16_set_returning_functions.md`：generate_series / unnest（FROM 子句）
- `docs/design/15_explain_analyze.md`：EXPLAIN ANALYZE
- `docs/design/13_listen_notify.md`：LISTEN/NOTIFY

## 性能优化

- `docs/design/18_hash_join.md`：Hash Join 实现（等值连接优化）
  - 设计概述：[18_hash_join.md](./18_hash_join.md)
  - 详细实现计划：[hash_join_implementation_plan.md](./hash_join_implementation_plan.md)
