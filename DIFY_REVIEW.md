# Dify PostgreSQL 兼容性测试报告

**测试时间**: 2026-01-21
**目标数据库**: (redacted)
**数据库版本**: PostgreSQL 15.0 (兼容)

## 一、当前阻塞 Dify 启动的关键问题

### 1. hstore 扩展支持问题 [严重/阻塞]

**错误信息**:
```
psycopg2.DatabaseError: error with status PGRES_EMPTY_QUERY and no message from the libpq
```

**原因分析**:
SQLAlchemy 的 psycopg2 方言在连接时会自动检测 hstore 扩展的 OID。它执行以下查询：
```sql
SELECT t.oid, t.typarray
FROM pg_type t
JOIN pg_namespace ns ON ns.oid = t.typnamespace
WHERE t.typname = 'hstore' AND ns.nspname = 'public';
```

当数据库不支持 hstore 扩展时，这个查询返回空结果，psycopg2 无法正确处理。

**测试结果**:
- `CREATE EXTENSION IF NOT EXISTS hstore;` → ERROR: Extension 'hstore' is not available
- pg_type 中没有 hstore 类型

**解决方案**:
1. **方案 A (推荐)**: 实现 hstore 扩展支持
2. **方案 B**: 在 pg_type 系统表中添加 hstore 类型的虚拟条目（即使不实际支持 hstore 功能）
3. **方案 C**: 需要修改 Dify 代码，禁用 hstore 检测

---

## 二、功能兼容性测试结果

### 支持的功能 ✅

| 功能 | 测试结果 | 备注 |
|------|----------|------|
| **基础类型** | | |
| UUID 类型 | ✅ 支持 | |
| JSONB 类型 | ✅ 支持 | |
| BYTEA 类型 | ✅ 支持 | |
| TEXT 类型 | ✅ 支持 | |
| BOOLEAN 类型 | ✅ 支持 | |
| NUMERIC/DECIMAL | ✅ 支持 | |
| TIMESTAMP WITH TIME ZONE | ✅ 支持 | |
| ARRAY 类型 | ✅ 支持 | |
| SERIAL/BIGSERIAL | ✅ 支持 | |
| **UUID 函数** | | |
| uuid_generate_v4() | ✅ 支持 | |
| gen_random_uuid() | ✅ 支持 | |
| **uuidv7 相关函数** | | Dify 的迁移脚本使用这些来创建 uuidv7 函数 |
| set_bit() | ✅ 支持 | |
| uuid_send() | ✅ 支持 | |
| int8send() | ✅ 支持 | |
| overlay() | ✅ 支持 | |
| encode(..., 'hex') | ✅ 支持 | |
| clock_timestamp() | ✅ 支持 | |
| extract(epoch from ...) | ✅ 支持 | |
| **JSONB 操作** | | |
| JSONB 操作符 (->, ->>) | ✅ 支持 | |
| JSONB 路径提取 (#>>) | ✅ 支持 | |
| JSONB 包含操作符 (@>) | ✅ 支持 | |
| to_jsonb() | ✅ 支持 | |
| jsonb_build_object() | ✅ 支持 | |
| json_build_object() | ✅ 支持 | |
| row_to_json() | ✅ 支持 | |
| jsonb_set() | ✅ 支持 | |
| jsonb_typeof() | ✅ 支持 | |
| json_extract_path_text() | ✅ 支持 | |
| **索引** | | |
| GIN 索引 | ✅ 支持 | Dify 的 JSONB 列使用 GIN 索引 |
| **SQL 特性** | | |
| CTE (WITH 子句) | ✅ 支持 | |
| 窗口函数 (row_number() OVER) | ✅ 支持 | |
| DISTINCT ON | ✅ 支持 | |
| ON CONFLICT (UPSERT) | ✅ 支持 | |
| RETURNING 子句 | ✅ 支持 | |
| FOR UPDATE 锁 | ✅ 支持 | |
| COALESCE | ✅ 支持 | |
| LIMIT/OFFSET | ✅ 支持 | |
| **聚合函数** | | |
| string_agg() | ✅ 支持 | |
| array_agg() | ✅ 支持 | |
| **数组函数** | | |
| unnest() | ✅ 支持 | |
| **正则表达式** | | |
| regexp_replace() | ✅ 支持 | |
| ~ 正则匹配操作符 | ✅ 支持 | |
| **系统表** | | |
| pg_type | ✅ 支持 | |
| pg_namespace | ✅ 支持 | |
| **函数创建** | | |
| CREATE FUNCTION (plpgsql) | ✅ 支持 | |
| generate_series() (FROM 子句) | ✅ 支持 | |

### 不支持的功能 ❌

| 功能 | 测试结果 | 影响程度 | 备注 |
|------|----------|----------|------|
| **扩展** | | | |
| hstore 扩展 | ❌ 不可用 | **严重** | 阻塞 Dify 启动 |
| pg_available_extensions 表 | ❌ 不存在 | 低 | 仅影响扩展查询 |
| **JSON 聚合函数** | | | |
| json_agg() | ❌ 不支持 | 中等 | |
| jsonb_agg() | ❌ 不支持 | 中等 | |
| **JSONB 集合返回函数** | | | |
| jsonb_each() | ❌ 不支持 | 中等 | 返回 "Table not found" |
| jsonb_array_elements() | ❌ 不支持 | 中等 | 返回 "Table not found" |
| **其他** | | | |
| LATERAL JOIN | ❌ 不支持 | 中等 | |
| generate_series() (SELECT 中) | ❌ 受限 | 低 | 作为表函数在 FROM 中可用 |
| pg_class 系统表 | ⚠️ 部分 | 低 | 查询返回 0 行 |

---

## 三、Dify 代码中使用的 PostgreSQL 特性

### 3.1 迁移脚本使用的特性

1. **uuid_generate_v4()** - 几乎所有表的主键默认值
   ```sql
   server_default=sa.text('uuid_generate_v4()')
   ```

2. **GIN 索引** - 用于 JSONB 列的索引
   ```python
   postgresql_using='gin'
   ```

3. **uuidv7 函数创建** - 自定义 SQL 函数
   ```sql
   CREATE FUNCTION uuidv7() RETURNS uuid AS $$ ... $$ LANGUAGE SQL;
   ```

### 3.2 模型层使用的特性

1. **JSONB 类型** (models/types.py)
   - 所有 JSON 字段在 PostgreSQL 下使用 JSONB

2. **UUID 类型** (models/types.py)
   - StringUUID 类型映射到 PostgreSQL UUID

3. **BYTEA 类型** (models/types.py)
   - BinaryData 类型映射到 PostgreSQL BYTEA

---

## 四、优先级修复建议

### P0 - 立即需要 (阻塞启动)

1. **hstore 支持**
   - 实现 hstore 扩展，或
   - 在 pg_type 中添加 hstore 类型条目以通过 psycopg2 的检测

### P1 - 高优先级 (核心功能)

2. **jsonb_agg() / json_agg()**
   - Dify 可能在某些聚合查询中使用

3. **jsonb_each() / jsonb_array_elements()**
   - JSONB 集合返回函数，用于解析 JSON 数组和对象

### P2 - 中优先级 (完整兼容)

4. **LATERAL JOIN**
   - 高级 SQL 查询可能使用

5. **pg_available_extensions 系统表**
   - 扩展管理功能

### P3 - 低优先级

6. **pg_class 系统表完整性**
   - 系统元数据查询

---

## 五、临时绕过方案

如果暂时无法实现 hstore 支持，可以尝试以下方案：

### 方案 1: 修改 SQLAlchemy 配置

在 Dify 代码中禁用 hstore 检测（需要修改源码）：
```python
# 在创建引擎时添加
create_engine(..., use_native_hstore=False)
```

### 方案 2: 在数据库中模拟 hstore 类型

```sql
-- 在 pg_type 中添加虚拟的 hstore 类型条目
-- 注意：这需要数据库支持修改系统表
INSERT INTO pg_type (typname, typnamespace, typowner, ...)
VALUES ('hstore', (SELECT oid FROM pg_namespace WHERE nspname = 'public'), ...);
```

---

## 六、测试命令参考

```bash
# 连接到测试数据库
docker run --rm -e PGPASSWORD=admin postgres:15-alpine psql \
  -h <host> -p <port> \
  -U "<user>" -d postgres

# 测试 hstore 检测查询
SELECT t.oid, t.typarray
FROM pg_type t
JOIN pg_namespace ns ON ns.oid = t.typnamespace
WHERE t.typname = 'hstore' AND ns.nspname = 'public';
```

---

## 七、总结

你的 PostgreSQL 兼容数据库支持 Dify 所需的大部分核心功能，包括：
- 所有基础数据类型 (UUID, JSONB, BYTEA, TEXT 等)
- UUID 生成函数
- GIN 索引
- 基础 JSONB 操作
- CTE、窗口函数、UPSERT 等高级 SQL 特性

**主要问题**: hstore 扩展不可用导致 psycopg2 连接时报错，这是当前阻塞 Dify 运行的唯一关键问题。

**次要问题**: json_agg、jsonb_each、jsonb_array_elements 等函数不可用，可能影响部分功能。

建议优先解决 hstore 问题以使 Dify 能够正常启动，然后根据实际使用情况逐步补充其他功能。
