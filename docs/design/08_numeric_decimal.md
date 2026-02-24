# 设计：NUMERIC / DECIMAL（精确小数）

**Status**: Draft  
**Priority（ORM 迁移）**: P0

## 背景与动机

当前 `sqlparser` 的 `NUMERIC/DECIMAL` 在 db9-server 内被映射为 `FLOAT8`（`src/sql/helpers.rs:349`），这会带来：
- 精度丢失（金额/计费/统计场景严重）
- ORM 类型映射不一致（TypeORM/Prisma 的 Decimal 语义）

迁移优先视角下，NUMERIC/DECIMAL 是非常高频的列类型，必须补齐“精确小数”的基础能力。

## 目标（MVP）

- 支持 `NUMERIC(p,s)` / `DECIMAL(p,s)` 的列定义（`p/s` 可选）
- 存储与比较为**精确**小数（不使用 f64）
- 支持基本操作：
  - 解析/输出（text）
  - 比较运算（`=, <, >, <=, >=`）
  - 简单算术（`+,-,*`；`/` 可后置）
- 与 ORM 交互：
  - `information_schema.columns` 正确显示 precision/scale（至少能让 ORM 不误判为 float）

## 非目标（MVP 不做）

- 完整 PostgreSQL arbitrary precision numeric 的所有边界行为（先满足常见 `p<=38`、`s<=18` 等工程范围）
- 全部内建 numeric 函数（round/ceil 等可增量补）

## 设计选型

### 方案 A：`i128 + scale`（推荐，性能/实现复杂度平衡）

引入：

```rust
DataType::Numeric { precision: Option<u32>, scale: Option<u32> }
Value::Numeric { unscaled: i128, scale: u32 }
```

- `unscaled` 表示去掉小数点后的整数
- `scale` 表示小数位数

优点：
- 纯整数运算，高性能
- 序列化/反序列化简单（bincode 支持 i128）
- 避免引入重量级大数依赖

限制：
- 受限于 i128 的范围；需要在 parse/运算时做溢出检测并报错

### 方案 B：引入 decimal/bigdecimal crate（后续）

更接近 PG 的任意精度，但引入依赖与额外分配/运算成本。

## 设计细节（方案 A）

### 1) 解析与规范化

- 字面量解析：
  - `'123.45'::numeric` / `NUMERIC '123.45'` / `CAST(... AS NUMERIC)`
  - `SqlValue::Number("123.45", ...)`：拆分整数/小数部分，得到 `unscaled=12345, scale=2`
- 写入列时：
  - 若列定义有 `scale`：对 value 做 rescale（必要时四舍五入/截断，需对齐 PG 规则并写明）
  - 若列定义无 scale：保留 value 自身 scale

### 2) 比较

比较时对齐 scale：
- `a.scale == b.scale`：直接比较 unscaled
- 否则将较小 scale 的一侧乘以 `10^(delta)` 后比较（注意溢出）

### 3) 算术

MVP 建议：
- `+/-`：先对齐 scale，再加减
- `*`：`scale = a.scale + b.scale`，`unscaled = a.unscaled * b.unscaled`（溢出检测）
- `/`：先不在 MVP 做通用除法（或仅实现“被整除/保留固定 scale”的受限版本），避免引入复杂 rounding 规则

### 4) 存储与 wire 输出

- 存储：`Value::Numeric` bincode 序列化
- 输出：text 表达为带小数点的十进制字符串

### 5) DDL 与 introspection

- `convert_data_type()`：
  - `SqlDataType::Numeric/Decimal` => `DataType::Numeric { precision, scale }`
- `information_schema.columns`：
  - `numeric_precision` / `numeric_scale` 输出对应值

## 实现步骤（建议）

1. 扩展 `DataType/Value`（新增 Numeric 变体，注意 bincode 兼容：只追加不重排）
2. 扩展类型转换与表达式求值（parse/compare/+,-,*）
3. 补齐 `coerce_value_for_column()` 对 numeric 的 cast/parse
4. 补齐 `information_schema` 输出（precision/scale）
5. 增加 SQL/ORM 测试

## 测试计划

### 单元测试（`cargo test`）

- 解析：
  - `"0"`, `"1.0"`, `"-12.3400"` 的 unscaled/scale
- 比较：
  - `1.2 == 1.20`、`1.2 < 1.21`
- 运算：
  - `1.2 + 3.45 = 4.65`
  - `2.5 * 4 = 10.0`（scale 行为固定）
- 溢出：超出 i128 报错

### 集成测试（SQL，`./run_tests.sh`）

新增 `tests/43_numeric_decimal.sql`：
- 建表 `DECIMAL(10,2)`，插入/选择/比较
- `SUM/AVG`（若暂时用 FLOAT64 聚合则明确差异；理想是后续扩展 aggregate）
- `information_schema.columns` 中 precision/scale 可见

### ORM 测试

建议优先：
- TypeORM：decimal 列 round-trip（写入 string/number 读取一致）
- Knex：decimal schema + where 比较

（注：Prisma 测试当前在 `run_tests.sh` 被跳过，但仍建议保留设计与测试计划，后续修复 Prisma boolean 问题后恢复。）

