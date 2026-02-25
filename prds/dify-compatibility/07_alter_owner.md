# PRD-D07: ALTER ... TO OWNER 支持

**阶段**: Phase 1 (P1)
**预估**: 1-2 天
**依赖**: 无

## 背景

Dify 的 schema 文件（pg_dump 导出）包含大量 `ALTER ... OWNER TO` 语句用于设置数据库对象的所有者：

```sql
ALTER TABLE public.accounts OWNER TO postgres;
ALTER FUNCTION public.uuidv7() OWNER TO postgres;
ALTER SEQUENCE public.task_id_sequence OWNER TO postgres;
```

当前 db9-server：
- **不支持** `ALTER ... OWNER TO` 语法
- 在多语句执行时会跳过这些语句（见 `helpers.rs:981`）
- 导入 schema 时产生警告信息

## 问题分析

从 `/tmp/dify_schema.sql` 统计：
- `ALTER TABLE ... OWNER TO`: 108 条
- `ALTER SEQUENCE ... OWNER TO`: 3 条
- `ALTER FUNCTION ... OWNER TO`: 2 条

这些语句目前被静默跳过，虽然不影响基本功能，但：
1. 导入 schema 时产生大量警告
2. 无法完整恢复 `pg_dump` 导出的数据库
3. 多租户场景下缺少权限管理基础

## 目标

实现 `ALTER ... OWNER TO` 语法支持，允许设置和查询数据库对象的所有者。

## 设计概览

### 存储设计

在 `TableSchema` 中添加 owner 字段：

```rust
// src/model/mod.rs

pub struct TableSchema {
    pub table_id: u64,
    pub table_name: String,
    pub columns: Vec<ColumnDef>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    pub check_constraints: Vec<CheckConstraint>,
    pub owner: Option<String>,  // 新增：所有者用户名
    // ...
}
```

对于序列和函数，也需要添加 owner 字段：

```rust
// src/model/mod.rs

pub struct Sequence {
    pub name: String,
    pub current_value: i64,
    pub increment: i64,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub owner: Option<String>,  // 新增
}

pub struct StoredProcedure {
    pub name: String,
    pub args: Vec<DataType>,
    pub return_type: Option<DataType>,
    pub body: String,
    pub owner: Option<String>,  // 新增
}
```

### 元数据存储

owner 信息存储在已有的系统键中：

```
_sys_schema_{table_name}    → TableSchema (包含 owner)
_sys_sequence_{seq_name}    → Sequence (包含 owner)
_sys_proc_{proc_name}       → StoredProcedure (包含 owner)
```

无需新增存储键，只需更新现有结构。

## 实现步骤

### Phase 1: 数据结构扩展 (2 小时)

#### 1) 更新类型定义

```rust
// src/model/mod.rs

impl TableSchema {
    pub fn new(table_name: String) -> Self {
        Self {
            owner: None,  // 默认无所有者
            // ... 其他字段
        }
    }

    pub fn set_owner(&mut self, owner: String) {
        self.owner = Some(owner);
    }

    pub fn get_owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
}
```

#### 2) 序列化兼容性

确保新增字段不破坏现有数据：

```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct TableSchema {
    // ...
    #[serde(default)]  // 旧数据反序列化时默认为 None
    pub owner: Option<String>,
}
```

### Phase 2: SQL 解析和执行 (4 小时)

#### 1) 解析 ALTER ... OWNER TO

sqlparser-rs 已支持解析 `OWNER TO`，需要在执行器中处理：

```rust
// src/sql/ddl.rs

pub async fn execute_alter_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    search_path: &[String],
    name: &ObjectName,
    operation: &AlterTableOperation,
) -> Result<ExecuteResult> {
    // ... 现有代码 ...

    match operation {
        AlterTableOperation::OwnerTo { new_owner } => {
            // 设置表的所有者
            schema.owner = Some(new_owner.to_string());
            store.save_schema(txn, &t, &schema).await?;

            Ok(ExecuteResult {
                affected_rows: 0,
                output: vec![],
                notice: Some(format!(
                    "ALTER TABLE {} OWNER TO {}",
                    t, new_owner
                )),
            })
        }
        // ... 其他操作 ...
    }
}
```

#### 2) ALTER SEQUENCE OWNER TO

```rust
// src/sql/executor.rs

Statement::AlterSequence { name, operations, .. } => {
    for op in operations {
        match op {
            AlterSequenceOperation::Owner { owner_name } => {
                let resolved = names::resolve_sequence_name(
                    &self.store,
                    txn,
                    name,
                    search_path
                ).await?;

                let mut seq = self.store.get_sequence(txn, &resolved.full).await?
                    .ok_or_else(|| anyhow!("Sequence does not exist"))?;

                seq.owner = Some(owner_name.to_string());
                self.store.save_sequence(txn, &resolved.full, &seq).await?;

                return Ok(ExecuteResult::success());
            }
            _ => { /* 其他操作 */ }
        }
    }
}
```

#### 3) ALTER FUNCTION OWNER TO

```rust
// src/sql/executor.rs

Statement::AlterFunction { name, owner, .. } => {
    if let Some(new_owner) = owner {
        let resolved = names::resolve_function_name(
            &self.store,
            txn,
            name,
            search_path
        ).await?;

        let mut proc = self.store.get_procedure(txn, &resolved.full).await?
            .ok_or_else(|| anyhow!("Function does not exist"))?;

        proc.owner = Some(new_owner.to_string());
        self.store.save_procedure(txn, &resolved.full, &proc).await?;

        return Ok(ExecuteResult::success());
    }
}
```

#### 4) 移除跳过逻辑

```rust
// src/sql/helpers.rs

pub fn should_skip_in_multi_statement(sql: &str) -> Option<String> {
    let sql_upper = sql.trim().to_uppercase();

    // 删除以下三行：
    // if sql_upper.starts_with("ALTER TABLE") && sql_upper.contains("OWNER TO") {
    //     return Some("ALTER TABLE OWNER TO not supported".into());
    // }

    // if sql_upper.starts_with("ALTER FUNCTION") {
    //     return Some("ALTER FUNCTION not supported".into());
    // }

    // if sql_upper.starts_with("ALTER SEQUENCE") {
    //     return Some("ALTER SEQUENCE not supported".into());
    // }

    // 保留其他检查...
}
```

### Phase 3: information_schema 集成 (2 小时)

更新 information_schema 虚拟表以显示所有者信息：

```rust
// src/sql/information_schema.rs

async fn query_information_schema_tables(&self, txn: &mut Transaction)
    -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for (table_name, schema) in self.store.list_all_schemas(txn).await? {
        rows.push(Row::new(vec![
            Value::Text("public".into()),          // table_schema
            Value::Text(table_name),               // table_name
            Value::Text("BASE TABLE".into()),      // table_type
            Value::from_option(schema.owner.map(Value::Text)), // owner
        ]));
    }

    Ok(rows)
}
```

同样更新 `information_schema.sequences` 和其他元数据表。

## 测试

### 基础功能测试

```sql
-- tests/95_alter_owner.sql

-- 创建测试表
CREATE TABLE owner_test (id INT PRIMARY KEY);

-- 设置所有者
ALTER TABLE owner_test OWNER TO alice;

-- 查询所有者（通过 information_schema）
SELECT table_name, table_owner
FROM information_schema.tables
WHERE table_name = 'owner_test';
-- Expected: owner_test | alice

-- 修改所有者
ALTER TABLE owner_test OWNER TO bob;

-- 验证修改
SELECT table_name, table_owner
FROM information_schema.tables
WHERE table_name = 'owner_test';
-- Expected: owner_test | bob
```

### 序列所有者测试

```sql
-- 创建序列
CREATE SEQUENCE owner_seq;

-- 设置所有者
ALTER SEQUENCE owner_seq OWNER TO alice;

-- 查询（通过 information_schema.sequences）
SELECT sequence_name, sequence_owner
FROM information_schema.sequences
WHERE sequence_name = 'owner_seq';
-- Expected: owner_seq | alice
```

### 函数所有者测试

```sql
-- 创建函数
CREATE FUNCTION test_func() RETURNS INT AS $$
    SELECT 1;
$$ LANGUAGE SQL;

-- 设置所有者
ALTER FUNCTION test_func() OWNER TO alice;

-- 查询（通过 information_schema.routines）
SELECT routine_name, routine_owner
FROM information_schema.routines
WHERE routine_name = 'test_func';
-- Expected: test_func | alice
```

### Dify Schema 导入测试

```bash
# 导入完整 Dify schema（应该不再有 OWNER TO 警告）
psql -h 127.0.0.1 -p 5433 < /tmp/dify_schema.sql

# 验证导入成功
psql -h 127.0.0.1 -p 5433 -c "\dt"
psql -h 127.0.0.1 -p 5433 -c "SELECT COUNT(*) FROM information_schema.tables WHERE table_owner = 'postgres';"
-- Expected: 108
```

### 兼容性测试

```sql
-- 不存在的对象
ALTER TABLE nonexistent OWNER TO alice;
-- Expected: ERROR: Table 'nonexistent' does not exist

-- 多次修改所有者
ALTER TABLE owner_test OWNER TO alice;
ALTER TABLE owner_test OWNER TO bob;
ALTER TABLE owner_test OWNER TO charlie;
-- Expected: 所有者为 charlie

-- 空所有者（PostgreSQL 不允许，但测试容错）
-- 这应该被拒绝或设置为当前用户
```

## EXPLAIN 输出（不适用）

此功能不影响查询执行计划。

## 限制

### MVP 限制

1. **仅存储所有者信息**：不实际执行权限检查
   - ALTER 操作不验证当前用户是否有权限
   - 所有用户可以修改任何对象的所有者
   - 查询不检查用户是否有权访问对象

2. **不支持 ROLE**：仅支持简单用户名
   - 不支持 `OWNER TO CURRENT_USER`
   - 不支持 `OWNER TO SESSION_USER`
   - 不验证所有者是否为有效用户

3. **部分对象支持**：
   - ✅ TABLE
   - ✅ SEQUENCE
   - ✅ FUNCTION
   - ❌ VIEW (待实现)
   - ❌ MATERIALIZED VIEW (待实现)
   - ❌ INDEX (PostgreSQL 索引继承表的所有者)

### 后续增强

1. **Phase 2: 权限系统**
   - 实现基于所有者的访问控制
   - 添加 `pg_authid`, `pg_roles` 系统表
   - 实现 `GRANT`/`REVOKE` 权限管理

2. **Phase 3: 高级功能**
   - 支持 `REASSIGN OWNED BY`
   - 支持 `DROP OWNED BY`
   - 所有者级联更新（表删除时清理）

## 验收标准

1. **基本功能**：
   ```sql
   ALTER TABLE my_table OWNER TO alice;
   ALTER SEQUENCE my_seq OWNER TO bob;
   ALTER FUNCTION my_func() OWNER TO charlie;
   -- Expected: 成功执行，无错误
   ```

2. **信息查询**：
   ```sql
   SELECT table_name, table_owner FROM information_schema.tables;
   -- Expected: 显示所有表及其所有者
   ```

3. **Schema 导入**：
   ```bash
   psql < /tmp/dify_schema.sql
   # Expected: 无 "ALTER ... OWNER TO not supported" 警告
   ```

4. **持久化**：
   ```sql
   ALTER TABLE test OWNER TO alice;
   -- 重启服务器
   SELECT table_owner FROM information_schema.tables WHERE table_name = 'test';
   -- Expected: alice
   ```

## 风险

| 风险 | 影响 | 缓解 |
|------|------|------|
| 序列化版本不兼容 | 无法读取旧数据 | 使用 `#[serde(default)]` |
| 权限误解 | 用户误以为有权限控制 | 文档说明当前为元数据存储 |
| 大小写敏感 | owner 名称不一致 | 规范化处理（lowercase） |

## 实现检查清单

- [ ] 更新 `TableSchema`, `Sequence`, `StoredProcedure` 结构
- [ ] 实现 `ALTER TABLE ... OWNER TO`
- [ ] 实现 `ALTER SEQUENCE ... OWNER TO`
- [ ] 实现 `ALTER FUNCTION ... OWNER TO`
- [ ] 移除 `helpers.rs` 中的跳过逻辑
- [ ] 更新 `information_schema.tables`
- [ ] 更新 `information_schema.sequences`
- [ ] 更新 `information_schema.routines`
- [ ] 添加测试文件 `tests/95_alter_owner.sql`
- [ ] 添加测试文件 `tests/95_alter_owner.expected`
- [ ] 验证 Dify schema 导入
- [ ] 更新文档说明限制
