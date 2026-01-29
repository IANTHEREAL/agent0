# Hash Join 实现计划

**状态**: 已审核，待实现  
**创建日期**: 2026-01-22  
**预计工期**: 4 周  
**优先级**: P1

---

## 1. 背景与目标

### 1.1 问题陈述

pg-tikv 当前所有 JOIN 操作都使用 Nested Loop Join，时间复杂度 O(N×M)。对于大表 JOIN：
- 1K × 1K = 1M 次比较 → ~1秒
- 10K × 10K = 100M 次比较 → ~100秒
- 100K × 100K = 10B 次比较 → 超时

### 1.2 目标

| 目标 | 衡量标准 |
|------|----------|
| **性能** | 等值连接场景下比 Nested Loop 快 10-100x |
| **正确性** | 所有 JOIN 类型结果与 PostgreSQL 完全一致 |
| **稳定性** | 内存可控，不会 OOM |
| **兼容性** | 无缝集成现有 Volcano operator 框架 |
| **可观测** | EXPLAIN 显示 Hash Join 计划 |

### 1.3 非目标（第一版不实现）

- Grace Hash Join（磁盘溢出）
- Parallel Hash Join（多线程）
- Semi Join / Anti Join 优化
- 自动统计信息收集

---

## 2. 技术设计摘要

### 2.1 算法概述

```
Hash Join 两阶段算法:

┌─────────────────────────────────────────────────────────────┐
│ Phase 1: BUILD                                               │
│   - 读取较小表（build side）的所有行                          │
│   - 对每行的 join key 计算 hash                              │
│   - 插入 hash table: hash(key) → [rows with same key]       │
├─────────────────────────────────────────────────────────────┤
│ Phase 2: PROBE                                               │
│   - 流式读取较大表（probe side）                              │
│   - 对每行的 join key 计算 hash                              │
│   - 在 hash table 中查找匹配行                               │
│   - 输出匹配的行对                                           │
└─────────────────────────────────────────────────────────────┘

时间复杂度: O(N + M) vs Nested Loop 的 O(N × M)
空间复杂度: O(min(N, M)) - build side 需要全部放入内存
```

### 2.2 核心组件

```
src/sql/operators/hash_join.rs (新文件)
├── hash_join_key()           - Value[] → u64 hash
├── join_keys_equal()         - 比较两个 key 是否相等
├── JoinHashTable             - Build 侧 hash table
│   ├── insert()              - 插入行
│   ├── probe()               - 查找匹配行 (返回 index + row)
│   └── all_rows_with_indices() - 遍历所有行
├── HashJoinOperator          - Volcano 迭代器
│   ├── open()                - Build phase
│   ├── next()                - Probe phase + emit
│   └── close()               - 清理
└── HashJoinConfig            - 配置（内存限制等）

src/sql/planner.rs (修改)
├── choose_join_algorithm()   - 选择 Hash vs Nested Loop
└── extract_equi_join_keys()  - 从 ON 条件提取等值 key

src/sql/executor_operators.rs (修改)
└── 集成 Hash Join 到执行路径
```

### 2.3 JOIN 类型支持

| JOIN Type | Build 侧追踪 | Probe 侧追踪 | 实现复杂度 |
|-----------|-------------|-------------|-----------|
| INNER     | ❌          | ❌          | 简单 |
| LEFT      | ❌          | ✅ 未匹配输出 NULL | 中等 |
| RIGHT     | ✅ bitmap   | ❌          | 中等 |
| FULL      | ✅ bitmap   | ✅          | 复杂 |

---

## 3. 详细实现计划

### Phase 1: 核心数据结构（第 1 周）

#### 3.1.1 文件创建

```bash
# 创建新文件
touch src/sql/operators/hash_join.rs
```

#### 3.1.2 Hash 函数实现

**文件**: `src/sql/operators/hash_join.rs`

```rust
//! Hash Join operator for equi-joins.
//!
//! Implements a classic hash join with:
//! - Build phase: smaller relation → hash table
//! - Probe phase: larger relation → stream + lookup

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::Expr;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::eval_expr;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

/// Compute a 64-bit hash for a slice of Values (join key).
///
/// Design decisions:
/// - NULL hashes to a fixed constant (for bucketing, not equality)
/// - Float64 NaN hashes to a fixed constant
/// - Numeric is normalized before hashing (1.0 == 1.00)
/// - Uses SipHash for DoS resistance
pub fn hash_join_key(values: &[Value]) -> u64 {
    let mut hasher = DefaultHasher::new();
    
    // Length-prefix for safety
    values.len().hash(&mut hasher);
    
    for value in values {
        hash_single_value(&mut hasher, value);
    }
    
    hasher.finish()
}

fn hash_single_value<H: Hasher>(hasher: &mut H, value: &Value) {
    // Discriminant first
    std::mem::discriminant(value).hash(hasher);
    
    match value {
        Value::Null => {
            0xDEAD_BEEF_CAFE_BABEu64.hash(hasher);
        }
        Value::Boolean(b) => b.hash(hasher),
        Value::Int32(i) => i.hash(hasher),
        Value::Int64(i) => i.hash(hasher),
        Value::Float64(f) => {
            if f.is_nan() {
                u64::MAX.hash(hasher);
            } else if *f == 0.0 {
                0.0f64.to_bits().hash(hasher);
            } else {
                f.to_bits().hash(hasher);
            }
        }
        Value::Text(s) => s.hash(hasher),
        Value::Bytes(b) => b.hash(hasher),
        Value::Timestamp(ts) => ts.hash(hasher),
        Value::Interval(iv) => {
            iv.months.hash(hasher);
            iv.millis.hash(hasher);
        }
        Value::Uuid(bytes) => bytes.hash(hasher),
        Value::Array(arr) => {
            arr.len().hash(hasher);
            for elem in arr {
                hash_single_value(hasher, elem);
            }
        }
        Value::Vector(vec) => {
            vec.len().hash(hasher);
            for f in vec {
                f.to_bits().hash(hasher);
            }
        }
        Value::Json(s) | Value::Jsonb(s) => s.hash(hasher),
        Value::Time(t) => t.hash(hasher),
        Value::Date(d) => d.hash(hasher),
        Value::Numeric(d) => {
            let normalized = d.normalize();
            let unpacked = normalized.unpack();
            unpacked.lo.hash(hasher);
            unpacked.mid.hash(hasher);
            unpacked.hi.hash(hasher);
            unpacked.negative.hash(hasher);
            unpacked.scale.hash(hasher);
        }
    }
}

/// Compare two join keys for equality (SQL semantics).
///
/// - NULL != NULL (returns false)
/// - NaN == NaN (returns true, PostgreSQL-like)
/// - Handles Int32/Int64 type coercion
pub fn join_keys_equal(left: &[Value], right: &[Value]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .all(|(l, r)| values_equal_for_join(l, r))
}

fn values_equal_for_join(left: &Value, right: &Value) -> bool {
    match (left, right) {
        // NULL never equals anything
        (Value::Null, _) | (_, Value::Null) => false,
        
        // Float: PostgreSQL-like equality (NaN == NaN)
        (Value::Float64(a), Value::Float64(b)) => a == b || (a.is_nan() && b.is_nan()),
        
        // Integer type coercion
        (Value::Int32(a), Value::Int64(b)) => (*a as i64) == *b,
        (Value::Int64(a), Value::Int32(b)) => *a == (*b as i64),
        
        // Numeric: normalize before compare
        (Value::Numeric(a), Value::Numeric(b)) => a.normalize() == b.normalize(),
        
        // Default
        _ => left == right,
    }
}
```

**单元测试**:

```rust
#[cfg(test)]
mod hash_tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    #[test]
    fn test_hash_consistency() {
        let key1 = vec![Value::Int32(42), Value::Text("hello".into())];
        let key2 = vec![Value::Int32(42), Value::Text("hello".into())];
        assert_eq!(hash_join_key(&key1), hash_join_key(&key2));
    }

    #[test]
    fn test_hash_different_keys() {
        let key1 = vec![Value::Int32(1)];
        let key2 = vec![Value::Int32(2)];
        assert_ne!(hash_join_key(&key1), hash_join_key(&key2));
    }

    #[test]
    fn test_null_hash_consistent() {
        let key1 = vec![Value::Null];
        let key2 = vec![Value::Null];
        assert_eq!(hash_join_key(&key1), hash_join_key(&key2));
    }

    #[test]
    fn test_null_not_equal() {
        assert!(!join_keys_equal(&[Value::Null], &[Value::Null]));
    }

    #[test]
    fn test_nan_equal() {
        let nan = Value::Float64(f64::NAN);
        assert!(join_keys_equal(&[nan.clone()], &[nan]));
    }

    #[test]
    fn test_int_type_coercion() {
        assert!(join_keys_equal(
            &[Value::Int32(42)],
            &[Value::Int64(42)]
        ));
    }

    #[test]
    fn test_numeric_normalization() {
        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();
        assert_eq!(
            hash_join_key(&[Value::Numeric(d1)]),
            hash_join_key(&[Value::Numeric(d2)])
        );
        assert!(join_keys_equal(
            &[Value::Numeric(d1)],
            &[Value::Numeric(d2)]
        ));
    }
}
```

#### 3.1.3 JoinHashTable 实现

```rust
/// Hash bucket storing rows with the same hash value.
#[derive(Debug, Default)]
struct HashBucket {
    /// Rows in this bucket
    rows: Vec<Row>,
    /// Extracted join keys (parallel to rows)
    keys: Vec<Vec<Value>>,
    /// Global indices for OUTER join tracking (parallel to rows)
    global_indices: Vec<usize>,
}

/// Configuration for Hash Join.
#[derive(Debug, Clone)]
pub struct HashJoinConfig {
    /// Maximum memory for hash table (bytes). Default: 256 MB
    pub max_memory_bytes: usize,
    /// Minimum rows to prefer Hash Join over Nested Loop. Default: 100
    pub min_rows_threshold: usize,
}

impl Default for HashJoinConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 256 * 1024 * 1024,
            min_rows_threshold: 100,
        }
    }
}

/// Build-side hash table for Hash Join.
#[derive(Debug)]
pub struct JoinHashTable {
    /// Map: hash(key) → bucket
    buckets: HashMap<u64, HashBucket>,
    /// Column indices forming the join key
    key_indices: Vec<usize>,
    /// Number of rows (excluding null-key rows)
    row_count: usize,
    /// Estimated memory usage (bytes)
    memory_bytes: usize,
    /// Rows with NULL in join key (never match, but needed for OUTER)
    null_key_rows: Vec<Row>,
    /// Start index for null_key_rows in global numbering
    null_key_start_index: usize,
}

impl JoinHashTable {
    pub fn new(key_indices: Vec<usize>) -> Self {
        Self {
            buckets: HashMap::new(),
            key_indices,
            row_count: 0,
            memory_bytes: 0,
            null_key_rows: Vec::new(),
            null_key_start_index: 0,
        }
    }

    pub fn with_capacity(key_indices: Vec<usize>, estimated_rows: usize) -> Self {
        Self {
            buckets: HashMap::with_capacity((estimated_rows as f64 * 1.4) as usize),
            key_indices,
            row_count: 0,
            memory_bytes: 0,
            null_key_rows: Vec::new(),
            null_key_start_index: 0,
        }
    }

    /// Insert a row. Returns estimated memory delta.
    pub fn insert(&mut self, row: Row) -> usize {
        let key = self.extract_key(&row);
        let mem_delta = Self::estimate_row_size(&row);

        // NULL key → separate storage
        if key.iter().any(|v| matches!(v, Value::Null)) {
            self.null_key_rows.push(row);
            self.memory_bytes += mem_delta;
            return mem_delta;
        }

        let hash = hash_join_key(&key);
        let global_idx = self.row_count;

        let bucket = self.buckets.entry(hash).or_default();
        bucket.rows.push(row);
        bucket.keys.push(key);
        bucket.global_indices.push(global_idx);

        self.row_count += 1;
        self.memory_bytes += mem_delta;
        mem_delta
    }

    /// Call after all inserts to finalize indices.
    pub fn finalize(&mut self) {
        self.null_key_start_index = self.row_count;
    }

    /// Probe for matching rows. Returns (global_index, row) pairs.
    pub fn probe<'a>(&'a self, probe_key: &[Value]) -> ProbeIter<'a> {
        if probe_key.iter().any(|v| matches!(v, Value::Null)) {
            return ProbeIter::Empty;
        }

        let hash = hash_join_key(probe_key);
        match self.buckets.get(&hash) {
            Some(bucket) => ProbeIter::Scanning {
                bucket,
                probe_key: probe_key.to_vec(),
                index: 0,
            },
            None => ProbeIter::Empty,
        }
    }

    /// Total row count (for bitmap sizing).
    pub fn total_row_count(&self) -> usize {
        self.row_count + self.null_key_rows.len()
    }

    /// Iterate all rows with indices (for OUTER join unmatched emission).
    pub fn all_rows_with_indices(&self) -> impl Iterator<Item = (usize, &Row)> {
        self.buckets
            .values()
            .flat_map(|b| b.global_indices.iter().copied().zip(b.rows.iter()))
            .chain(
                self.null_key_rows
                    .iter()
                    .enumerate()
                    .map(move |(i, r)| (self.null_key_start_index + i, r)),
            )
    }

    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes
    }

    fn extract_key(&self, row: &Row) -> Vec<Value> {
        self.key_indices
            .iter()
            .map(|&i| row.values[i].clone())
            .collect()
    }

    fn estimate_row_size(row: &Row) -> usize {
        std::mem::size_of::<Row>()
            + row
                .values
                .iter()
                .map(|v| match v {
                    Value::Text(s) => s.len() + 24,
                    Value::Bytes(b) => b.len() + 24,
                    Value::Array(a) => a.len() * 16 + 24,
                    Value::Vector(v) => v.len() * 8 + 24,
                    Value::Json(s) | Value::Jsonb(s) => s.len() + 24,
                    _ => 16,
                })
                .sum::<usize>()
    }
}

/// Iterator for probe results.
pub enum ProbeIter<'a> {
    Empty,
    Scanning {
        bucket: &'a HashBucket,
        probe_key: Vec<Value>,
        index: usize,
    },
}

impl<'a> Iterator for ProbeIter<'a> {
    type Item = (usize, &'a Row);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ProbeIter::Empty => None,
            ProbeIter::Scanning {
                bucket,
                probe_key,
                index,
            } => {
                while *index < bucket.rows.len() {
                    let i = *index;
                    *index += 1;
                    if join_keys_equal(&bucket.keys[i], probe_key) {
                        return Some((bucket.global_indices[i], &bucket.rows[i]));
                    }
                }
                None
            }
        }
    }
}
```

**单元测试**:

```rust
#[cfg(test)]
mod hash_table_tests {
    use super::*;

    fn make_row(id: i32, name: &str) -> Row {
        Row::new(vec![Value::Int32(id), Value::Text(name.to_string())])
    }

    #[test]
    fn test_insert_and_probe() {
        let mut table = JoinHashTable::new(vec![0]); // key on column 0
        table.insert(make_row(1, "a"));
        table.insert(make_row(1, "b")); // same key
        table.insert(make_row(2, "c"));
        table.finalize();

        let matches: Vec<_> = table.probe(&[Value::Int32(1)]).collect();
        assert_eq!(matches.len(), 2);

        let matches: Vec<_> = table.probe(&[Value::Int32(2)]).collect();
        assert_eq!(matches.len(), 1);

        let matches: Vec<_> = table.probe(&[Value::Int32(999)]).collect();
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn test_null_key_handling() {
        let mut table = JoinHashTable::new(vec![0]);
        table.insert(Row::new(vec![Value::Null, Value::Text("null_row".into())]));
        table.insert(make_row(1, "normal"));
        table.finalize();

        // NULL key should not match anything
        let matches: Vec<_> = table.probe(&[Value::Null]).collect();
        assert_eq!(matches.len(), 0);

        // But should be in all_rows for OUTER join
        assert_eq!(table.total_row_count(), 2);
    }

    #[test]
    fn test_global_indices() {
        let mut table = JoinHashTable::new(vec![0]);
        table.insert(make_row(1, "first"));
        table.insert(make_row(2, "second"));
        table.insert(make_row(1, "third")); // same key as first
        table.finalize();

        let matches: Vec<_> = table.probe(&[Value::Int32(1)]).collect();
        // Should get indices 0 and 2
        let indices: Vec<usize> = matches.iter().map(|(i, _)| *i).collect();
        assert!(indices.contains(&0));
        assert!(indices.contains(&2));
    }
}
```

#### 3.1.4 mod.rs 更新

**文件**: `src/sql/operators/mod.rs`

添加：
```rust
mod hash_join;
pub use hash_join::{
    hash_join_key, join_keys_equal, HashJoinConfig, HashJoinOperator, JoinHashTable,
};
```

#### 3.1.5 Phase 1 验收标准

- [ ] `cargo test hash_tests` 全部通过
- [ ] `cargo test hash_table_tests` 全部通过
- [ ] `cargo clippy` 无警告
- [ ] 代码覆盖率 > 80%

---

### Phase 2: HashJoinOperator - INNER JOIN（第 1-2 周）

#### 3.2.1 Operator 实现

```rust
/// Join type for Hash Join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashJoinType {
    Inner,
    Left,
    Right,
    Full,
}

/// State machine for Hash Join execution.
#[derive(Debug)]
enum HashJoinState {
    /// Not yet opened
    Created,
    /// Probing phase
    Probing {
        hash_table: JoinHashTable,
        current_probe_row: Option<Row>,
        current_matches: Vec<(usize, Row)>,
        match_index: usize,
        build_matched: Option<Vec<bool>>,
        probe_had_match: bool,
    },
    /// Emitting unmatched build rows (RIGHT/FULL only)
    EmittingUnmatched {
        unmatched_rows: Vec<Row>,
        emit_index: usize,
    },
    /// Done
    Exhausted,
}

/// Hash Join operator implementing Volcano iterator model.
pub struct HashJoinOperator {
    /// Build side (materialized into hash table)
    build_child: BoxedOperator,
    /// Probe side (streamed)
    probe_child: BoxedOperator,
    /// Join type
    join_type: HashJoinType,
    /// Key indices in build side
    build_key_indices: Vec<usize>,
    /// Key indices in probe side
    probe_key_indices: Vec<usize>,
    /// Whether left input is build side (for output column order)
    left_is_build: bool,
    /// Additional filter (residual non-equi conditions)
    filter: Option<Expr>,
    /// Output schema
    output_schema: TableSchema,
    /// State
    state: HashJoinState,
    /// Config
    config: HashJoinConfig,
}

impl HashJoinOperator {
    /// Create a new Hash Join operator.
    ///
    /// # Arguments
    /// * `left_child` - Left input
    /// * `right_child` - Right input  
    /// * `join_type` - INNER/LEFT/RIGHT/FULL
    /// * `left_key_indices` - Join key column indices in left
    /// * `right_key_indices` - Join key column indices in right
    /// * `left_is_build` - Use left as build side (usually smaller table)
    /// * `filter` - Optional residual filter
    /// * `config` - Configuration
    pub fn new(
        left_child: BoxedOperator,
        right_child: BoxedOperator,
        join_type: HashJoinType,
        left_key_indices: Vec<usize>,
        right_key_indices: Vec<usize>,
        left_is_build: bool,
        filter: Option<Expr>,
        config: HashJoinConfig,
    ) -> Self {
        // Build output schema: always left + right order
        let mut columns = Vec::new();
        for col in &left_child.schema().columns {
            columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }
        for col in &right_child.schema().columns {
            columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let output_schema = TableSchema {
            name: "hash_join".to_string(),
            table_id: 0,
            columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        // Assign build/probe based on flag
        let (build_child, probe_child, build_key_indices, probe_key_indices) = 
            if left_is_build {
                (left_child, right_child, left_key_indices, right_key_indices)
            } else {
                (right_child, left_child, right_key_indices, left_key_indices)
            };

        Self {
            build_child,
            probe_child,
            join_type,
            build_key_indices,
            probe_key_indices,
            left_is_build,
            filter,
            output_schema,
            state: HashJoinState::Created,
            config,
        }
    }

    fn extract_probe_key(&self, row: &Row) -> Vec<Value> {
        self.probe_key_indices
            .iter()
            .map(|&i| row.values[i].clone())
            .collect()
    }

    /// Create output row maintaining left + right order.
    fn make_output_row(&self, probe_row: &Row, build_row: &Row) -> Row {
        if self.left_is_build {
            // left=build, right=probe → build + probe
            let mut vals = build_row.values.clone();
            vals.extend(probe_row.values.clone());
            Row::new(vals)
        } else {
            // left=probe, right=build → probe + build
            let mut vals = probe_row.values.clone();
            vals.extend(build_row.values.clone());
            Row::new(vals)
        }
    }

    /// Create row with NULL on build side (unmatched probe).
    fn make_probe_only_row(&self, probe_row: &Row) -> Row {
        let null_count = self.build_child.schema().columns.len();
        if self.left_is_build {
            // left=build(NULL) + right=probe
            let mut vals: Vec<Value> = vec![Value::Null; null_count];
            vals.extend(probe_row.values.clone());
            Row::new(vals)
        } else {
            // left=probe + right=build(NULL)
            let mut vals = probe_row.values.clone();
            vals.extend(vec![Value::Null; null_count]);
            Row::new(vals)
        }
    }

    /// Create row with NULL on probe side (unmatched build).
    fn make_build_only_row(&self, build_row: &Row) -> Row {
        let null_count = self.probe_child.schema().columns.len();
        if self.left_is_build {
            // left=build + right=probe(NULL)
            let mut vals = build_row.values.clone();
            vals.extend(vec![Value::Null; null_count]);
            Row::new(vals)
        } else {
            // left=probe(NULL) + right=build
            let mut vals: Vec<Value> = vec![Value::Null; null_count];
            vals.extend(build_row.values.clone());
            Row::new(vals)
        }
    }

    fn check_filter(&self, row: &Row) -> Result<bool> {
        match &self.filter {
            None => Ok(true),
            Some(expr) => {
                match eval_expr(expr, Some(row), Some(&self.output_schema))? {
                    Value::Boolean(b) => Ok(b),
                    Value::Null => Ok(false),
                    _ => Err(anyhow!("JOIN filter must be boolean")),
                }
            }
        }
    }
}

#[async_trait]
impl PhysicalOperator for HashJoinOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.build_child.open(ctx).await?;
        self.probe_child.open(ctx).await?;

        // BUILD PHASE
        let mut hash_table = JoinHashTable::new(self.build_key_indices.clone());

        while let Some(row) = self.build_child.next(ctx).await? {
            hash_table.insert(row);

            if hash_table.memory_bytes() > self.config.max_memory_bytes {
                tracing::warn!(
                    "Hash join exceeded memory limit: {} > {}",
                    hash_table.memory_bytes(),
                    self.config.max_memory_bytes
                );
                // Continue anyway (no spill in v1)
            }
        }

        hash_table.finalize();

        // Initialize build-side tracking for RIGHT/FULL
        let build_matched = match self.join_type {
            HashJoinType::Right | HashJoinType::Full => {
                Some(vec![false; hash_table.total_row_count()])
            }
            _ => None,
        };

        self.state = HashJoinState::Probing {
            hash_table,
            current_probe_row: None,
            current_matches: Vec::new(),
            match_index: 0,
            build_matched,
            probe_had_match: false,
        };

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        loop {
            match &mut self.state {
                HashJoinState::Created => {
                    return Err(anyhow!("HashJoinOperator not opened"));
                }

                HashJoinState::Probing {
                    hash_table,
                    current_probe_row,
                    current_matches,
                    match_index,
                    build_matched,
                    probe_had_match,
                } => {
                    // Emit remaining matches for current probe row
                    while *match_index < current_matches.len() {
                        let (build_idx, build_row) = &current_matches[*match_index];
                        *match_index += 1;

                        let output = self.make_output_row(
                            current_probe_row.as_ref().unwrap(),
                            build_row,
                        );

                        if self.check_filter(&output)? {
                            // Mark build row as matched
                            if let Some(bm) = build_matched {
                                bm[*build_idx] = true;
                            }
                            *probe_had_match = true;
                            return Ok(Some(output));
                        }
                    }

                    // Emit unmatched probe row for LEFT/FULL
                    if !*probe_had_match && current_probe_row.is_some() {
                        if matches!(self.join_type, HashJoinType::Left | HashJoinType::Full) {
                            let row = self.make_probe_only_row(
                                current_probe_row.as_ref().unwrap()
                            );
                            *current_probe_row = None;
                            return Ok(Some(row));
                        }
                    }

                    // Get next probe row
                    match self.probe_child.next(ctx).await? {
                        Some(probe_row) => {
                            let key = self.extract_probe_key(&probe_row);
                            let matches: Vec<(usize, Row)> = hash_table
                                .probe(&key)
                                .map(|(i, r)| (i, r.clone()))
                                .collect();

                            *current_probe_row = Some(probe_row);
                            *current_matches = matches;
                            *match_index = 0;
                            *probe_had_match = false;
                            continue;
                        }
                        None => {
                            // Probe exhausted → emit unmatched build rows
                            if matches!(self.join_type, HashJoinType::Right | HashJoinType::Full) {
                                let bm = build_matched.take().unwrap_or_default();
                                let unmatched: Vec<Row> = hash_table
                                    .all_rows_with_indices()
                                    .filter(|(i, _)| !bm.get(*i).copied().unwrap_or(false))
                                    .map(|(_, r)| r.clone())
                                    .collect();

                                self.state = HashJoinState::EmittingUnmatched {
                                    unmatched_rows: unmatched,
                                    emit_index: 0,
                                };
                                continue;
                            }

                            self.state = HashJoinState::Exhausted;
                            return Ok(None);
                        }
                    }
                }

                HashJoinState::EmittingUnmatched {
                    unmatched_rows,
                    emit_index,
                } => {
                    if *emit_index < unmatched_rows.len() {
                        let row = self.make_build_only_row(&unmatched_rows[*emit_index]);
                        *emit_index += 1;
                        return Ok(Some(row));
                    }
                    self.state = HashJoinState::Exhausted;
                    return Ok(None);
                }

                HashJoinState::Exhausted => {
                    return Ok(None);
                }
            }
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.build_child.close(ctx).await?;
        self.probe_child.close(ctx).await?;
        self.state = HashJoinState::Exhausted;
        Ok(())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.probe_child.as_ref(), self.build_child.as_ref()]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![self.probe_child.as_mut(), self.build_child.as_mut()]
    }

    fn name(&self) -> &'static str {
        "HashJoin"
    }

    fn explain_info(&self) -> Option<String> {
        let jt = match self.join_type {
            HashJoinType::Inner => "INNER",
            HashJoinType::Left => "LEFT",
            HashJoinType::Right => "RIGHT",
            HashJoinType::Full => "FULL",
        };
        Some(format!(
            "type={}, build_keys={:?}, probe_keys={:?}",
            jt, self.build_key_indices, self.probe_key_indices
        ))
    }
}
```

#### 3.2.2 Phase 2 验收标准

- [ ] INNER JOIN 单元测试通过
- [ ] LEFT JOIN 单元测试通过
- [ ] RIGHT JOIN 单元测试通过
- [ ] FULL JOIN 单元测试通过
- [ ] 空表边界情况测试通过
- [ ] NULL key 测试通过

---

### Phase 3: Planner 集成（第 3 周）

#### 3.3.1 Join 算法选择

**文件**: `src/sql/planner.rs`

```rust
/// Result of join algorithm selection.
#[derive(Debug, Clone)]
pub enum JoinAlgorithmChoice {
    NestedLoop,
    HashJoin {
        left_is_build: bool,
        left_key_indices: Vec<usize>,
        right_key_indices: Vec<usize>,
    },
}

/// Choose join algorithm based on condition and estimates.
pub fn choose_join_algorithm(
    join_condition: Option<&Expr>,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
    left_row_estimate: usize,
    right_row_estimate: usize,
    config: &HashJoinConfig,
) -> JoinAlgorithmChoice {
    // No condition → cross join → nested loop
    let Some(cond) = join_condition else {
        return JoinAlgorithmChoice::NestedLoop;
    };

    // Try to extract equi-join keys
    let Some((left_keys, right_keys)) = extract_equi_join_keys(cond, left_schema, right_schema) else {
        return JoinAlgorithmChoice::NestedLoop;
    };

    // Too small → nested loop may be faster
    if left_row_estimate + right_row_estimate < config.min_rows_threshold {
        return JoinAlgorithmChoice::NestedLoop;
    }

    // Build smaller side
    let left_is_build = left_row_estimate <= right_row_estimate;

    JoinAlgorithmChoice::HashJoin {
        left_is_build,
        left_key_indices: left_keys,
        right_key_indices: right_keys,
    }
}

/// Extract equi-join key indices from ON condition.
///
/// Returns (left_key_indices, right_key_indices) or None if non-equi.
fn extract_equi_join_keys(
    expr: &Expr,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
) -> Option<(Vec<usize>, Vec<usize>)> {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            match op {
                BinaryOperator::Eq => {
                    extract_column_pair(left, right, left_schema, right_schema)
                }
                BinaryOperator::And => {
                    let (mut lk1, mut rk1) = extract_equi_join_keys(left, left_schema, right_schema)?;
                    let (lk2, rk2) = extract_equi_join_keys(right, left_schema, right_schema)?;
                    lk1.extend(lk2);
                    rk1.extend(rk2);
                    Some((lk1, rk1))
                }
                _ => None,
            }
        }
        Expr::Nested(inner) => extract_equi_join_keys(inner, left_schema, right_schema),
        _ => None,
    }
}

/// Extract (left_col_index, right_col_index) from `left_col = right_col`.
fn extract_column_pair(
    left_expr: &Expr,
    right_expr: &Expr,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let left_col = extract_column_name(left_expr)?;
    let right_col = extract_column_name(right_expr)?;

    // Try left.col = right.col
    if let (Some(li), Some(ri)) = (
        left_schema.column_index(&left_col),
        right_schema.column_index(&right_col),
    ) {
        return Some((vec![li], vec![ri]));
    }

    // Try right.col = left.col (swapped)
    if let (Some(li), Some(ri)) = (
        left_schema.column_index(&right_col),
        right_schema.column_index(&left_col),
    ) {
        return Some((vec![li], vec![ri]));
    }

    None
}

fn extract_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(normalize_ident(ident)),
        Expr::CompoundIdentifier(parts) => parts.last().map(normalize_ident),
        _ => None,
    }
}
```

#### 3.3.2 Executor 集成

**文件**: `src/sql/executor_operators.rs` 或 `executor_join.rs`

在创建 join operator 时调用 planner：

```rust
fn create_join_operator(
    left: BoxedOperator,
    right: BoxedOperator,
    join_type: JoinType,
    condition: Option<&Expr>,
    // ... other params
) -> BoxedOperator {
    let config = HashJoinConfig::default();
    
    // Estimate row counts (simple heuristic for now)
    let left_estimate = 1000; // TODO: use statistics
    let right_estimate = 1000;

    match choose_join_algorithm(
        condition,
        left.schema(),
        right.schema(),
        left_estimate,
        right_estimate,
        &config,
    ) {
        JoinAlgorithmChoice::HashJoin {
            left_is_build,
            left_key_indices,
            right_key_indices,
        } => {
            let hash_join_type = match join_type {
                JoinType::Inner => HashJoinType::Inner,
                JoinType::Left => HashJoinType::Left,
                JoinType::Right => HashJoinType::Right,
                JoinType::Full => HashJoinType::Full,
                JoinType::Cross => unreachable!(), // Cross uses nested loop
            };
            
            Box::new(HashJoinOperator::new(
                left,
                right,
                hash_join_type,
                left_key_indices,
                right_key_indices,
                left_is_build,
                None, // residual filter
                config,
            ))
        }
        JoinAlgorithmChoice::NestedLoop => {
            Box::new(NestedLoopJoinOperator::new(left, right, join_type, condition.cloned()))
        }
    }
}
```

#### 3.3.3 Phase 3 验收标准

- [ ] `extract_equi_join_keys` 单元测试通过
- [ ] `choose_join_algorithm` 单元测试通过
- [ ] EXPLAIN 显示 "HashJoin" 或 "NestedLoop"
- [ ] `./run_tests.sh` 全部通过

---

### Phase 4: 集成测试与优化（第 4 周）

#### 3.4.1 SQL 集成测试

**文件**: `tests/XX_hash_join.sql`

```sql
-- ============================================
-- Hash Join Integration Tests
-- ============================================

-- Setup
CREATE TABLE hj_left (id INT PRIMARY KEY, name TEXT, value INT);
CREATE TABLE hj_right (id INT PRIMARY KEY, left_id INT, data TEXT);

INSERT INTO hj_left VALUES (1, 'a', 100), (2, 'b', 200), (3, 'c', 300);
INSERT INTO hj_right VALUES (10, 1, 'x'), (20, 1, 'y'), (30, 2, 'z'), (40, 999, 'orphan');

-- ============================================
-- INNER JOIN
-- ============================================
SELECT l.id, l.name, r.data
FROM hj_left l
INNER JOIN hj_right r ON l.id = r.left_id
ORDER BY l.id, r.id;

-- Expected:
-- id|name|data
-- 1|a|x
-- 1|a|y
-- 2|b|z

-- ============================================
-- LEFT JOIN
-- ============================================
SELECT l.id, l.name, r.data
FROM hj_left l
LEFT JOIN hj_right r ON l.id = r.left_id
ORDER BY l.id, r.id NULLS LAST;

-- Expected:
-- id|name|data
-- 1|a|x
-- 1|a|y
-- 2|b|z
-- 3|c|NULL

-- ============================================
-- RIGHT JOIN
-- ============================================
SELECT l.id, l.name, r.data
FROM hj_left l
RIGHT JOIN hj_right r ON l.id = r.left_id
ORDER BY r.id;

-- Expected:
-- id|name|data
-- 1|a|x
-- 1|a|y
-- 2|b|z
-- NULL|NULL|orphan

-- ============================================
-- FULL JOIN
-- ============================================
SELECT l.id, l.name, r.data
FROM hj_left l
FULL JOIN hj_right r ON l.id = r.left_id
ORDER BY COALESCE(l.id, 0), COALESCE(r.id, 0);

-- Expected: all rows from both sides

-- ============================================
-- Multi-column key
-- ============================================
CREATE TABLE hj_multi_left (a INT, b INT, val TEXT);
CREATE TABLE hj_multi_right (x INT, y INT, info TEXT);

INSERT INTO hj_multi_left VALUES (1, 2, 'left1'), (1, 3, 'left2');
INSERT INTO hj_multi_right VALUES (1, 2, 'right1'), (1, 3, 'right2'), (9, 9, 'no_match');

SELECT l.val, r.info
FROM hj_multi_left l
JOIN hj_multi_right r ON l.a = r.x AND l.b = r.y
ORDER BY l.val;

-- ============================================
-- NULL in join key
-- ============================================
INSERT INTO hj_left VALUES (NULL, 'null_left', 0);
INSERT INTO hj_right VALUES (50, NULL, 'null_right');

SELECT l.id, l.name, r.data
FROM hj_left l
LEFT JOIN hj_right r ON l.id = r.left_id
WHERE l.id IS NULL OR l.id = 1
ORDER BY l.id NULLS FIRST, r.id;

-- ============================================
-- Empty table edge cases
-- ============================================
CREATE TABLE hj_empty (id INT);

SELECT * FROM hj_left l JOIN hj_empty e ON l.id = e.id;
SELECT * FROM hj_empty e JOIN hj_left l ON e.id = l.id;
SELECT * FROM hj_left l LEFT JOIN hj_empty e ON l.id = e.id ORDER BY l.id;

-- ============================================
-- Cleanup
-- ============================================
DROP TABLE hj_left;
DROP TABLE hj_right;
DROP TABLE hj_multi_left;
DROP TABLE hj_multi_right;
DROP TABLE hj_empty;
```

#### 3.4.2 性能基准测试

**文件**: `scripts/benchmark_hash_join.py`

```python
#!/usr/bin/env python3
"""Benchmark Hash Join vs Nested Loop."""

import psycopg2
import time
import os

DSN = os.environ.get("PG_DSN", "postgres://admin:admin@127.0.0.1:5433/postgres")

def setup_tables(cur, size):
    cur.execute("DROP TABLE IF EXISTS bench_left, bench_right")
    cur.execute(f"""
        CREATE TABLE bench_left AS
        SELECT generate_series(1, {size}) as id,
               'name_' || generate_series(1, {size}) as name
    """)
    cur.execute(f"""
        CREATE TABLE bench_right AS
        SELECT generate_series(1, {size}) as id,
               generate_series(1, {size}) * 10 as value
    """)

def benchmark_join(cur, query):
    start = time.time()
    cur.execute(query)
    result = cur.fetchall()
    elapsed = time.time() - start
    return elapsed, len(result)

def main():
    conn = psycopg2.connect(DSN)
    conn.autocommit = True
    cur = conn.cursor()

    print("Size\tRows\tTime(s)")
    print("-" * 40)

    for size in [100, 1000, 5000, 10000]:
        setup_tables(cur, size)
        
        query = """
            SELECT COUNT(*)
            FROM bench_left l
            JOIN bench_right r ON l.id = r.id
        """
        
        elapsed, count = benchmark_join(cur, query)
        print(f"{size}\t{count}\t{elapsed:.3f}")

    cur.execute("DROP TABLE IF EXISTS bench_left, bench_right")
    conn.close()

if __name__ == "__main__":
    main()
```

#### 3.4.3 Phase 4 验收标准

- [ ] `tests/XX_hash_join.sql` 与 `.expected` 完全匹配
- [ ] `./run_tests.sh` 全部通过（包括 ORM 测试）
- [ ] 性能基准：10K×10K JOIN < 1秒
- [ ] 内存使用合理（不超过配置限制的 2x）

---

## 4. 测试矩阵

### 4.1 功能测试

| 测试类别 | 测试用例 | 优先级 |
|----------|----------|--------|
| INNER JOIN | 基本匹配 | P0 |
| INNER JOIN | 多行匹配同一 key | P0 |
| INNER JOIN | 无匹配 | P0 |
| LEFT JOIN | 基本 + unmatched | P0 |
| RIGHT JOIN | 基本 + unmatched | P0 |
| FULL JOIN | 两侧 unmatched | P1 |
| Multi-key | `ON a=x AND b=y` | P0 |
| NULL key | 两侧都有 NULL | P0 |
| Empty table | 左空/右空/双空 | P1 |
| Filter | 带额外 WHERE 条件 | P1 |
| Type coercion | INT32 = INT64 | P1 |

### 4.2 边界测试

| 场景 | 预期行为 |
|------|----------|
| 所有 build 行都是 NULL key | INNER 返回空，LEFT 返回 probe 行 |
| 所有 probe 行都是 NULL key | INNER 返回空，RIGHT 返回 build 行 |
| 极高重复率（所有行同 key） | 正确处理，不崩溃 |
| 单行表 JOIN 单行表 | 正确匹配 |

### 4.3 性能测试

| 规模 | Nested Loop 预期 | Hash Join 目标 |
|------|------------------|----------------|
| 100×100 | <100ms | <10ms |
| 1K×1K | ~1s | <50ms |
| 10K×10K | ~100s | <500ms |
| 100K×100K | timeout | <5s |

---

## 5. 风险与缓解

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| OUTER JOIN 结果错误 | 中 | 高 | 详尽测试，与 PostgreSQL 对比 |
| 内存溢出 | 低 | 高 | 配置限制，日志告警 |
| 列顺序错误 | 中 | 中 | `left_is_build` 追踪 |
| 类型不匹配漏检 | 低 | 中 | Planner 检查 + 运行时比较 |
| 性能回退 | 低 | 低 | 小表仍用 Nested Loop |

---

## 6. 里程碑

| 周次 | 交付物 | 验收标准 |
|------|--------|----------|
| Week 1 | hash 函数 + JoinHashTable | 单元测试 100% 通过 |
| Week 2 | HashJoinOperator (all types) | Operator 单元测试通过 |
| Week 3 | Planner 集成 + EXPLAIN | `./run_tests.sh` 通过 |
| Week 4 | 集成测试 + 性能优化 | 所有测试通过，性能达标 |

---

## 7. 文件清单

### 新增文件

| 文件 | 行数估计 | 说明 |
|------|----------|------|
| `src/sql/operators/hash_join.rs` | ~600 | 核心实现 |
| `tests/XX_hash_join.sql` | ~100 | 集成测试 |
| `tests/XX_hash_join.expected` | ~50 | 预期输出 |
| `scripts/benchmark_hash_join.py` | ~50 | 性能测试 |

### 修改文件

| 文件 | 修改内容 |
|------|----------|
| `src/sql/operators/mod.rs` | 添加 hash_join 模块导出 |
| `src/sql/planner.rs` | 添加 join 算法选择逻辑 |
| `src/sql/executor_operators.rs` | 集成 Hash Join |

---

## 8. 附录：设计决策记录

### D1: 为什么选择 SipHash 而非更快的 hash？

**决策**: 使用 `DefaultHasher` (SipHash-1-3)

**原因**:
- 防止 hash collision 攻击（用户可控制 join key）
- 性能足够（不是瓶颈）
- 标准库内置，无额外依赖

**替代方案**: ahash（更快但需要依赖）- 可在 v2 考虑

### D2: 为什么不实现 Grace Hash Join？

**决策**: 第一版不实现磁盘溢出

**原因**:
- 复杂度高（需要分区、磁盘 I/O）
- 256MB 内存限制覆盖大多数场景
- 可在 v2 按需添加

### D3: 为什么 probe 返回 `(index, row)` 而非只返回 row？

**决策**: 返回 `(global_index, &Row)`

**原因**:
- RIGHT/FULL JOIN 需要追踪哪些 build 行被匹配
- Index 用于设置 `build_matched[idx] = true`
- 避免额外的 map 查找

### D4: 为什么输出列顺序用 `left_is_build` 追踪？

**决策**: 始终输出 left + right 列顺序

**原因**:
- 用户预期 `SELECT * FROM a JOIN b` 返回 a 的列在前
- 如果 a 是 build 侧（因为更小），内部是 build + probe
- 需要在输出时重新排列为 left + right

---

## 9. 参考资料

- PostgreSQL Hash Join: `src/backend/executor/nodeHashjoin.c`
- DuckDB Hash Join: `src/execution/operator/join/physical_hash_join.cpp`
- DataFusion Hash Join: `datafusion/physical-plan/src/joins/hash_join.rs`
- 论文: "A Practical Introduction to Hash-Based Join Algorithms" (Bratbergsengen)
