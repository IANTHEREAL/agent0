# PRD-D04: Bytea 底层函数

**阶段**: Phase 1 (P1)  
**预估**: 4 小时  
**依赖**: 无

## 背景

Dify 的 `uuidv7()` 函数依赖多个 bytea 底层函数：

```sql
CREATE FUNCTION public.uuidv7() RETURNS uuid AS $$
SELECT encode(
    set_bit(
        set_bit(
            overlay(uuid_send(gen_random_uuid()) placing
                substring(int8send((extract(epoch from clock_timestamp()) * 1000)::bigint) from 3)
            from 1 for 6),
        52, 1),
    53, 1), 'hex')::uuid;
$$;
```

需实现：`set_bit()`, `int8send()`, `uuid_send()`

## 目标

实现 PostgreSQL bytea 底层函数，支持 uuidv7() 自定义函数。

## 函数列表

| 函数 | 签名 | 说明 |
|------|------|------|
| `set_bit` | `set_bit(bytea, int, int) → bytea` | 设置指定位 |
| `get_bit` | `get_bit(bytea, int) → int` | 获取指定位 |
| `int8send` | `int8send(bigint) → bytea` | bigint 转 8 字节 bytea |
| `int4send` | `int4send(int) → bytea` | int 转 4 字节 bytea |
| `uuid_send` | `uuid_send(uuid) → bytea` | uuid 转 16 字节 bytea |

## 实现

### 1) set_bit / get_bit

```rust
// src/sql/expr.rs - eval_function()

"SET_BIT" => {
    let bytes = args[0].as_bytea()?;
    let bit_pos = args[1].as_i32()? as usize;
    let new_val = args[2].as_i32()?;
    
    if new_val != 0 && new_val != 1 {
        return Err(anyhow!("set_bit: new value must be 0 or 1"));
    }
    
    let byte_pos = bit_pos / 8;
    let bit_offset = 7 - (bit_pos % 8); // big-endian bit order
    
    if byte_pos >= bytes.len() {
        return Err(anyhow!("set_bit: bit index {} out of range", bit_pos));
    }
    
    let mut result = bytes.to_vec();
    if new_val == 1 {
        result[byte_pos] |= 1 << bit_offset;
    } else {
        result[byte_pos] &= !(1 << bit_offset);
    }
    
    Ok(Value::Bytea(result))
}

"GET_BIT" => {
    let bytes = args[0].as_bytea()?;
    let bit_pos = args[1].as_i32()? as usize;
    
    let byte_pos = bit_pos / 8;
    let bit_offset = 7 - (bit_pos % 8);
    
    if byte_pos >= bytes.len() {
        return Err(anyhow!("get_bit: bit index {} out of range", bit_pos));
    }
    
    let bit_val = (bytes[byte_pos] >> bit_offset) & 1;
    Ok(Value::Int32(bit_val as i32))
}
```

### 2) int8send / int4send

```rust
"INT8SEND" => {
    let val = args[0].as_i64()?;
    Ok(Value::Bytea(val.to_be_bytes().to_vec()))
}

"INT4SEND" => {
    let val = args[0].as_i32()?;
    Ok(Value::Bytea(val.to_be_bytes().to_vec()))
}
```

### 3) uuid_send

```rust
"UUID_SEND" => {
    let uuid = args[0].as_uuid()?;
    Ok(Value::Bytea(uuid.as_bytes().to_vec()))
}
```

### 4) 类型转换辅助

```rust
// src/types/mod.rs

impl Value {
    pub fn as_bytea(&self) -> Result<&[u8]> {
        match self {
            Value::Bytea(b) => Ok(b),
            _ => Err(anyhow!("expected bytea")),
        }
    }
    
    pub fn as_uuid(&self) -> Result<uuid::Uuid> {
        match self {
            Value::Uuid(u) => Ok(*u),
            _ => Err(anyhow!("expected uuid")),
        }
    }
}
```

## 测试

### SQL 测试

```sql
-- int8send
SELECT int8send(1234567890123456789);
-- Expected: bytea (8 bytes, big-endian)

SELECT length(int8send(0));
-- Expected: 8

-- uuid_send
SELECT length(uuid_send(gen_random_uuid()));
-- Expected: 16

-- set_bit
SELECT set_bit('\x00'::bytea, 0, 1);
-- Expected: \x80 (highest bit set)

SELECT set_bit('\x00'::bytea, 7, 1);
-- Expected: \x01 (lowest bit set)

SELECT get_bit('\x80'::bytea, 0);
-- Expected: 1

SELECT get_bit('\x80'::bytea, 7);
-- Expected: 0

-- 组合测试（uuidv7 子表达式）
SELECT substring(int8send(1705312800000::bigint) from 3);
-- Expected: 6 bytes
```

### 单元测试

```rust
#[test]
fn test_int8send() {
    let result = eval_function("int8send", &[Value::Int64(0x0102030405060708)]);
    assert_eq!(result, Value::Bytea(vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]));
}

#[test]
fn test_set_bit() {
    let bytes = Value::Bytea(vec![0x00]);
    let result = eval_set_bit(&bytes, 0, 1);
    assert_eq!(result, Value::Bytea(vec![0x80]));
}

#[test]
fn test_uuid_send() {
    let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let result = eval_function("uuid_send", &[Value::Uuid(uuid)]);
    assert_eq!(result.as_bytea().unwrap().len(), 16);
}
```

## 验收标准

```sql
-- 必须通过（uuidv7 核心组件）
SELECT length(int8send(0)) = 8;
SELECT length(uuid_send(gen_random_uuid())) = 16;
SELECT get_bit(set_bit('\x00'::bytea, 0, 1), 0) = 1;
SELECT substring(int8send(1705312800000::bigint) from 3 for 6) IS NOT NULL;
```

## 后续

完成本 PRD 后，配合 PRD-D05 (encode/decode) 即可支持完整的 uuidv7() 函数。
