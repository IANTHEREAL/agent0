# Hash Join 设计文档

**Status**: Historical  
**作者**: AI Assistant  
**日期**: 2026-01-22  
**关联**: [实现计划](./hash_join_implementation_plan.md)

> **Historical / non-SoT note**
>
> This document is a historical design record, not a current-behavior contract.
> Validate current behavior against `docs/sot/**`, `docs/ARCHITECTURE.md`, and the implementation under `src/**` before using it for product or compatibility decisions.

---

## 概述

为 db9-server 添加 Hash Join 支持，将等值连接的时间复杂度从 O(N×M) 降低到 O(N+M)。

## 动机

当前所有 JOIN 操作使用 Nested Loop Join：
- 1K × 1K rows = 1M 次比较 (~1秒)
- 10K × 10K rows = 100M 次比较 (~100秒)  
- 大表 JOIN 基本不可用

## 设计要点

### 算法

```
BUILD: 小表 → Hash Table
PROBE: 大表流式读取 → Hash 查找 → 输出匹配
```

### 组件

| 组件 | 职责 |
|------|------|
| `hash_join_key()` | Value[] → u64 hash |
| `JoinHashTable` | Build 侧存储 |
| `HashJoinOperator` | Volcano 迭代器 |
| Planner 扩展 | 选择 Hash vs Nested Loop |

### JOIN 类型支持

| 类型 | Build 追踪 | Probe 追踪 |
|------|-----------|-----------|
| INNER | ❌ | ❌ |
| LEFT | ❌ | ✅ |
| RIGHT | ✅ | ❌ |
| FULL | ✅ | ✅ |

## 关键设计决策

### NULL 处理

- SQL 语义: `NULL != NULL`
- Hash bucket: NULL key 行进入特殊存储，不参与匹配
- OUTER JOIN: NULL key 行在适当时候输出

### 列顺序

输出始终是 `left + right` 顺序，不管哪边是 build 侧。使用 `left_is_build` 标志追踪。

### 类型强制转换

支持 `Int32 = Int64` 等跨类型比较，在比较函数中处理。

## 性能目标

| 场景 | Nested Loop | Hash Join |
|------|-------------|-----------|
| 1K × 1K | 1s | <50ms |
| 10K × 10K | 100s | <500ms |
| 100K × 100K | timeout | <5s |

## Review 发现与修复

### Critical (已修复)

1. **RIGHT/FULL JOIN 追踪**: 添加 `global_indices` 和 `build_matched` bitmap
2. **`tap_mut` 依赖**: 改用标准库模式
3. **Numeric hash 不稳定**: 添加 `normalize()` 调用

### Important (已修复)

1. **列顺序**: 添加 `left_is_build` 标志
2. **Float NaN**: 特殊处理，NaN 不等于任何值
3. **类型强制**: 添加 Int32/Int64 比较支持

## 实现计划

- **Week 1**: 核心数据结构 (hash 函数 + hash table)
- **Week 2**: HashJoinOperator (所有 JOIN 类型)
- **Week 3**: Planner 集成 + EXPLAIN
- **Week 4**: 集成测试 + 性能优化

详见 [实现计划文档](./hash_join_implementation_plan.md)

## 测试策略

### 功能测试
- 所有 JOIN 类型 × 各种数据场景
- NULL key 处理
- 空表边界情况
- 多列 key

### 性能测试
- 与 PostgreSQL 对比
- 不同数据规模基准测试

## 非目标 (v1)

- Grace Hash Join (磁盘溢出)
- Parallel Hash Join (多线程)
- Semi/Anti Join 优化

## 参考

- PostgreSQL: `src/backend/executor/nodeHashjoin.c`
- DuckDB: `src/execution/operator/join/physical_hash_join.cpp`
- DataFusion: `datafusion/physical-plan/src/joins/hash_join.rs`
