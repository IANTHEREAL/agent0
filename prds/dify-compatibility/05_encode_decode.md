# PRD-D05: encode() / decode() 函数

**阶段**: Phase 1 (P1)  
**预估**: 2 小时  
**依赖**: 无

## 背景

Dify 的 `uuidv7()` 函数最后一步需要 `encode(..., 'hex')` 将 bytea 转为十六进制字符串：

```sql
encode(
    set_bit(...),
    'hex')::uuid
```

## 目标

实现 `encode()` 和 `decode()` 函数，支持常用编码格式。

## 函数签名

```sql
encode(data bytea, format text) → text
decode(string text, format text) → bytea
```

## 支持的格式

| 格式 | 说明 | 示例 |
|------|------|------|
| `hex` | 十六进制 | `\x48656c6c6f` → `'48656c6c6f'` |
| `base64` | Base64 | `\x48656c6c6f` → `'SGVsbG8='` |
| `escape` | PostgreSQL 转义 | `\x48656c6c6f` → `'Hello'` |

## 实现

### 1) encode()

```rust
// src/sql/expr.rs - eval_function()

"ENCODE" => {
    let data = args[0].as_bytea()?;
    let format = args[1].as_text()?.to_lowercase();
    
    let result = match format.as_str() {
        "hex" => {
            // 转换为十六进制字符串
            data.iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        }
        "base64" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(data)
        }
        "escape" => {
            // PostgreSQL escape 格式
            data.iter()
                .map(|&b| {
                    if b >= 32 && b < 127 && b != b'\\' {
                        (b as char).to_string()
                    } else {
                        format!("\\{:03o}", b)
                    }
                })
                .collect::<String>()
        }
        _ => return Err(anyhow!("unrecognized encoding: {}", format)),
    };
    
    Ok(Value::Text(result))
}
```

### 2) decode()

```rust
"DECODE" => {
    let string = args[0].as_text()?;
    let format = args[1].as_text()?.to_lowercase();
    
    let result = match format.as_str() {
        "hex" => {
            // 十六进制字符串转 bytea
            let s = string.trim();
            if s.len() % 2 != 0 {
                return Err(anyhow!("invalid hex string length"));
            }
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i+2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|e| anyhow!("invalid hex: {}", e))?
        }
        "base64" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(string)
                .map_err(|e| anyhow!("invalid base64: {}", e))?
        }
        "escape" => {
            // PostgreSQL escape 格式解码
            decode_escape(string)?
        }
        _ => return Err(anyhow!("unrecognized encoding: {}", format)),
    };
    
    Ok(Value::Bytea(result))
}
```

### 3) escape 解码辅助

```rust
fn decode_escape(s: &str) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut chars = s.chars().peekable();
    
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('\\') => {
                    chars.next();
                    result.push(b'\\');
                }
                Some(c) if c.is_ascii_digit() => {
                    // 八进制转义 \NNN
                    let mut octal = String::new();
                    for _ in 0..3 {
                        if let Some(&c) = chars.peek() {
                            if c.is_ascii_digit() {
                                octal.push(chars.next().unwrap());
                            } else {
                                break;
                            }
                        }
                    }
                    let byte = u8::from_str_radix(&octal, 8)
                        .map_err(|_| anyhow!("invalid escape sequence"))?;
                    result.push(byte);
                }
                _ => result.push(b'\\'),
            }
        } else {
            result.push(c as u8);
        }
    }
    
    Ok(result)
}
```

## 测试

### SQL 测试

```sql
-- encode hex
SELECT encode('\x48656c6c6f'::bytea, 'hex');
-- Expected: '48656c6c6f'

SELECT encode('\xdeadbeef'::bytea, 'hex');
-- Expected: 'deadbeef'

-- decode hex
SELECT decode('48656c6c6f', 'hex');
-- Expected: \x48656c6c6f (Hello)

-- encode base64
SELECT encode('\x48656c6c6f'::bytea, 'base64');
-- Expected: 'SGVsbG8='

-- decode base64
SELECT decode('SGVsbG8=', 'base64');
-- Expected: \x48656c6c6f

-- 往返测试
SELECT decode(encode('\xdeadbeef'::bytea, 'hex'), 'hex') = '\xdeadbeef'::bytea;
-- Expected: true

-- uuidv7 关键路径
SELECT encode('\x0189123456781234567890abcdef1234'::bytea, 'hex')::uuid;
-- Expected: valid UUID
```

### 单元测试

```rust
#[test]
fn test_encode_hex() {
    let result = eval_encode(&[0xde, 0xad, 0xbe, 0xef], "hex");
    assert_eq!(result, "deadbeef");
}

#[test]
fn test_decode_hex() {
    let result = eval_decode("deadbeef", "hex");
    assert_eq!(result, vec![0xde, 0xad, 0xbe, 0xef]);
}

#[test]
fn test_roundtrip() {
    let original = vec![0x01, 0x89, 0x12, 0x34, 0x56, 0x78];
    let encoded = eval_encode(&original, "hex");
    let decoded = eval_decode(&encoded, "hex");
    assert_eq!(original, decoded);
}
```

## 验收标准

```sql
-- 必须通过
SELECT encode('\xdeadbeef'::bytea, 'hex') = 'deadbeef';
SELECT decode('deadbeef', 'hex') = '\xdeadbeef'::bytea;
SELECT decode(encode('hello'::bytea, 'base64'), 'base64') = 'hello'::bytea;

-- uuidv7 最终验证
SELECT encode('\x0189123456781234567890abcdef1234'::bytea, 'hex')::uuid IS NOT NULL;
```

## 完成后

配合 PRD-D04 (bytea functions)，即可支持完整的 uuidv7() 函数定义和执行。
