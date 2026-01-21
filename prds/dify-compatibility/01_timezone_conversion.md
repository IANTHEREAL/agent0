# PRD-D01: AT TIME ZONE 时区转换

**阶段**: Phase 1 (P0)  
**预估**: 4 小时  
**依赖**: 无

## 背景

Dify 大量使用时区转换进行日期分组统计：

```sql
DATE(DATE_TRUNC('day', created_at AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'))
```

当前 pg-tikv 不支持 `AT TIME ZONE` 表达式，导致此类查询失败。

## 目标

支持 `timestamp AT TIME ZONE zone` 语法：
- 将 timestamp 解释为指定时区并转换为 UTC
- 将 timestamptz 转换为指定时区的本地时间

## 语法

```sql
-- timestamp → timestamptz（解释为指定时区）
timestamp_value AT TIME ZONE 'zone_name'

-- timestamptz → timestamp（转换到指定时区）
timestamptz_value AT TIME ZONE 'zone_name'
```

## 需求

### 功能需求

1. 支持时区名称：`'UTC'`, `'America/New_York'`, `'Asia/Shanghai'` 等
2. 支持时区偏移：`'+08:00'`, `'-05:00'`
3. 链式转换：`ts AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'`

### 支持的时区（MVP）

优先支持 IANA 常用时区：

| 时区 | 偏移 |
|------|------|
| UTC | +00:00 |
| America/New_York | -05:00 / -04:00 (DST) |
| America/Los_Angeles | -08:00 / -07:00 (DST) |
| Europe/London | +00:00 / +01:00 (DST) |
| Europe/Paris | +01:00 / +02:00 (DST) |
| Asia/Shanghai | +08:00 |
| Asia/Tokyo | +09:00 |

### 非目标（MVP）

- 完整 DST（夏令时）规则计算
- 所有 IANA 时区数据库
- 历史时区变更

## 实现

### 1) 表达式求值

在 `expr.rs` 中处理 `Expr::AtTimeZone`：

```rust
// src/sql/expr.rs

Expr::AtTimeZone { timestamp, time_zone } => {
    let ts = eval_expr(timestamp, row, schema)?;
    let tz = eval_expr(time_zone, row, schema)?;
    
    let zone_name = match tz {
        Value::Text(s) => s,
        _ => return Err(anyhow!("time zone must be text")),
    };
    
    let offset = parse_timezone_offset(&zone_name)?;
    
    match ts {
        Value::Timestamp(naive) => {
            // timestamp → timestamptz: 解释为该时区
            let utc = naive - chrono::Duration::seconds(offset);
            Ok(Value::TimestampTz(utc))
        }
        Value::TimestampTz(utc) => {
            // timestamptz → timestamp: 转换到该时区
            let local = utc + chrono::Duration::seconds(offset);
            Ok(Value::Timestamp(local))
        }
        _ => Err(anyhow!("AT TIME ZONE requires timestamp")),
    }
}
```

### 2) 时区解析

```rust
// src/sql/timezone.rs

use std::collections::HashMap;
use once_cell::sync::Lazy;

static TIMEZONE_OFFSETS: Lazy<HashMap<&'static str, i64>> = Lazy::new(|| {
    let mut m = HashMap::new();
    // 秒为单位
    m.insert("UTC", 0);
    m.insert("GMT", 0);
    m.insert("America/New_York", -5 * 3600);  // 简化，不处理 DST
    m.insert("America/Los_Angeles", -8 * 3600);
    m.insert("Europe/London", 0);
    m.insert("Europe/Paris", 1 * 3600);
    m.insert("Asia/Shanghai", 8 * 3600);
    m.insert("Asia/Tokyo", 9 * 3600);
    m.insert("PRC", 8 * 3600);
    m
});

pub fn parse_timezone_offset(zone: &str) -> Result<i64> {
    // 1. 检查偏移格式 (+08:00, -05:00)
    if let Some(offset) = parse_offset_string(zone) {
        return Ok(offset);
    }
    
    // 2. 检查已知时区名
    let zone_upper = zone.to_uppercase();
    let zone_normalized = zone.replace(" ", "_");
    
    TIMEZONE_OFFSETS
        .get(zone_normalized.as_str())
        .or_else(|| TIMEZONE_OFFSETS.get(zone_upper.as_str()))
        .copied()
        .ok_or_else(|| anyhow!("unknown time zone: {}", zone))
}

fn parse_offset_string(s: &str) -> Option<i64> {
    // +08:00 或 -05:00
    let s = s.trim();
    if s.len() < 5 { return None; }
    
    let sign = match s.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    
    let rest = &s[1..];
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 2 { return None; }
    
    let hours: i64 = parts[0].parse().ok()?;
    let mins: i64 = parts[1].parse().ok()?;
    
    Some(sign * (hours * 3600 + mins * 60))
}
```

### 3) sqlparser 支持

sqlparser-rs 已支持 `AtTimeZone` 表达式，需确认版本兼容。

## 测试

### SQL 测试

```sql
-- 基础转换
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC';
-- Expected: 2024-01-15 10:00:00+00

SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'Asia/Shanghai';
-- Expected: 2024-01-15 02:00:00+00 (解释为 +8，转为 UTC)

-- 链式转换
SELECT TIMESTAMP '2024-01-15 10:00:00' 
    AT TIME ZONE 'UTC' 
    AT TIME ZONE 'America/New_York';
-- Expected: 2024-01-15 05:00:00

-- 偏移格式
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE '+08:00';

-- Dify 实际模式
SELECT DATE(DATE_TRUNC('day', 
    TIMESTAMP '2024-01-15 10:30:00' AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'
));
-- Expected: 2024-01-15
```

### 单元测试

```rust
#[test]
fn test_parse_timezone_offset() {
    assert_eq!(parse_timezone_offset("UTC").unwrap(), 0);
    assert_eq!(parse_timezone_offset("+08:00").unwrap(), 8 * 3600);
    assert_eq!(parse_timezone_offset("-05:00").unwrap(), -5 * 3600);
    assert_eq!(parse_timezone_offset("Asia/Shanghai").unwrap(), 8 * 3600);
}

#[test]
fn test_at_time_zone() {
    // timestamp AT TIME ZONE 'zone' → timestamptz
    // timestamptz AT TIME ZONE 'zone' → timestamp
}
```

## 验收标准

```sql
-- 必须通过
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC';
SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'Asia/Shanghai';
SELECT now() AT TIME ZONE 'America/New_York';
```

## 后续增强

- 集成 chrono-tz 库支持完整 IANA 时区
- DST 自动切换
- SET timezone TO 会话变量
