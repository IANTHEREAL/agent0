# PRD-D06: GIN 索引查询优化

**阶段**: Phase 2 (P2)  
**预估**: 3-5 天  
**依赖**: PRD-D02 (JSONB @>)

## 背景

Dify 在 JSONB 列上创建 GIN 索引以加速包含查询：

```sql
CREATE INDEX document_metadata_idx ON documents USING gin (doc_metadata);
SELECT * FROM documents WHERE doc_metadata @> '{"type": "pdf"}';
```

当前 db9-server：
- 可以创建 GIN 索引（DDL 兼容）
- **不会使用 GIN 索引**（全表扫描）

## 目标

实现 GIN 索引的查询加速，针对 JSONB `@>` 操作符。

## 设计概览

### GIN 索引存储结构

GIN (Generalized Inverted Index) 将 JSONB 中的每个键/值路径单独索引：

```
JSONB: {"type": "pdf", "author": {"name": "John"}}

索引条目:
  "type" → [row_id_1, row_id_5, ...]
  "type"="pdf" → [row_id_1, ...]
  "author" → [row_id_1, row_id_2, ...]
  "author"."name" → [row_id_1, ...]
  "author"."name"="John" → [row_id_1, ...]
```

### 存储 Key 格式

```
i_{table_id}_{index_id}_gin_{path_hash}_{pk}
```

其中 `path_hash` = hash(JSON 路径 + 值)

## 实现步骤

### Phase 2a: 索引写入 (2 天)

#### 1) JSONB 路径提取

```rust
// src/sql/gin.rs

pub fn extract_gin_entries(json: &JsonValue, prefix: &str) -> Vec<GinEntry> {
    let mut entries = Vec::new();
    
    match json {
        JsonValue::Object(map) => {
            for (key, val) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", prefix, key)
                };
                
                // 键存在条目
                entries.push(GinEntry::KeyExists(path.clone()));
                
                // 键值对条目
                if let Some(scalar) = val.as_scalar() {
                    entries.push(GinEntry::KeyValue(path.clone(), scalar));
                }
                
                // 递归嵌套对象
                if val.is_object() || val.is_array() {
                    entries.extend(extract_gin_entries(val, &path));
                }
            }
        }
        JsonValue::Array(arr) => {
            for (i, val) in arr.iter().enumerate() {
                let path = format!("{}[{}]", prefix, i);
                entries.extend(extract_gin_entries(val, &path));
            }
        }
        _ => {}
    }
    
    entries
}

pub enum GinEntry {
    KeyExists(String),
    KeyValue(String, String),
}
```

#### 2) 索引写入

在 INSERT/UPDATE 时写入 GIN 条目：

```rust
// src/sql/dml.rs

async fn write_gin_index_entries(
    txn: &mut Transaction,
    table_id: u64,
    index: &IndexDef,
    row: &Row,
    pk: &[u8],
) -> Result<()> {
    if index.method.as_deref() != Some("gin") {
        return Ok(());
    }
    
    // 获取 JSONB 列值
    let col_idx = index.columns[0]; // GIN 通常单列
    let json = row.values[col_idx].as_jsonb()?;
    
    // 提取所有条目
    let entries = extract_gin_entries(json, "");
    
    // 写入索引
    for entry in entries {
        let key = format!(
            "i_{}_{}_gin_{}_{}",
            table_id,
            index.id,
            entry.hash(),
            hex::encode(pk)
        );
        txn.put(key.as_bytes(), &[]).await?;
    }
    
    Ok(())
}
```

### Phase 2b: 查询加速 (2-3 天)

#### 1) 查询规划器识别

```rust
// src/sql/planner.rs

fn can_use_gin_index(
    index: &IndexDef,
    predicate: &Expr,
) -> Option<GinScanPlan> {
    if index.method.as_deref() != Some("gin") {
        return None;
    }
    
    // 检查是否是 @> 操作符
    if let Expr::BinaryOp { left, op: BinaryOperator::Contains, right } = predicate {
        // left 必须是索引列
        // right 必须是常量 JSONB
        let col_name = extract_column_name(left)?;
        let pattern = extract_jsonb_literal(right)?;
        
        if index.columns.iter().any(|c| c == col_name) {
            return Some(GinScanPlan {
                index_id: index.id,
                pattern,
            });
        }
    }
    
    None
}
```

#### 2) GIN 索引扫描

```rust
// src/sql/executor.rs

async fn execute_gin_scan(
    txn: &mut Transaction,
    table: &TableSchema,
    index: &IndexDef,
    pattern: &JsonValue,
) -> Result<Vec<Row>> {
    // 从 pattern 提取查询条目
    let query_entries = extract_gin_entries(pattern, "");
    
    // 对每个条目查询索引
    let mut pk_sets: Vec<HashSet<Vec<u8>>> = Vec::new();
    
    for entry in &query_entries {
        let prefix = format!(
            "i_{}_{}_gin_{}",
            table.table_id,
            index.id,
            entry.hash()
        );
        
        let pairs = txn.scan(prefix.as_bytes().to_vec().., 10000).await?;
        let pks: HashSet<_> = pairs.into_iter()
            .map(|(k, _)| extract_pk_from_gin_key(&k))
            .collect();
        
        pk_sets.push(pks);
    }
    
    // 求交集（所有条件都必须满足）
    let matching_pks = pk_sets.into_iter()
        .reduce(|a, b| a.intersection(&b).cloned().collect())
        .unwrap_or_default();
    
    // 获取行并验证（防止假阳性）
    let mut results = Vec::new();
    for pk in matching_pks {
        let row = fetch_row_by_pk(txn, table, &pk).await?;
        // 重新验证 @> 条件
        if jsonb_contains(&row.get_jsonb(index.columns[0])?, pattern) {
            results.push(row);
        }
    }
    
    Ok(results)
}
```

## 测试

### 性能测试

```sql
-- 创建测试表
CREATE TABLE gin_test (
    id SERIAL PRIMARY KEY,
    metadata JSONB
);

-- 插入 10000 行
INSERT INTO gin_test (metadata)
SELECT jsonb_build_object(
    'type', CASE WHEN i % 10 = 0 THEN 'pdf' ELSE 'doc' END,
    'size', i * 100,
    'author', jsonb_build_object('name', 'user_' || i)
)
FROM generate_series(1, 10000) i;

-- 创建 GIN 索引
CREATE INDEX gin_test_metadata_idx ON gin_test USING gin (metadata);

-- 查询（应该使用索引）
EXPLAIN SELECT * FROM gin_test WHERE metadata @> '{"type": "pdf"}';
-- Expected: 显示 GIN Index Scan

-- 验证正确性
SELECT COUNT(*) FROM gin_test WHERE metadata @> '{"type": "pdf"}';
-- Expected: 1000
```

### 正确性测试

```sql
-- 嵌套对象查询
SELECT * FROM gin_test WHERE metadata @> '{"author": {"name": "user_1"}}';
-- Expected: 1 row

-- 不存在的键
SELECT * FROM gin_test WHERE metadata @> '{"nonexistent": true}';
-- Expected: 0 rows
```

## EXPLAIN 输出

```
Index Scan using gin_test_metadata_idx on gin_test
  Index Cond: (metadata @> '{"type": "pdf"}'::jsonb)
  Rows Removed by Filter: 0
  Actual Rows: 1000
```

## 限制

### MVP 限制

- 仅支持 `@>` 操作符
- 仅支持单列 GIN 索引
- 不支持 `?`, `?|`, `?&` 的索引加速

### 后续增强

- 支持 `@>` + `AND` 组合条件
- 支持键存在查询 (`?`)
- 支持部分 GIN 索引 (`WHERE`)
- 支持数组元素查询

## 验收标准

```sql
-- 创建 GIN 索引
CREATE INDEX test_gin ON test_table USING gin (jsonb_col);

-- 查询使用索引
EXPLAIN SELECT * FROM test_table WHERE jsonb_col @> '{"key": "value"}';
-- Expected: 包含 "GIN Index Scan" 或 "Index Scan"

-- 结果正确
SELECT COUNT(*) FROM test_table WHERE jsonb_col @> '{"key": "value"}';
-- Expected: 与全表扫描结果一致
```

## 风险

| 风险 | 影响 | 缓解 |
|------|------|------|
| 索引膨胀 | 存储空间 | 限制嵌套深度 |
| 假阳性 | 额外 I/O | 最终验证 |
| 写放大 | 写入性能 | 批量写入优化 |
