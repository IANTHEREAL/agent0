# PRD-D03: CURRENT_TIMESTAMP 精度控制

**阶段**: Phase 1 (P1)  
**预估**: 2 小时  
**依赖**: 无

## 背景

Dify DDL 中大量使用精度截断的时间戳默认值：

```sql
created_at timestamp without time zone DEFAULT CURRENT_TIMESTAMP(0) NOT NULL
```

`CURRENT_TIMESTAMP(0)` 表示截断到秒（0 位小数），当前 db9-server 可能未正确处理精度参数。

## 目标

支持 `CURRENT_TIMESTAMP(p)` 和 `NOW(p)` 的精度参数，其中 p = 0..6。

## 语法

```sql
CURRENT_TIMESTAMP       -- 默认 6 位微秒精度
CURRENT_TIMESTAMP(0)    -- 秒精度（无小数）
CURRENT_TIMESTAMP(3)    -- 毫秒精度
NOW()                   -- 同 CURRENT_TIMESTAMP
NOW(0)                  -- 同 CURRENT_TIMESTAMP(0)
```

## 需求

### 精度定义

| p | 精度 | 示例 |
|---|------|------|
| 0 | 秒 | 2024-01-15 10:30:00 |
| 1 | 0.1秒 | 2024-01-15 10:30:00.1 |
| 2 | 0.01秒 | 2024-01-15 10:30:00.12 |
| 3 | 毫秒 | 2024-01-15 10:30:00.123 |
| 4 | 0.1毫秒 | 2024-01-15 10:30:00.1234 |
| 5 | 0.01毫秒 | 2024-01-15 10:30:00.12345 |
| 6 | 微秒（默认） | 2024-01-15 10:30:00.123456 |

## 实现

### 1) 函数求值

```rust
// src/sql/expr.rs - eval_function()

"CURRENT_TIMESTAMP" | "NOW" => {
    let precision = if args.is_empty() {
        6 // 默认微秒精度
    } else {
        match &args[0] {
            Value::Int32(p) => (*p).clamp(0, 6) as u32,
            Value::Int64(p) => (*p).clamp(0, 6) as u32,
            _ => 6,
        }
    };
    
    let now = chrono::Utc::now().naive_utc();
    let truncated = truncate_timestamp_precision(now, precision);
    Ok(Value::Timestamp(truncated))
}
```

### 2) 精度截断

```rust
// src/sql/helpers.rs

pub fn truncate_timestamp_precision(
    ts: chrono::NaiveDateTime,
    precision: u32,
) -> chrono::NaiveDateTime {
    let nanos = ts.timestamp_subsec_nanos();
    
    // 计算保留的纳秒位数
    let divisor = match precision {
        0 => 1_000_000_000, // 截断所有小数
        1 => 100_000_000,
        2 => 10_000_000,
        3 => 1_000_000,     // 保留毫秒
        4 => 100_000,
        5 => 10_000,
        6 => 1_000,         // 保留微秒
        _ => 1,
    };
    
    let truncated_nanos = (nanos / divisor) * divisor;
    
    ts.with_nanosecond(truncated_nanos).unwrap_or(ts)
}
```

### 3) DEFAULT 表达式支持

确保 `DEFAULT CURRENT_TIMESTAMP(0)` 在 DDL 解析时正确处理：

```rust
// src/sql/ddl.rs - parse_default_expr()

// 已有的 eval_default_expr 应该能处理函数调用
// 需确认 sqlparser 正确解析 CURRENT_TIMESTAMP(0) 为带参数的函数
```

## 测试

### SQL 测试

```sql
-- 精度测试
SELECT CURRENT_TIMESTAMP(0);
-- Expected: 秒精度，如 2024-01-15 10:30:00

SELECT CURRENT_TIMESTAMP(3);
-- Expected: 毫秒精度，如 2024-01-15 10:30:00.123

SELECT NOW(0);
-- Expected: 同 CURRENT_TIMESTAMP(0)

-- DDL 测试
CREATE TABLE test_ts (
    id SERIAL PRIMARY KEY,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP(0)
);

INSERT INTO test_ts DEFAULT VALUES;
SELECT created_at FROM test_ts;
-- Expected: 秒精度时间戳

DROP TABLE test_ts;
```

### 单元测试

```rust
#[test]
fn test_truncate_timestamp_precision() {
    let ts = NaiveDateTime::parse_from_str(
        "2024-01-15 10:30:45.123456789",
        "%Y-%m-%d %H:%M:%S%.f"
    ).unwrap();
    
    let p0 = truncate_timestamp_precision(ts, 0);
    assert_eq!(p0.timestamp_subsec_nanos(), 0);
    
    let p3 = truncate_timestamp_precision(ts, 3);
    assert_eq!(p3.timestamp_subsec_millis(), 123);
    assert_eq!(p3.timestamp_subsec_nanos() % 1_000_000, 0);
}
```

## 验收标准

```sql
-- 必须通过
SELECT CURRENT_TIMESTAMP(0) = DATE_TRUNC('second', CURRENT_TIMESTAMP);
-- Expected: true

CREATE TABLE t (ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP(0));
INSERT INTO t DEFAULT VALUES;
SELECT ts FROM t WHERE ts::text NOT LIKE '%.%';
-- Expected: 1 row (无小数部分)
```

## 输出格式

当精度为 0 时，输出不应包含小数点：
- ✅ `2024-01-15 10:30:00`
- ❌ `2024-01-15 10:30:00.000000`
