# 设计：Round 3 兼容性补齐（已实现）

**Status**: Implemented  
**重点覆盖**：`generate_series`、`INSERT ... SELECT`、GROUPING SETS/CUBE/ROLLUP、regex 运算符/函数、JSONB 高级操作、系统函数/格式化（`PG_TYPEOF`/`QUOTE_*`/`FORMAT`）  
**测试**：`tests/67_*.sql` ~ `tests/80_*.sql`

本文件最初用于记录“缺口功能”的实现计划；目前相关功能已在引擎中落地，并由新增的 SQL/ORM 测试覆盖。后续如有新增缺口，可继续在此补充。

## 已落地能力（摘要）

- **Set-returning functions**：`generate_series(...)` 可在 `FROM`/`JOIN` 中使用（含 int/numeric/date/timestamp+interval），并修复边界溢出导致的死循环风险。
- **DML 兼容**：`INSERT ... SELECT` 支持更多类型的 round-trip（`bytea`、`timestamp` 等），避免 panic/类型丢失。
- **GROUP BY 扩展**：支持 `GROUPING SETS` / `ROLLUP` / `CUBE`，并保证 `CUBE ((a,b), c)` 这类“分组项作为整体”的语义。
- **Regex**：支持 `~`/`~*`/`!~`/`!~*` 以及 `REGEXP_REPLACE` / `REGEXP_MATCHES` / `REGEXP_SPLIT_TO_TABLE` / `REGEXP_SPLIT_TO_ARRAY`。
- **JSONB**：
  - 运算符：`?`/`?|`/`?&`、`#>`/`#>>`、`#-`、`||`、`-`（key / index）等
  - SRF：`jsonb_each*` / `jsonb_array_elements*` 等（通过执行器 SRF 通道展开）
  - `TO_JSONB(ROW(...))` / `ROW_TO_JSON(ROW(...))` 输出符合预期的 `f1/f2/...` key
- **系统函数 & 格式化**：
  - `PG_TYPEOF`：数组返回具体元素数组类型（如 `integer[]`）
  - `QUOTE_IDENT`：使用 sqlparser 关键字全集判断是否需要加引号，并正确转义 `"`
  - `FORMAT`：区分宽度与 positional（`%10s` vs `%1$s`），`%I` 行为与 `quote_ident` 对齐

## 测试覆盖（67-80）

- `tests/67_set_operations.sql`：UNION/INTERSECT/EXCEPT
- `tests/68_distinct_on.sql`：DISTINCT ON
- `tests/69_generate_series.sql`：generate_series 全套重载 + join
- `tests/70_regex_functions.sql`：regex 运算符/函数
- `tests/71_lateral_join.sql`：LATERAL JOIN
- `tests/72_json_advanced.sql`：JSONB 高级操作
- `tests/73_system_functions.sql`：系统函数/FORMAT/QUOTE_*
- `tests/74_filter_clause.sql`：聚合 FILTER
- `tests/75_grouping_sets.sql`：GROUPING SETS/CUBE/ROLLUP + GROUPING()
- `tests/76_common_table_expressions.sql`：递归 CTE + MATERIALIZED/NOT MATERIALIZED 兼容
- `tests/77_insert_variants.sql`：INSERT 变体（RETURNING/SELECT/ON CONFLICT）
- `tests/78_update_delete_variants.sql`：UPDATE FROM / DELETE USING 等
- `tests/79_comparison_operators.sql`：比较/三值逻辑/行比较等
- `tests/80_string_operations.sql`：常用字符串函数/LIKE/FORMAT/QUOTE
