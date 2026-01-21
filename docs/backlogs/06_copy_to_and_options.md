# 设计：COPY TO / COPY 选项（pg_dump/迁移工具兼容）

**Status**: Draft  
**Priority（ORM 迁移）**: P1（迁移间接依赖，pg_dump/restore 常用）

## 背景与动机

pg-tikv 当前已经通过 pgwire 实现了 `COPY ... FROM STDIN`（用于 `pg_restore`/大批量导入），但仍存在几个关键缺口：
- 缺 `COPY ... TO STDOUT`（`pg_dump` 常用，很多工具也用）
- `COPY` 在 SQL 层被直接 skip（`src/sql/helpers.rs`），协议层只实现了 CopyIn 流程（`src/protocol/handler.rs`）
- `COPY` 的常见 `WITH (...)` 选项（CSV/HEADER/DELIMITER/NULL 等）未覆盖

虽然这不一定是 ORM migration 的“硬前置”，但在实际工程中常与迁移/初始化/备份恢复一起使用，优先级较高。

## 目标（MVP）

- 支持 `COPY <table> [(cols...)] TO STDOUT`（文本格式，PG 默认格式）
- 保持现有 `COPY ... FROM STDIN` 兼容
- 明确支持/不支持的 `WITH` 选项，并对不支持的选项给出清晰错误

## 非目标（MVP 不做）

- COPY binary 格式
- 完整 CSV 方言（可先支持 `FORMAT csv` + `HEADER` 的子集）
- 流式执行器（当前执行器会物化 `Vec<Row>`，先保证功能，再做 streaming）

## 设计概览

### 1) 协议层：新增 CopyOut 流程

现有 `SimpleQueryHandler` 在检测到 `COPY ... FROM STDIN` 时返回 `Response::CopyIn(...)`。

新增检测 `COPY ... TO STDOUT`：
- 增加 `parse_copy_to_command(query) -> (table, columns, options)`
- 返回 `Response::CopyOut(...)`（pgwire crate 通常提供 CopyOut 响应；若版本不支持，需要在 handler 层用 message 级别实现）

CopyOut 的数据体是多段 `CopyData`：
- 每段是一段 bytes（按行输出即可）
- 以 `CopyDone` 结束

### 2) 输出格式：先实现 PG 默认 text

PG 默认 text COPY 格式：
- 列之间以 `\t` 分隔
- 行以 `\n` 结束
- NULL 用 `\\N` 表示
- 特殊字符转义（`\n`, `\t`, `\\` 等）

MVP：
- 仅支持 `FORMAT text`（默认）
- 对 `FORMAT csv`/其它选项：先报“不支持”，或实现一个最小子集（可选）

### 3) 与执行器的衔接

实现方式（MVP）：
- 将 `COPY table [(cols)] TO STDOUT` 等价改写为 `SELECT <cols> FROM <table>` 并调用现有 `executor.execute(...)`
- 取回 `ExecuteResult::Select` 的 rows，逐行编码为 COPY text 行并发送

性能注意：
- 避免在编码过程中反复分配：用一个可复用的 `Vec<u8>` buffer（逐行 clear）
- 大表会产生大量输出，MVP 仍会在执行器侧物化 rows；后续可引入 streaming 执行器再优化

### 4) COPY FROM STDIN 的选项与语法扩展（增量）

现有 CopyIn 解析使用 regex，支持：
- `COPY table (col1, col2, ...) FROM stdin`
- `COPY table FROM stdin`

增量建议：
- 支持 schema-qualified：`COPY public.table ...`
- 支持 `WITH (...)`：至少解析并忽略 `FORMAT text` / `DELIMITER`（若不支持则报错）

## 实现步骤（建议）

1. handler 增加 `COPY ... TO STDOUT` 检测与 CopyOut 响应
2. 复用 executor 执行 `SELECT` 并做 text 编码
3. 扩展 `parse_copy_command` 支持 schema 与 WITH 子句（可选）
4. 在集成测试中补齐 pg_dump/psql 级验证

## 测试计划

### 单元测试（`cargo test`）

- COPY text 编码：NULL/特殊字符/数组/bytea 的文本格式（至少保证稳定且可被 `COPY FROM` 读回）
- `parse_copy_to_command` 的语法覆盖（带/不带列名、带 schema、带 WITH）

### 集成测试（推荐 Python/psql，`./run_tests.sh`）

由于 COPY 是协议级行为，纯 SQL 文件难以验证输出内容，建议新增 `scripts/integration_test.py` 用例：
- 建表 + 插入包含 NULL、制表符、换行的文本
- 运行 `psql -c "COPY ... TO STDOUT"` 捕获 stdout
- 校验输出行数与内容（至少字段分隔正确，NULL 为 `\\N`）

（可选）pg_dump 兼容性：
- 用 `pg_dump --data-only --column-inserts` 或 `pg_dump --data-only` 生成数据
- 结合 `pg_restore`/`psql` 做往返（需要额外脚本支持）

### ORM 测试

ORM 迁移阶段通常不直接用 COPY，但一些生态工具会用（数据种子/备份恢复）。测试策略：
- 在 `orm-tests/pg-client` 增加一个最小用例：通过 node-postgres 执行 `COPY ... TO STDOUT` 并读取流（若测试框架允许）

