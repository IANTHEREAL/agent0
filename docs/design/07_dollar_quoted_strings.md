# 设计：Dollar-Quoted Strings（`$$...$$` / `$tag$...$tag$`）

**Status**: Draft  
**Priority（ORM 迁移）**: P0

## 背景与动机

大量 Postgres 生态的迁移脚本/扩展/函数定义会使用 dollar-quoted string：
- `$$ ... $$`
- `$tag$ ... $tag$`

例如：
- `CREATE FUNCTION ... AS $$ ... $$ LANGUAGE plpgsql;`
- 纯 SQL 中把 `$$...$$` 当作字符串常量使用

当前 db9-server 在 parse 前直接拦截包含 `$$`/`$_$` 的 SQL，并返回 “Dollar-quoted strings not supported”（`src/sql/helpers.rs`），导致上述迁移脚本无法执行（甚至无法“跳过/存储定义”）。

## 目标（MVP）

- 允许 SQL 中出现 dollar-quoted string，并正确解析为字符串常量
- 修复解析辅助逻辑在 dollar-quote 场景下的误判：
  - `count_sql_parameters()`（`$1` 占位符计数）
  - `find_keyword_outside_strings()`（例如 RETURNING/关键字定位）
- 至少保证：
  - `SELECT $$abc$$;` 可执行并返回 `abc`
  - `CREATE FUNCTION ... AS $$...$$ ...` 不再被“预先拒绝”（是否执行由函数实现决定）

## 非目标（MVP 不做）

- PL/pgSQL 语义本身（先保证字符串字面量与解析器可用）
- 完整支持所有 dollar quote 变体的边界行为（先覆盖主流 `$$` 与 `$tag$`）

## 设计概览

### 1) 移除“预先拒绝”拦截

在 `src/sql/helpers.rs:get_unsupported_reason()` 中删除/放宽：
- `if sql_upper.contains("$_$") || sql_upper.contains("$$") { ... }`

优先让 `sqlparser-rs (0.40.0)` 直接解析 dollar-quoted string。

### 2) 表达式求值：支持 sqlparser 的 DollarQuoted value

当前 `eval_value()` 仅支持：
- `Null/Boolean/Number/SingleQuotedString/DoubleQuotedString`

需要补齐 sqlparser 的 dollar-quoted 变体（以 0.40.0 为准，通常为 `SqlValue::DollarQuotedString(...)` 或等价结构）：
- 将其视为 `Value::Text`
- 复用现有逻辑：若内容形如 `[...]` 可解析为 vector literal（可选）

### 3) 关键字扫描/参数计数：识别 dollar-quote 区间

`count_sql_parameters()` 与 `find_keyword_outside_strings()` 目前只跟踪 `'` 与 `"`。

在包含 dollar-quote 的 SQL 中：
- `$tag$ ... $tag$` 内部可能出现 `$1` 或 `RETURNING` 字样，不能被当作真实占位符/关键字

MVP 建议实现一个轻量扫描器：
- 在遍历字符时识别 dollar-quote 起始：
  - `$` + `[A-Za-z_0-9]*` + `$` 作为 tag（tag 可为空即 `$$`）
  - 进入 “in_dollar_quote(tag)” 状态
- 在该状态下，只有当再次遇到相同 tag 结束符时才退出
- 处于 dollar-quote 状态时：
  - `count_sql_parameters` 不识别 `$<digits>`
  - `find_keyword_outside_strings` 不匹配 keyword

性能：该扫描器只用于解析前的辅助处理（Describe/RETURNING/参数替换），不会进入行级热路径。

## 实现步骤（建议）

1. 放宽 unsupported 拦截
2. 在 `eval_value()` 中支持 dollar-quoted value
3. 修复关键字扫描与参数计数对 dollar-quote 的识别
4. 增加集成测试覆盖

## 测试计划

### 单元测试（`cargo test`）

- `count_sql_parameters()`：
  - `SELECT $$ $1 $$, $1;` => 只计到 1 个参数
- `find_keyword_outside_strings()`：
  - `INSERT ... $$ RETURNING $$ RETURNING id` => RETURNING 位置应指向真实 RETURNING
- `eval_value()`：
  - dollar-quoted string => `Value::Text`，内容保持原样

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/42_dollar_quote.sql`：
- `SELECT $$hello$$ as v;`
- `SELECT $tag$hello$tag$ as v;`
- `CREATE FUNCTION ... AS $$ ... $$ ...;`（至少确保不因 parser/skip 失败）

### ORM/迁移工具验证（建议）

选取一个真实迁移片段（例如 TypeORM migration 中的 `CREATE FUNCTION ... $$`）在 `orm-tests/typeorm/` 增加 smoke test：
- 执行该 SQL，确保不报 parser/unsupported 错误

