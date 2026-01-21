# Dify 兼容性实施计划

基于 [dify-database-compatibility-spec.md](../dify-database-compatibility-spec.md) 的分析，制定分阶段实施计划。

## 现状分析

### ✅ 已支持

| 功能 | 状态 | 备注 |
|------|------|------|
| UUID 类型 | ✅ | `uuid_generate_v4()`, `gen_random_uuid()` |
| JSONB 基础 | ✅ | `->`, `->>` 操作符 |
| JSONB 函数 | ✅ | `jsonb_array_elements_text()` |
| 窗口函数 | ✅ | `ROW_NUMBER() OVER (PARTITION BY ...)` |
| 聚合函数 | ✅ | `COUNT`, `SUM`, `AVG`, `STRING_AGG` |
| 序列 | ✅ | `CREATE SEQUENCE`, `nextval()` |
| DATE_TRUNC | ✅ | 支持常用字段 |
| JOIN | ✅ | LEFT/INNER JOIN |
| 子查询 | ✅ | FROM 子句子查询 |
| CASE 表达式 | ✅ | 完整支持 |
| COALESCE | ✅ | 完整支持 |
| split_part() | ✅ | 字符串分割 |
| clock_timestamp() | ✅ | 事务内变化时间戳 |
| overlay() | ✅ | 字符串/bytea 替换 |

### ❌ 缺失（需实现）

| 功能 | 优先级 | 影响 |
|------|--------|------|
| GIN 索引查询加速 | P0 | JSONB 查询性能 |
| AT TIME ZONE | P0 | 时区转换（Dify 大量使用） |
| JSONB @> 操作符 | P0 | JSONB 包含查询 |
| bytea 函数 | P1 | uuidv7() 依赖 |
| CREATE EXTENSION | P1 | uuid-ossp 扩展 |
| CURRENT_TIMESTAMP(0) | P1 | 精度截断 |
| encode(bytea, 'hex') | P1 | uuidv7() 依赖 |

## 实施阶段

```
Phase 1 (P0): 核心兼容      ← 2-3 天，可运行 Dify 基础功能
Phase 2 (P1): 完整兼容      ← 2 天，支持 uuidv7 等高级特性
Phase 3 (P2): 性能优化      ← 3-5 天，GIN 索引加速
```

## PRD 列表

| PRD | 标题 | 阶段 | 预估 |
|-----|------|------|------|
| [01_timezone_conversion.md](01_timezone_conversion.md) | AT TIME ZONE | P0 | 4h |
| [02_jsonb_containment.md](02_jsonb_containment.md) | JSONB @> 操作符 | P0 | 3h |
| [03_timestamp_precision.md](03_timestamp_precision.md) | CURRENT_TIMESTAMP(n) | P1 | 2h |
| [04_bytea_functions.md](04_bytea_functions.md) | set_bit/int8send/uuid_send | P1 | 4h |
| [05_encode_decode.md](05_encode_decode.md) | encode()/decode() | P1 | 2h |
| [06_gin_index_query.md](06_gin_index_query.md) | GIN 索引查询优化 | P2 | 3-5d |

## 验证方案

1. **单元测试**: 每个 PRD 对应测试文件
2. **集成测试**: `tests/87_dify_compat.sql`
3. **实际验证**: 导入 Dify schema，运行示例查询
