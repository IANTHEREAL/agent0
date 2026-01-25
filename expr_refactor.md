# Expression Evaluation Refactoring Plan

**Created:** 2025-01-24  
**Status:** Draft  
**Target Files:** `src/sql/expr.rs` (~6300 lines)

## Executive Summary

当前 `expr.rs` 存在严重的可维护性问题：
- **代码重复**: `eval_expr` 与 `eval_expr_join` 两套平行实现 (~3000 行重复)
- **巨型函数**: `eval_function` 单函数 2500+ 行，150+ 个函数分支
- **类型爆炸**: 算术运算 20+ 个类型组合分支
- **性能问题**: 热路径上的字符串分配、重复正则编译

本计划分 **5 个阶段**，预计 **8-12 周**完成。

---

## Phase 0: 准备工作 (Week 1)

### 0.1 建立基准测试

**目标**: 确保重构不引入性能回归

**任务**:
- [ ] 创建 `benches/expr_eval.rs`，覆盖：
  - 简单表达式: `a + b`, `x > 10`
  - 函数调用: `UPPER(name)`, `LENGTH(text)`
  - 复杂表达式: `CASE WHEN ... THEN ... END`
  - JSON 操作: `data->'key'->>'value'`
  - JOIN 场景: 多表条件评估
- [ ] 记录当前性能基准

**验收标准**:
```bash
cargo bench --bench expr_eval
# 产出 baseline.json
```

### 0.2 增加测试覆盖

**目标**: 为重构建立安全网

**任务**:
- [ ] 审计现有单元测试覆盖率
- [ ] 补充边界条件测试：
  - NULL 传播
  - 类型强制转换边界
  - 溢出处理
- [ ] 确保所有 150+ 函数有至少一个测试

**验收标准**:
```bash
cargo test --lib -- expr::
# 全部通过，覆盖率 > 80%
```

---

## Phase 1: 统一 eval_expr 与 eval_expr_join (Week 2-3)

### 1.1 设计 EvalContext trait

**问题**: `eval_expr(expr, row, schema)` 和 `eval_expr_join(expr, ctx)` 有不同的上下文参数

**解决方案**: 引入统一的 `EvalContext` trait

```rust
// src/sql/expr/context.rs

/// Evaluation context for expression evaluation
pub trait EvalContext {
    /// Resolve a column identifier to its value
    fn resolve_column(&self, name: &str) -> Result<Value>;
    
    /// Resolve a qualified column (table.column) to its value
    fn resolve_qualified_column(&self, table: &str, column: &str) -> Result<Value>;
    
    /// Get the schema for type information (optional)
    fn schema(&self) -> Option<&TableSchema>;
    
    /// Check if an expression yields TIMESTAMPTZ
    fn is_timestamptz(&self, expr: &Expr) -> bool;
}

/// Context for single-table evaluation
pub struct SingleTableContext<'a> {
    row: Option<&'a Row>,
    schema: Option<&'a TableSchema>,
}

/// Context for JOIN evaluation
pub struct JoinContext<'a> {
    pub column_offsets: HashMap<String, usize>,
    pub combined_row: &'a Row,
    pub combined_schema: &'a TableSchema,
}
```

**任务**:
- [ ] 创建 `src/sql/expr/context.rs`
- [ ] 实现 `SingleTableContext`
- [ ] 迁移现有 `JoinContext` 实现 `EvalContext`
- [ ] 添加单元测试

### 1.2 重写 eval_expr 为泛型

**任务**:
- [ ] 创建新函数签名:
  ```rust
  pub fn eval_expr<C: EvalContext>(expr: &Expr, ctx: &C) -> Result<Value>
  ```
- [ ] 逐个迁移 match arms，验证每个 arm 后运行测试
- [ ] 保留旧函数作为 wrapper (向后兼容):
  ```rust
  // Deprecated: use eval_expr with SingleTableContext
  pub fn eval_expr_legacy(expr: &Expr, row: Option<&Row>, schema: Option<&TableSchema>) -> Result<Value> {
      let ctx = SingleTableContext::new(row, schema);
      eval_expr(expr, &ctx)
  }
  ```

**风险控制**:
- 每次修改后运行完整测试套件
- 使用 `#[deprecated]` 标记旧 API，给下游适配时间

### 1.3 删除重复代码

**任务**:
- [ ] 删除 `eval_expr_join_impl`
- [ ] 删除 `eval_function_join`
- [ ] 删除 `eval_substring_join`, `eval_extract_join`, `eval_overlay_join`
- [ ] 删除 `eval_json_access_expr_join`, `eval_array_index_join`
- [ ] 更新所有调用方

**预期收益**: 减少 ~3000 行代码

**验收标准**:
```bash
wc -l src/sql/expr.rs  # < 3500 lines
cargo test
python3 scripts/integration_test.py
cd orm-tests && npm test
```

---

## Phase 2: 函数分发重构 (Week 4-6)

### 2.1 设计函数注册表

**问题**: 2500 行的 `match func_name.to_uppercase().as_str() { ... }`

**解决方案**: 使用 `phf` (perfect hash function) 生成编译期哈希表

```rust
// src/sql/expr/functions/registry.rs

use phf::phf_map;

pub type SqlFn = fn(args: Vec<Value>, ctx: &dyn EvalContext) -> Result<Value>;

pub static FUNCTIONS: phf::Map<&'static str, SqlFn> = phf_map! {
    "COALESCE" => funcs::coalesce,
    "NULLIF" => funcs::nullif,
    "UPPER" => funcs::string::upper,
    "LOWER" => funcs::string::lower,
    "LENGTH" => funcs::string::length,
    // ... 150+ entries
};

pub fn eval_function(name: &str, args: Vec<Value>, ctx: &dyn EvalContext) -> Result<Value> {
    let name_upper = name.to_ascii_uppercase();
    match FUNCTIONS.get(name_upper.as_str()) {
        Some(func) => func(args, ctx),
        None => Err(anyhow!("Unknown function: {}", name)),
    }
}
```

**任务**:
- [ ] 添加 `phf` 依赖
- [ ] 创建 `src/sql/expr/functions/registry.rs`
- [ ] 定义 `SqlFn` 类型和注册表结构

### 2.2 按类别拆分函数实现

**目标目录结构**:
```
src/sql/expr/functions/
├── mod.rs           # 导出 + registry
├── registry.rs      # phf_map 定义
├── string.rs        # UPPER, LOWER, LENGTH, CONCAT, LEFT, RIGHT, TRIM, etc.
├── math.rs          # ABS, CEIL, FLOOR, ROUND, SQRT, POWER, etc.
├── datetime.rs      # NOW, DATE_TRUNC, EXTRACT, AGE, TO_CHAR, etc.
├── json.rs          # JSONB_*, JSON_*, TO_JSON, TO_JSONB
├── array.rs         # ARRAY_*, UNNEST, STRING_TO_ARRAY, etc.
├── regex.rs         # REGEXP_REPLACE, REGEXP_MATCHES, REGEXP_SPLIT_TO_ARRAY
├── uuid.rs          # GEN_RANDOM_UUID, UUID_GENERATE_V4, UUIDV7
├── pg_compat.rs     # PG_BACKEND_PID, VERSION, CURRENT_DATABASE, etc.
└── misc.rs          # COALESCE, NULLIF, GREATEST, LEAST, FORMAT
```

**任务** (每个文件):
- [ ] 创建 `string.rs`，迁移 ~25 个字符串函数
- [ ] 创建 `math.rs`，迁移 ~20 个数学函数
- [ ] 创建 `datetime.rs`，迁移 ~15 个日期时间函数
- [ ] 创建 `json.rs`，迁移 ~20 个 JSON 函数
- [ ] 创建 `array.rs`，迁移 ~15 个数组函数
- [ ] 创建 `regex.rs`，迁移 ~5 个正则函数
- [ ] 创建 `uuid.rs`，迁移 ~3 个 UUID 函数
- [ ] 创建 `pg_compat.rs`，迁移 ~20 个 PG 兼容函数
- [ ] 创建 `misc.rs`，迁移剩余函数

**每个函数的迁移模式**:
```rust
// Before (in giant match)
"UPPER" => match args.into_iter().next() {
    Some(Value::Text(s)) => Ok(Value::Text(s.to_uppercase())),
    Some(v) => Ok(v),
    None => Ok(Value::Null),
},

// After (in string.rs)
pub fn upper(args: Vec<Value>, _ctx: &dyn EvalContext) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s.to_uppercase())),
        Some(v) => Ok(v),
        None => Ok(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_upper() {
        assert_eq!(upper(vec![Value::Text("hello".into())], &NoopContext).unwrap(), 
                   Value::Text("HELLO".into()));
    }
}
```

### 2.3 消除字符串 uppercase 开销

**当前**:
```rust
match func_name.to_uppercase().as_str() { ... }  // 每次分配
```

**优化**:
```rust
// phf_map 使用 &'static str 键，但查询时需要处理大小写
// 方案 A: 所有键都是大写，查询时转换一次
let key = func_name.to_ascii_uppercase();  // 栈上分配 if ASCII
FUNCTIONS.get(key.as_str())

// 方案 B: 使用 unicase::Ascii 或自定义 case-insensitive hasher
```

**验收标准**:
```bash
cargo bench --bench expr_eval
# 函数调用性能提升 > 10%
```

---

## Phase 3: 类型系统优化 (Week 7-8)

### 3.1 引入 NumericValue 统一类型

**问题**: `add_values` 等有 20+ 个 match arms

**解决方案**: 引入中间表示

```rust
// src/sql/expr/numeric.rs

/// Unified numeric representation for arithmetic operations
pub enum NumericValue {
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Decimal(Decimal),
}

impl NumericValue {
    pub fn from_value(v: Value) -> Option<Self> {
        match v {
            Value::Int32(n) => Some(NumericValue::Int32(n)),
            Value::Int64(n) => Some(NumericValue::Int64(n)),
            Value::Float64(n) => Some(NumericValue::Float64(n)),
            Value::Numeric(d) => Some(NumericValue::Decimal(d)),
            Value::Text(s) => Self::try_parse(&s),
            _ => None,
        }
    }
    
    /// Coerce two numeric values to a common type for arithmetic
    pub fn coerce_pair(l: NumericValue, r: NumericValue) -> (NumericValue, NumericValue) {
        use NumericValue::*;
        match (&l, &r) {
            // Same types: no coercion needed
            (Int32(_), Int32(_)) => (l, r),
            (Int64(_), Int64(_)) => (l, r),
            (Float64(_), Float64(_)) => (l, r),
            (Decimal(_), Decimal(_)) => (l, r),
            
            // Int32 + Int64 -> Int64
            (Int32(a), Int64(_)) => (Int64(*a as i64), r),
            (Int64(_), Int32(b)) => (l, Int64(*b as i64)),
            
            // Any + Float64 -> Float64
            (Float64(_), _) => (l, r.to_float64()),
            (_, Float64(_)) => (l.to_float64(), r),
            
            // Any + Decimal -> Decimal (except Float64)
            (Decimal(_), _) => (l, r.to_decimal()),
            (_, Decimal(_)) => (l.to_decimal(), r),
        }
    }
}
```

**任务**:
- [ ] 创建 `src/sql/expr/numeric.rs`
- [ ] 实现 `NumericValue` 和 `coerce_pair`
- [ ] 重写 `add_values`:
  ```rust
  fn add_values(left: Value, right: Value) -> Result<Value> {
      // Handle special cases first
      match (&left, &right) {
          (Value::Timestamp(ts), Value::Interval(iv)) => { ... }
          (Value::Interval(iv), Value::Timestamp(ts)) => { ... }
          // ... other non-numeric cases
          _ => {}
      }
      
      // Numeric arithmetic
      let l = NumericValue::from_value(left).ok_or_else(|| anyhow!("..."))?;
      let r = NumericValue::from_value(right).ok_or_else(|| anyhow!("..."))?;
      let (l, r) = NumericValue::coerce_pair(l, r);
      
      match (l, r) {
          (NumericValue::Int32(a), NumericValue::Int32(b)) => Ok(Value::Int32(a + b)),
          (NumericValue::Int64(a), NumericValue::Int64(b)) => Ok(Value::Int64(a + b)),
          (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(Value::Float64(a + b)),
          (NumericValue::Decimal(a), NumericValue::Decimal(b)) => Ok(Value::Numeric(a + b)),
          _ => unreachable!("coerce_pair ensures same types"),
      }
  }
  ```
- [ ] 同样重写 `sub_values`, `mul_values`, `div_values`, `mod_values`

**预期收益**: 每个算术函数从 ~50 行减少到 ~20 行

### 3.2 统一 compare_values

**任务**:
- [ ] 引入 `Comparable` trait 或使用 `NumericValue` 进行比较
- [ ] 减少 `compare_values` 中的类型组合

---

## Phase 4: 性能优化 (Week 9-10)

### 4.1 正则表达式缓存

**问题**: `WHERE col ~ 'pattern'` 每行都编译正则

**解决方案**: 使用 `thread_local` LRU 缓存

```rust
// src/sql/expr/regex_cache.rs

use lru::LruCache;
use std::cell::RefCell;

thread_local! {
    static REGEX_CACHE: RefCell<LruCache<String, regex::Regex>> = 
        RefCell::new(LruCache::new(std::num::NonZeroUsize::new(64).unwrap()));
}

pub fn get_or_compile(pattern: &str) -> Result<regex::Regex> {
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = regex::Regex::new(pattern)?;
        cache.put(pattern.to_string(), re.clone());
        Ok(re)
    })
}
```

**任务**:
- [ ] 添加 `lru` 依赖
- [ ] 创建 `src/sql/expr/regex_cache.rs`
- [ ] 替换所有 `regex::Regex::new` 调用

### 4.2 减少 Value clone

**问题**: `eval_binary_op(left_val.clone(), ...)` 在循环中

**解决方案**:
```rust
// Before
for elem in arr {
    let cmp = eval_binary_op(left_val.clone(), compare_op, elem)?;
}

// After: Use references where possible
for elem in &arr {
    if compare_op_matches(&left_val, compare_op, elem)? {
        return Ok(Value::Boolean(true));
    }
}
```

**任务**:
- [ ] 审计所有 `.clone()` 调用
- [ ] 引入 `compare_op_matches(left: &Value, op, right: &Value) -> Result<bool>`
- [ ] 对于必须 clone 的场景，使用 `Cow<Value>` 或 `Arc<Value>`

### 4.3 JSON 链式访问优化

**问题**: `data->'a'->'b'->'c'` 每层都 parse/serialize

**解决方案**: 保持 `serde_json::Value` 在链式访问中

```rust
// 新增内部表示
enum JsonIntermediate {
    Parsed(serde_json::Value),
    String(String),
}

fn eval_json_access_chain(left: &Expr, ops: &[(&Expr, &JsonOperator)], ctx: &C) -> Result<Value> {
    let mut current = eval_expr(left, ctx)?;
    let mut json_val: Option<serde_json::Value> = None;
    
    for (key_expr, op) in ops {
        // Lazy parse: only parse once
        if json_val.is_none() {
            json_val = Some(parse_json_from_value(&current)?);
        }
        
        let key = eval_expr(key_expr, ctx)?;
        let jv = json_val.as_mut().unwrap();
        
        // Mutate in place
        *jv = access_json(jv, op, &key)?;
    }
    
    // Only serialize at the end
    match json_val {
        Some(jv) => Ok(Value::Jsonb(jv.to_string())),
        None => Ok(current),
    }
}
```

---

## Phase 5: 文件拆分与清理 (Week 11-12)

### 5.1 最终目录结构

```
src/sql/expr/
├── mod.rs              # 公开 API: eval_expr, compare_values, cast_value
├── context.rs          # EvalContext trait + implementations
├── eval.rs             # eval_expr 核心实现 (~500 lines)
├── binary_op.rs        # eval_binary_op, 算术 helpers (~300 lines)
├── compare.rs          # compare_values, compare_order_by_values (~200 lines)
├── cast.rs             # cast_value (~300 lines)
├── numeric.rs          # NumericValue 类型强制 (~150 lines)
├── json_access.rs      # JSON 操作符 (~200 lines)
├── regex_cache.rs      # 正则缓存 (~50 lines)
├── like.rs             # LIKE/ILIKE/SIMILAR TO 匹配 (~150 lines)
├── interval.rs         # Interval 解析和算术 (~150 lines)
├── functions/
│   ├── mod.rs          # 注册表 + 导出 (~100 lines)
│   ├── registry.rs     # phf_map (~200 lines)
│   ├── string.rs       # ~400 lines
│   ├── math.rs         # ~300 lines
│   ├── datetime.rs     # ~350 lines
│   ├── json.rs         # ~400 lines
│   ├── array.rs        # ~300 lines
│   ├── regex.rs        # ~150 lines
│   ├── uuid.rs         # ~80 lines
│   ├── pg_compat.rs    # ~200 lines
│   ├── vector.rs       # ~100 lines
│   └── misc.rs         # ~150 lines
└── tests/
    ├── context_tests.rs
    ├── binary_op_tests.rs
    ├── function_tests.rs
    └── integration.rs
```

**预期行数**: 总计 ~4000 行 (从 6300 行减少 ~37%)

### 5.2 更新 mod.rs 导出

```rust
// src/sql/expr/mod.rs

mod context;
mod eval;
mod binary_op;
mod compare;
mod cast;
mod numeric;
mod json_access;
mod regex_cache;
mod like;
mod interval;
mod functions;

pub use context::{EvalContext, SingleTableContext, JoinContext};
pub use eval::eval_expr;
pub use compare::{compare_values, compare_order_by_values};
pub use cast::cast_value;
pub use functions::eval_function;

// Deprecated re-exports for backward compatibility
#[deprecated(since = "0.2.0", note = "Use eval_expr with SingleTableContext")]
pub use eval::eval_expr_legacy;

#[deprecated(since = "0.2.0", note = "Use eval_expr with JoinContext")]
pub use eval::eval_expr_join_legacy;
```

### 5.3 更新调用方

**需要更新的文件**:
- [ ] `src/sql/executor.rs` - 主要调用方
- [ ] `src/sql/executor/core.rs`
- [ ] `src/sql/executor/operators.rs`
- [ ] `src/sql/helpers.rs`
- [ ] `src/sql/dml.rs`
- [ ] `src/sql/window.rs`
- [ ] `src/sql/aggregate.rs`
- [ ] `src/sql/operators/*.rs`

---

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| 引入回归 bug | 高 | 每阶段完整测试，保留旧 API |
| 性能下降 | 中 | 基准测试监控，可回滚 |
| 改动范围蔓延 | 中 | 严格按阶段执行，不 scope creep |
| ORM 测试失败 | 高 | 每次改动后跑 600+ ORM 测试 |

---

## 验收标准 (整体)

1. **功能正确性**:
   - `cargo test` 全部通过
   - `python3 scripts/integration_test.py` 全部通过
   - `cd orm-tests && npm test` 600+ 测试全部通过

2. **代码质量**:
   - `expr.rs` 不再存在 (已拆分为模块)
   - 单个文件不超过 500 行
   - 无 `#[allow(clippy::too_many_lines)]`

3. **性能**:
   - 基准测试无回归 (允许 ±5% 波动)
   - 函数调用场景有 >10% 提升

4. **可维护性**:
   - 添加新 SQL 函数只需:
     1. 在对应 `functions/*.rs` 添加实现
     2. 在 `registry.rs` 注册
     3. 添加单元测试
   - 无需修改其他文件

---

## 时间线

| Week | Phase | 主要交付 |
|------|-------|----------|
| 1 | 0 | 基准测试 + 测试补充 |
| 2-3 | 1 | EvalContext trait，统一 eval_expr |
| 4-6 | 2 | 函数拆分，phf 注册表 |
| 7-8 | 3 | NumericValue，类型系统优化 |
| 9-10 | 4 | 正则缓存，性能优化 |
| 11-12 | 5 | 文件拆分，清理 |

---

## 附录: 函数分类参考

### String Functions (~25)
UPPER, LOWER, LENGTH, CHAR_LENGTH, CHARACTER_LENGTH, OCTET_LENGTH, BIT_LENGTH,
CONCAT, CONCAT_WS, LEFT, RIGHT, SUBSTR, SUBSTRING, LPAD, RPAD, REPLACE, REVERSE,
TRIM, BTRIM, LTRIM, RTRIM, REPEAT, SPLIT_PART, STRPOS, ASCII, CHR, INITCAP, TRANSLATE

### Math Functions (~20)
ABS, CEIL, CEILING, FLOOR, ROUND, TRUNC, TRUNCATE, SQRT, CBRT, POWER, POW,
EXP, LN, LOG, LOG10, SIGN, MOD, DEGREES, RADIANS, SIN, COS, TAN, PI, RANDOM

### DateTime Functions (~15)
NOW, CURRENT_TIMESTAMP, CURRENT_DATE, DATE_TRUNC, DATE, TO_CHAR, AGE,
CLOCK_TIMESTAMP, STATEMENT_TIMESTAMP, TRANSACTION_TIMESTAMP, EXTRACT (handled in eval)

### JSON Functions (~20)
JSONB_ARRAY_LENGTH, JSON_ARRAY_LENGTH, JSONB_TYPEOF, JSON_TYPEOF,
JSONB_BUILD_OBJECT, JSON_BUILD_OBJECT, JSONB_BUILD_ARRAY, JSON_BUILD_ARRAY,
JSONB_EXISTS, JSONB_EXISTS_ANY, JSONB_EXISTS_ALL, JSONB_OBJECT_KEYS, JSON_OBJECT_KEYS,
JSONB_EXTRACT_PATH, JSON_EXTRACT_PATH, JSONB_EXTRACT_PATH_TEXT, JSON_EXTRACT_PATH_TEXT,
JSONB_PRETTY, TO_JSON, TO_JSONB

### Array Functions (~15)
ARRAY_LENGTH, ARRAY_UPPER, ARRAY_LOWER, CARDINALITY, ARRAY_POSITION,
ARRAY_CAT, ARRAY_APPEND, ARRAY_PREPEND, ARRAY_REMOVE, ARRAY_TO_STRING,
STRING_TO_ARRAY, UNNEST

### Regex Functions (~5)
REGEXP_REPLACE, REGEXP_MATCHES, REGEXP_SPLIT_TO_ARRAY

### UUID Functions (~3)
GEN_RANDOM_UUID, UUID_GENERATE_V4, UUIDV7

### PG Compatibility Functions (~20)
PG_IS_IN_RECOVERY, PG_BACKEND_PID, PG_ENCODING_TO_CHAR, VERSION,
CURRENT_DATABASE, CURRENT_SCHEMA, CURRENT_USER, SESSION_USER, USER,
PG_GET_USERBYID, HAS_SCHEMA_PRIVILEGE, HAS_TABLE_PRIVILEGE, HAS_DATABASE_PRIVILEGE,
PG_GET_INDEXDEF, PG_GET_CONSTRAINTDEF, PG_GET_EXPR, FORMAT_TYPE,
OBJ_DESCRIPTION, COL_DESCRIPTION, SHOBJ_DESCRIPTION, PG_GET_SERIAL_SEQUENCE,
PG_TYPEOF, QUOTE_IDENT, QUOTE_LITERAL, QUOTE_NULLABLE, TXID_CURRENT

### Misc Functions (~10)
COALESCE, NULLIF, GREATEST, LEAST, FORMAT, MD5, ENCODE, DECODE, SET_CONFIG
