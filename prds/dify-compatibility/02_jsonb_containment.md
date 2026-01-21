# PRD-D02: JSONB @> 包含操作符

**阶段**: Phase 1 (P0)  
**预估**: 3 小时  
**依赖**: 无

## 背景

Dify 使用 JSONB 存储元数据，并通过 GIN 索引加速包含查询：

```sql
CREATE INDEX document_metadata_idx ON documents USING gin (doc_metadata);
SELECT * FROM documents WHERE doc_metadata @> '{"type": "pdf"}';
```

当前 pg-tikv 支持 `->` 和 `->>` 操作符，但缺少 `@>` 包含检查。

## 目标

实现 JSONB 包含操作符：
- `@>` (contains)
- `<@` (contained by)
- `?` (key exists)
- `?|` (any key exists)
- `?&` (all keys exist)

## 语法

```sql
-- 包含检查
jsonb_column @> '{"key": "value"}'   -- column 包含右侧
jsonb_column <@ '{"key": "value"}'   -- column 被右侧包含

-- 键存在检查
jsonb_column ? 'key'                 -- 键存在
jsonb_column ?| array['a', 'b']      -- 任一键存在
jsonb_column ?& array['a', 'b']      -- 所有键存在
```

## 需求

### 包含语义 (@>)

左侧 JSONB **包含** 右侧 JSONB 当且仅当：

1. **对象**: 右侧每个键值对都存在于左侧
   ```json
   {"a": 1, "b": 2} @> {"a": 1}  -- true
   {"a": 1} @> {"a": 1, "b": 2}  -- false
   ```

2. **数组**: 右侧每个元素都存在于左侧（顺序无关）
   ```json
   [1, 2, 3] @> [2, 1]  -- true
   [1, 2] @> [1, 2, 3]  -- false
   ```

3. **嵌套**: 递归检查
   ```json
   {"a": {"b": 1}} @> {"a": {"b": 1}}  -- true
   {"a": {"b": 1, "c": 2}} @> {"a": {"b": 1}}  -- true
   ```

## 实现

### 1) 操作符求值

在 `expr.rs` 中添加 JSONB 包含操作符：

```rust
// src/sql/expr.rs - eval_binary_op()

BinaryOperator::Contains => {
    // @> operator
    match (&left_val, &right_val) {
        (Value::Jsonb(left), Value::Jsonb(right)) => {
            Ok(Value::Boolean(jsonb_contains(left, right)))
        }
        _ => Err(anyhow!("@> requires jsonb operands")),
    }
}

BinaryOperator::ContainedBy => {
    // <@ operator
    match (&left_val, &right_val) {
        (Value::Jsonb(left), Value::Jsonb(right)) => {
            Ok(Value::Boolean(jsonb_contains(right, left)))
        }
        _ => Err(anyhow!("<@ requires jsonb operands")),
    }
}
```

### 2) 包含检查实现

```rust
// src/sql/jsonb.rs

use serde_json::Value as JsonValue;

/// 检查 container 是否包含 containee
pub fn jsonb_contains(container: &JsonValue, containee: &JsonValue) -> bool {
    match (container, containee) {
        // 对象包含：每个键值对都必须匹配
        (JsonValue::Object(c_map), JsonValue::Object(e_map)) => {
            e_map.iter().all(|(key, e_val)| {
                c_map.get(key)
                    .map(|c_val| jsonb_contains(c_val, e_val))
                    .unwrap_or(false)
            })
        }
        
        // 数组包含：每个元素都必须存在
        (JsonValue::Array(c_arr), JsonValue::Array(e_arr)) => {
            e_arr.iter().all(|e_elem| {
                c_arr.iter().any(|c_elem| jsonb_equal(c_elem, e_elem))
            })
        }
        
        // 标量：直接相等
        _ => jsonb_equal(container, containee),
    }
}

fn jsonb_equal(a: &JsonValue, b: &JsonValue) -> bool {
    match (a, b) {
        (JsonValue::Null, JsonValue::Null) => true,
        (JsonValue::Bool(a), JsonValue::Bool(b)) => a == b,
        (JsonValue::Number(a), JsonValue::Number(b)) => {
            // 数值比较需处理精度
            a.as_f64() == b.as_f64()
        }
        (JsonValue::String(a), JsonValue::String(b)) => a == b,
        (JsonValue::Array(a), JsonValue::Array(b)) => {
            a.len() == b.len() && 
            a.iter().zip(b.iter()).all(|(x, y)| jsonb_equal(x, y))
        }
        (JsonValue::Object(a), JsonValue::Object(b)) => {
            a.len() == b.len() &&
            a.iter().all(|(k, v)| {
                b.get(k).map(|bv| jsonb_equal(v, bv)).unwrap_or(false)
            })
        }
        _ => false,
    }
}
```

### 3) 键存在操作符

```rust
// src/sql/expr.rs - eval_binary_op()

BinaryOperator::QuestionMark => {
    // ? operator (key exists)
    match (&left_val, &right_val) {
        (Value::Jsonb(json), Value::Text(key)) => {
            let exists = match json {
                JsonValue::Object(map) => map.contains_key(key),
                JsonValue::Array(arr) => arr.iter().any(|v| {
                    matches!(v, JsonValue::String(s) if s == key)
                }),
                _ => false,
            };
            Ok(Value::Boolean(exists))
        }
        _ => Err(anyhow!("? requires jsonb and text")),
    }
}

// ?| and ?& 类似实现
```

## 测试

### SQL 测试

```sql
-- 对象包含
SELECT '{"a": 1, "b": 2}'::jsonb @> '{"a": 1}';
-- Expected: true

SELECT '{"a": 1}'::jsonb @> '{"a": 1, "b": 2}';
-- Expected: false

-- 嵌套对象
SELECT '{"a": {"b": 1, "c": 2}}'::jsonb @> '{"a": {"b": 1}}';
-- Expected: true

-- 数组包含
SELECT '[1, 2, 3]'::jsonb @> '[2, 1]';
-- Expected: true

-- 键存在
SELECT '{"a": 1, "b": 2}'::jsonb ? 'a';
-- Expected: true

SELECT '{"a": 1}'::jsonb ? 'b';
-- Expected: false

-- Dify 实际模式
SELECT * FROM (
    SELECT 1 as id, '{"type": "pdf", "author": "test"}'::jsonb as doc_metadata
) t WHERE doc_metadata @> '{"type": "pdf"}';
-- Expected: 1 row
```

## 验收标准

```sql
-- 必须通过
SELECT '{"a":1,"b":2}'::jsonb @> '{"a":1}';  -- true
SELECT '[1,2,3]'::jsonb @> '[1,2]';          -- true
SELECT '{"a":{"b":1}}'::jsonb @> '{"a":{}}'; -- true
SELECT '{"key":"val"}'::jsonb ? 'key';       -- true
```

## 性能说明

当前实现为全表扫描 + 逐行检查。GIN 索引加速将在 PRD-D06 中实现。
