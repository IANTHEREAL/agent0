# 设计：CREATE DATABASE 支持

**Status**: Draft  
**Priority**: P1  
**Author**: AI Assistant  
**Date**: 2026-01-23

> **Draft / non-SoT note**
>
> This document is a design draft, not a current-behavior contract.
> Validate current behavior against `docs/sot/**`, `docs/ARCHITECTURE.md`, and the implementation under `src/**` before using it for product or compatibility decisions.

## 背景与动机

### 当前状态

db9-server 目前将 TiKV keyspace 等同于 PostgreSQL 的 database 概念，导致：

1. **不支持 `CREATE DATABASE`** - 在 `helpers.rs` 中显式跳过
2. **每个 keyspace 只有一个逻辑数据库** - 与 PostgreSQL 语义不兼容
3. **连接时的 database 参数被忽略** - 始终返回 "postgres"

### 正确的层级关系

```
PostgreSQL:                          db9-server (当前):
┌─────────────────────┐              ┌─────────────────────┐
│  Cluster            │              │  Keyspace (tenant)  │
│  └── Database 1     │              │  └── Schema         │
│  │     └── Schema   │      ≠       │        └── Tables   │
│  │           └── T  │              └─────────────────────┘
│  └── Database 2     │
│        └── Schema   │
│              └── T  │
└─────────────────────┘

db9-server (目标):
┌─────────────────────────────────┐
│  Keyspace (tenant)              │
│  └── Database 1 (e.g. postgres) │
│  │     └── Schema (public, etc.)│
│  │           └── Tables         │
│  └── Database 2 (e.g. myapp)    │
│        └── Schema               │
│              └── Tables         │
└─────────────────────────────────┘
```

### 用户需求

- ORM 工具（如 Prisma、TypeORM）可能需要创建独立数据库
- `pg_dump` / `pg_restore` 使用 database 级别的备份恢复
- 多应用共享同一租户但需要数据库级别隔离
- 高效的 `DROP DATABASE` 和 `RENAME DATABASE` 操作

## 目标（MVP）

### 支持的 SQL 语句

```sql
-- 创建数据库
CREATE DATABASE dbname;
CREATE DATABASE dbname WITH OWNER = username;
CREATE DATABASE IF NOT EXISTS dbname;

-- 删除数据库（高效，使用 DeleteRange）
DROP DATABASE dbname;
DROP DATABASE IF EXISTS dbname;

-- 重命名数据库（高效，O(1) 操作）
ALTER DATABASE oldname RENAME TO newname;

-- 查询数据库
SELECT datname FROM pg_database;
\l  -- psql 命令

-- 切换数据库（通过重新连接）
-- psql: \c dbname
-- 连接字符串: postgres://user:pass@host:port/dbname
```

### 默认行为

- 每个 keyspace 自动创建 `postgres` 数据库（与 PostgreSQL 一致）
- 连接时不指定数据库，默认连接到 `postgres`
- `current_database()` 返回当前连接的数据库名

### 非目标（MVP 不做）

- `CREATE DATABASE ... TEMPLATE = xxx`（模板数据库）
- `ALTER DATABASE ... SET ...`（数据库级别配置）
- `CREATE DATABASE ... TABLESPACE = xxx`
- 跨数据库查询（PostgreSQL 也不支持）

## 设计详情

### 1. 数据模型

#### 1.1 DatabaseDef 结构

```rust
// src/model/mod.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseDef {
    /// Database name (e.g., "postgres", "myapp")
    pub name: String,
    
    /// Database OID (for pg_catalog compatibility)
    pub oid: u32,
    
    /// Owner username
    pub owner: String,
    
    /// Encoding (always UTF8)
    pub encoding: String,
    
    /// Creation timestamp
    pub created_at: i64,
    
    /// Is this the template database? (template0, template1)
    pub is_template: bool,
    
    /// Allow connections?
    pub allow_conn: bool,
}

impl DatabaseDef {
    pub fn new(name: String, owner: String, oid: u32) -> Self {
        Self {
            name,
            oid,
            owner,
            encoding: "UTF8".to_string(),
            created_at: chrono::Utc::now().timestamp_millis(),
            is_template: false,
            allow_conn: true,
        }
    }
    
    /// Create the default "postgres" database
    pub fn default_postgres(owner: String, oid: u32) -> Self {
        Self::new("postgres".to_string(), owner, oid)
    }
}
```

### 2. 存储层变更

#### 2.1 核心设计决策：使用 Database ID 而非 Name

**问题：如果使用 database name 作为 key 前缀**
```
# 如果用 name 做前缀：
d_mydb_sys_schema_public.users  → TableSchema
d_mydb_t_1_<pk>                 → Row

# RENAME DATABASE mydb TO newdb 需要：
# 1. 扫描所有以 d_mydb_ 开头的 key
# 2. 逐个删除旧 key，写入新 key
# 3. 复杂度：O(N)，N = 数据库内所有 key 数量
# 4. 大数据库可能需要数小时！
```

**解决方案：使用 database_id (u64) 作为 key 前缀**
```
# 用 ID 做前缀（8 字节二进制）：
d_<8bytes:1>_sys_schema_public.users  → TableSchema
d_<8bytes:1>_t_<8bytes:1>_<pk>        → Row

# RENAME DATABASE：只更新 DatabaseDef 中的 name 字段
# 复杂度：O(1)

# DROP DATABASE：使用 TiKV DeleteRange
# 复杂度：O(1) 或 O(log N)，由 TiKV 异步 GC 处理
```

#### 2.2 新增 Key 前缀

```rust
// src/storage/encoding.rs

// 数据库元数据 (keyspace 级别)
const SYS_DATABASE_BY_NAME_PREFIX: &[u8] = b"_sys_dbname_";  // name -> id 映射
const SYS_DATABASE_BY_ID_PREFIX: &[u8] = b"_sys_dbid_";      // id -> DatabaseDef
const SYS_NEXT_DATABASE_ID: &[u8] = b"_sys_next_database_id";

// Database 内数据前缀 (使用 8 字节 database_id)
// 格式: d_{db_id:8bytes}_
const DATABASE_DATA_PREFIX: &[u8] = b"d_";

/// Encode database name -> id mapping key
pub fn encode_database_name_key(db_name: &str) -> Vec<u8> {
    let mut key = SYS_DATABASE_BY_NAME_PREFIX.to_vec();
    key.extend_from_slice(db_name.to_lowercase().as_bytes());
    key
}

/// Encode database id -> def key
pub fn encode_database_id_key(db_id: u64) -> Vec<u8> {
    let mut key = SYS_DATABASE_BY_ID_PREFIX.to_vec();
    key.extend_from_slice(&db_id.to_be_bytes());
    key
}

/// Encode the prefix for all data within a database
/// Format: d_{db_id:8bytes}_
pub fn encode_database_data_prefix(db_id: u64) -> Vec<u8> {
    let mut key = DATABASE_DATA_PREFIX.to_vec();
    key.extend_from_slice(&db_id.to_be_bytes());
    key.push(b'_');
    key
}
```

#### 2.3 Key 布局变更

**当前布局：**
```
_sys_next_table_id              → u64
_sys_schema_{schema.table}      → TableSchema
_sys_schemadef_{schema}         → schema OID
_sys_view_{schema.view}         → ViewDef
_sys_seqdef_{schema.seq}        → SequenceDef
t_{table_id}_{pk}               → Row
i_{table_id}_{idx}_{vals}       → Index entry
```

**新布局（使用 database_id，`_` 分隔符）：**
```
# === Keyspace 级别元数据（跨数据库共享）===

# 数据库名称 -> ID 映射（支持快速查找）
_sys_dbname_{db_name}           → u64 (database_id)

# 数据库 ID -> 定义（支持遍历所有数据库）
_sys_dbid_{db_id:8bytes}        → DatabaseDef

# 下一个数据库 ID
_sys_next_database_id           → u64

# 用户/角色（跨数据库共享）
_sys_user_{username}            → UserDef
_sys_role_{rolename}            → RoleDef


# === Database 内数据（按 database_id 分区）===
# 前缀: d_{db_id:8bytes}_

# 元数据
d_{db_id}_sys_next_table_id             → u64
d_{db_id}_sys_schema_{schema.table}     → TableSchema
d_{db_id}_sys_schemadef_{schema}        → schema OID
d_{db_id}_sys_view_{schema.view}        → ViewDef
d_{db_id}_sys_matview_{schema.matview}  → MatViewDef
d_{db_id}_sys_seqdef_{schema.seq}       → SequenceDef
d_{db_id}_sys_type_{schema.type}        → TypeDef
d_{db_id}_sys_proc_{schema.proc}        → ProcedureDef
d_{db_id}_sys_func_{schema.func}        → FunctionDef
d_{db_id}_sys_trigger_{table}_{name}    → TriggerDef
d_{db_id}_sys_ext_{ext_name}            → ExtensionDef
d_{db_id}_sys_comment_{kind}...         → Comment

# 表数据和索引
d_{db_id}_t_{table_id}_{pk}             → Row
d_{db_id}_i_{table_id}_{idx}_{vals}     → Index entry

# 示例 (db_id=1, table_id=5, pk=42):
# d_\x00\x00\x00\x00\x00\x00\x00\x01_t_\x00\x00\x00\x00\x00\x00\x00\x05_<pk_bytes>
```

**为什么这个设计高效：**

| 操作 | 复杂度 | 说明 |
|------|--------|------|
| CREATE DATABASE | O(1) | 分配 ID，写入 2 个元数据 key |
| DROP DATABASE | O(1)* | DeleteRange 删除 `d_{db_id}_` 前缀，TiKV 异步 GC |
| RENAME DATABASE | O(1) | 删除旧 name key，写入新 name key，更新 DatabaseDef |
| 连接验证 | O(1) | 按 name 查找 ID |
| 查询表 | O(1) | 已知 db_id，直接构造 key |

*注：TiKV DeleteRange 是一个标记操作，实际数据由后台 GC 清理

#### 2.4 编码函数

```rust
// src/storage/encoding.rs

/// Encode schema key within a database
/// Format: d_{db_id}_sys_schema_{table_name}
pub fn encode_schema_key_v2(db_id: u64, table_name: &str) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(b"sys_schema_");
    key.extend_from_slice(table_name.as_bytes());
    key
}

/// Encode data key within a database
/// Format: d_{db_id}_t_{table_id}_{row_key}
pub fn encode_data_key_v2(db_id: u64, table_id: u64, row_key: &[u8]) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(b"t_");
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(row_key);
    key
}

/// Encode index key within a database
/// Format: d_{db_id}_i_{table_id}_{index_id}_{values}[_{pk}]
pub fn encode_index_key_v2(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    values: &[Value],
    pk: Option<&[Value]>,
) -> Vec<u8> {
    let mut key = encode_database_data_prefix(db_id);
    key.extend_from_slice(b"i_");
    key.extend_from_slice(&table_id.to_be_bytes());
    key.push(b'_');
    key.extend_from_slice(&index_id.to_be_bytes());
    key.push(b'_');
    // ... rest same as current encode_index_key
    key
}

/// Get the key range for all data within a database (for DROP DATABASE)
/// Range: [d_{db_id}_, d_{db_id+1}_)
pub fn encode_database_data_range(db_id: u64) -> (Vec<u8>, Vec<u8>) {
    let start = encode_database_data_prefix(db_id);
    let end = encode_database_data_prefix(db_id + 1);
    (start, end)
}

/// Get the key range for scanning all rows of a table
/// Range: [d_{db_id}_t_{table_id}_, d_{db_id}_t_{table_id+1}_)
pub fn encode_table_data_range_v2(db_id: u64, table_id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut start = encode_database_data_prefix(db_id);
    start.extend_from_slice(b"t_");
    start.extend_from_slice(&table_id.to_be_bytes());
    start.push(b'_');
    
    let mut end = encode_database_data_prefix(db_id);
    end.extend_from_slice(b"t_");
    end.extend_from_slice(&(table_id + 1).to_be_bytes());
    
    (start, end)
}

/// Get the key range for all indexes of a table (for DROP TABLE / TRUNCATE)
/// Range: [d_{db_id}_i_{table_id}_, d_{db_id}_i_{table_id+1}_)
pub fn encode_table_index_range_v2(db_id: u64, table_id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut start = encode_database_data_prefix(db_id);
    start.extend_from_slice(b"i_");
    start.extend_from_slice(&table_id.to_be_bytes());
    start.push(b'_');
    
    let mut end = encode_database_data_prefix(db_id);
    end.extend_from_slice(b"i_");
    end.extend_from_slice(&(table_id + 1).to_be_bytes());
    
    (start, end)
}
```

### 3. TikvStore 变更

#### 3.1 DatabaseDef 结构更新

```rust
// src/model/mod.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseDef {
    /// Database ID (used as key prefix for all data within this database)
    pub id: u64,
    
    /// Database name (e.g., "postgres", "myapp")
    pub name: String,
    
    /// Database OID (for pg_catalog compatibility, can be same as id)
    pub oid: u32,
    
    /// Owner username
    pub owner: String,
    
    /// Encoding (always UTF8)
    pub encoding: String,
    
    /// Creation timestamp
    pub created_at: i64,
    
    /// Is this the template database?
    pub is_template: bool,
    
    /// Allow connections?
    pub allow_conn: bool,
}
```

#### 3.2 数据库操作方法

```rust
// src/storage/tikv_store.rs

impl TikvStore {
    /// Create a new database
    /// 
    /// Creates two mappings:
    /// - name -> id (for lookups by name)
    /// - id -> DatabaseDef (for metadata storage)
    pub async fn create_database(
        &self,
        txn: &mut Transaction,
        name: &str,
        owner: &str,
        if_not_exists: bool,
    ) -> Result<Option<DatabaseDef>> {
        let name_lower = name.to_lowercase();
        
        // Check if database already exists
        let name_key = self.key(&encode_database_name_key(&name_lower));
        if txn.get(name_key.clone()).await?.is_some() {
            if if_not_exists {
                return Ok(None);
            }
            return Err(anyhow!("database \"{}\" already exists", name));
        }
        
        // Allocate new database ID
        let db_id = self.next_database_id(txn).await?;
        
        let def = DatabaseDef {
            id: db_id,
            name: name_lower.clone(),
            oid: db_id as u32,
            owner: owner.to_string(),
            encoding: "UTF8".to_string(),
            created_at: chrono::Utc::now().timestamp_millis(),
            is_template: false,
            allow_conn: true,
        };
        
        // Write name -> id mapping
        txn_put(txn, name_key, db_id.to_be_bytes().to_vec()).await?;
        
        // Write id -> DatabaseDef
        let id_key = self.key(&encode_database_id_key(db_id));
        let data = bincode::serialize(&def)?;
        txn_put(txn, id_key, data).await?;
        
        info!("Created database '{}' with ID {}", name, db_id);
        Ok(Some(def))
    }
    
    /// Drop a database efficiently using DeleteRange
    /// 
    /// Steps:
    /// 1. Look up database ID by name
    /// 2. Delete name -> id mapping
    /// 3. Delete id -> DatabaseDef mapping  
    /// 4. Use DeleteRange to remove all data with prefix d_{db_id}_
    pub async fn drop_database(
        &self,
        txn: &mut Transaction,
        db_name: &str,
        if_exists: bool,
        current_database: &str,
    ) -> Result<bool> {
        let name_lower = db_name.to_lowercase();
        
        // Cannot drop current database
        if name_lower == current_database.to_lowercase() {
            return Err(anyhow!("cannot drop the currently open database"));
        }
        
        // Cannot drop postgres (reserved)
        if name_lower == "postgres" {
            return Err(anyhow!("cannot drop database \"postgres\": it is a system database"));
        }
        
        // Look up database ID
        let name_key = self.key(&encode_database_name_key(&name_lower));
        let db_id = match txn.get(name_key.clone()).await? {
            Some(data) => u64::from_be_bytes(data.try_into().map_err(|_| anyhow!("Invalid database ID"))?),
            None => {
                if if_exists {
                    return Ok(false);
                }
                return Err(anyhow!("database \"{}\" does not exist", db_name));
            }
        };
        
        // Delete name -> id mapping
        txn_delete(txn, name_key).await?;
        
        // Delete id -> DatabaseDef mapping
        let id_key = self.key(&encode_database_id_key(db_id));
        txn_delete(txn, id_key).await?;
        
        // Use DeleteRange to efficiently delete all data within the database
        // This is O(1) - TiKV marks the range for deletion, actual cleanup is async
        let (start, end) = encode_database_data_range(db_id);
        let start_key = self.key(&start);
        let end_key = self.key(&end);
        
        // Note: tikv-client-rust may not expose delete_range directly in Transaction
        // Alternative: use batch_delete with scan, or call RawClient delete_range
        // For MVP, we can scan and delete (less efficient but works)
        self.delete_range_within_txn(txn, start_key, end_key).await?;
        
        info!("Dropped database '{}' (ID {})", db_name, db_id);
        Ok(true)
    }
    
    /// Rename a database - O(1) operation
    /// 
    /// Steps:
    /// 1. Verify old name exists, new name doesn't
    /// 2. Delete old name -> id mapping
    /// 3. Create new name -> id mapping
    /// 4. Update DatabaseDef with new name
    pub async fn rename_database(
        &self,
        txn: &mut Transaction,
        old_name: &str,
        new_name: &str,
        current_database: &str,
    ) -> Result<()> {
        let old_lower = old_name.to_lowercase();
        let new_lower = new_name.to_lowercase();
        
        // Cannot rename current database
        if old_lower == current_database.to_lowercase() {
            return Err(anyhow!("cannot rename the currently open database"));
        }
        
        // Cannot rename postgres
        if old_lower == "postgres" {
            return Err(anyhow!("cannot rename database \"postgres\""));
        }
        
        // Cannot rename to postgres
        if new_lower == "postgres" {
            return Err(anyhow!("cannot rename to \"postgres\""));
        }
        
        // Check old database exists
        let old_name_key = self.key(&encode_database_name_key(&old_lower));
        let db_id = match txn.get(old_name_key.clone()).await? {
            Some(data) => u64::from_be_bytes(data.try_into().map_err(|_| anyhow!("Invalid database ID"))?),
            None => return Err(anyhow!("database \"{}\" does not exist", old_name)),
        };
        
        // Check new name doesn't exist
        let new_name_key = self.key(&encode_database_name_key(&new_lower));
        if txn.get(new_name_key.clone()).await?.is_some() {
            return Err(anyhow!("database \"{}\" already exists", new_name));
        }
        
        // Delete old name mapping
        txn_delete(txn, old_name_key).await?;
        
        // Create new name mapping
        txn_put(txn, new_name_key, db_id.to_be_bytes().to_vec()).await?;
        
        // Update DatabaseDef
        let id_key = self.key(&encode_database_id_key(db_id));
        let mut def: DatabaseDef = match txn.get(id_key.clone()).await? {
            Some(data) => bincode::deserialize(&data)?,
            None => return Err(anyhow!("database metadata corrupted")),
        };
        def.name = new_lower.clone();
        let data = bincode::serialize(&def)?;
        txn_put(txn, id_key, data).await?;
        
        info!("Renamed database '{}' to '{}' (ID {})", old_name, new_name, db_id);
        Ok(())
    }
    
    /// Get database ID by name (for connection validation)
    pub async fn get_database_id(
        &self,
        txn: &mut Transaction,
        db_name: &str,
    ) -> Result<Option<u64>> {
        let name_key = self.key(&encode_database_name_key(&db_name.to_lowercase()));
        match txn.get(name_key).await? {
            Some(data) => Ok(Some(u64::from_be_bytes(data.try_into().map_err(|_| anyhow!("Invalid database ID"))?))),
            None => Ok(None),
        }
    }
    
    /// Get database definition by ID
    pub async fn get_database_by_id(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<DatabaseDef>> {
        let id_key = self.key(&encode_database_id_key(db_id));
        match txn.get(id_key).await? {
            Some(data) => Ok(Some(bincode::deserialize(&data)?)),
            None => Ok(None),
        }
    }
    
    /// Get database definition by name
    pub async fn get_database(
        &self,
        txn: &mut Transaction,
        db_name: &str,
    ) -> Result<Option<DatabaseDef>> {
        let db_id = match self.get_database_id(txn, db_name).await? {
            Some(id) => id,
            None => return Ok(None),
        };
        self.get_database_by_id(txn, db_id).await
    }
    
    /// List all databases (by scanning id -> def mappings)
    pub async fn list_databases(&self, txn: &mut Transaction) -> Result<Vec<DatabaseDef>> {
        let prefix = self.key(&SYS_DATABASE_BY_ID_PREFIX.to_vec());
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        
        let mut databases = Vec::new();
        for pair in pairs {
            let def: DatabaseDef = bincode::deserialize(pair.value())?;
            databases.push(def);
        }
        Ok(databases)
    }
    
    /// Bootstrap: create default "postgres" database if not exists
    pub async fn bootstrap_database(&self, txn: &mut Transaction, owner: &str) -> Result<()> {
        // Check if postgres database already exists
        if self.get_database_id(txn, "postgres").await?.is_some() {
            return Ok(());
        }
        
        // Create postgres database with ID 1 (reserved)
        let db_id = self.next_database_id(txn).await?;
        let def = DatabaseDef {
            id: db_id,
            name: "postgres".to_string(),
            oid: db_id as u32,
            owner: owner.to_string(),
            encoding: "UTF8".to_string(),
            created_at: chrono::Utc::now().timestamp_millis(),
            is_template: false,
            allow_conn: true,
        };
        
        // Write name -> id mapping
        let name_key = self.key(&encode_database_name_key("postgres"));
        txn_put(txn, name_key, db_id.to_be_bytes().to_vec()).await?;
        
        // Write id -> DatabaseDef
        let id_key = self.key(&encode_database_id_key(db_id));
        let data = bincode::serialize(&def)?;
        txn_put(txn, id_key, data).await?;
        
        info!("Bootstrapped default database 'postgres' with ID {}", db_id);
        Ok(())
    }
    
    /// Get next database ID
    pub async fn next_database_id(&self, txn: &mut Transaction) -> Result<u64> {
        const FIRST_DATABASE_ID: u64 = 1;
        
        let key = self.key(&encode_next_database_id_key());
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let id = u64::from_be_bytes(data.try_into().map_err(|_| anyhow!("Invalid ID"))?);
                id.checked_add(1).ok_or_else(|| anyhow!("Database ID overflow"))?
            }
            None => FIRST_DATABASE_ID,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }
}
```

#### 3.3 高效 DeleteRange 实现（使用 RawClient）

TiKV 的 `RawClient.delete_range()` 是**异步非阻塞**的：
- 调用立即返回（O(1) 时间复杂度）
- TiKV 标记范围为待删除
- 实际数据由 TiKV GC 后台异步清理
- 用户无需等待数据实际删除完成

**推荐实现：使用 RawClient**

```rust
impl TikvStore {
    /// Efficient delete range using RawKV API
    /// 
    /// This is NON-BLOCKING:
    /// 1. The delete_range call returns immediately after marking the range
    /// 2. Actual data cleanup happens asynchronously via TiKV GC
    /// 3. Operation completes in O(1) time regardless of data size
    pub async fn delete_range_async(&self, start: Vec<u8>, end: Vec<u8>) -> Result<()> {
        self.raw_client.delete_range(start..end).await?;
        Ok(())
    }
    
    /// Delete all data within a database (for DROP DATABASE)
    pub async fn delete_database_data(&self, db_id: u64) -> Result<()> {
        let (start, end) = encode_database_data_range(db_id);
        let start_key = self.key(&start);
        let end_key = self.key(&end);
        self.delete_range_async(start_key, end_key).await
    }
    
    /// Delete all data and indexes for a table (for DROP TABLE)
    pub async fn delete_table_data(&self, db_id: u64, table_id: u64) -> Result<()> {
        // Delete table rows: d_{db_id}_t_{table_id}_*
        let (data_start, data_end) = encode_table_data_range_v2(db_id, table_id);
        self.delete_range_async(self.key(&data_start), self.key(&data_end)).await?;
        
        // Delete table indexes: d_{db_id}_i_{table_id}_*
        let (idx_start, idx_end) = encode_table_index_range_v2(db_id, table_id);
        self.delete_range_async(self.key(&idx_start), self.key(&idx_end)).await?;
        
        Ok(())
    }
}
```

**MVP 备选：事务内扫描删除（较慢但无需 RawClient）**

```rust
impl TikvStore {
    /// Fallback: Delete range within transaction (slower, O(N))
    /// Use only if RawClient is not available
    async fn delete_range_within_txn(
        &self,
        txn: &mut Transaction,
        start: Vec<u8>,
        end: Vec<u8>,
    ) -> Result<()> {
        const BATCH_SIZE: u32 = 1000;
        
        loop {
            let range: BoundRange = (start.clone()..end.clone()).into();
            let pairs: Vec<_> = txn.scan(range, BATCH_SIZE).await?.collect();
            
            if pairs.is_empty() {
                break;
            }
            
            for pair in &pairs {
                let key: Vec<u8> = pair.key().clone().into();
                txn_delete(txn, key).await?;
            }
            
            if pairs.len() < BATCH_SIZE as usize {
                break;
            }
        }
        
        Ok(())
    }
}
```

#### 3.4 TikvStore 结构（双客户端架构）

为支持高效的范围删除，TikvStore 需要同时维护 TransactionClient 和 RawClient：

```rust
// src/storage/tikv_store.rs

use tikv_client::{TransactionClient, RawClient, Config};

pub struct TikvStore {
    /// Transaction client for normal CRUD operations
    txn_client: TransactionClient,
    
    /// Raw client for efficient range operations (DROP DATABASE/TABLE/TRUNCATE)
    raw_client: RawClient,
    
    /// Keyspace prefix for multi-tenancy
    keyspace_prefix: Vec<u8>,
}

impl TikvStore {
    pub async fn new(pd_endpoints: Vec<String>, keyspace: Option<String>) -> Result<Self> {
        let config = Config::default();
        let config = if let Some(ks) = &keyspace {
            config.with_keyspace(ks)
        } else {
            config
        };
        
        // Create both clients with same config (including keyspace)
        let txn_client = TransactionClient::new_with_config(
            pd_endpoints.clone(), 
            config.clone()
        ).await?;
        
        let raw_client = RawClient::new_with_config(
            pd_endpoints, 
            config
        ).await?;
        
        Ok(Self {
            txn_client,
            raw_client,
            keyspace_prefix: vec![],
        })
    }
    
    /// Begin a new transaction
    pub async fn begin(&self) -> Result<Transaction> {
        self.txn_client.begin_pessimistic().await
    }
    
    /// Get raw client reference for efficient range operations
    pub fn raw_client(&self) -> &RawClient {
        &self.raw_client
    }
}
```

#### 3.5 DROP TABLE 优化

利用 RawClient 的 delete_range 使 DROP TABLE 变为 O(1) 操作：

```rust
impl TikvStore {
    /// Drop a table efficiently using RawClient delete_range
    /// 
    /// Steps:
    /// 1. Delete table data: d_{db_id}_t_{table_id}_*
    /// 2. Delete table indexes: d_{db_id}_i_{table_id}_*
    /// 3. Delete table schema (single key, use transaction)
    /// 
    /// Time complexity: O(1) - actual cleanup is async via GC
    pub async fn drop_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        table_id: u64,
    ) -> Result<()> {
        // 1. Delete table data and indexes using RawClient (non-blocking)
        self.delete_table_data(db_id, table_id).await?;
        
        // 2. Delete schema metadata (single key, use transaction)
        let schema_key = self.key(&encode_schema_key_v2(db_id, table_name));
        txn_delete(txn, schema_key).await?;
        
        info!("Dropped table '{}' (ID {}) in database {}", table_name, table_id, db_id);
        Ok(())
    }
}
```

**当前实现 vs 优化后对比：**

```rust
// 当前实现（慢）- O(N) 其中 N = 行数
async fn drop_table_slow(&self, txn: &mut Transaction, table_id: u64) -> Result<()> {
    // 扫描所有行
    let rows = txn.scan(table_prefix..table_end, LIMIT).await?;
    // 逐个删除
    for row in rows {
        txn.delete(row.key()).await?;
    }
    // 还要扫描删除索引...
    Ok(())
}

// 优化后（快）- O(1)
async fn drop_table_fast(&self, db_id: u64, table_id: u64) -> Result<()> {
    // 直接标记范围删除，立即返回
    self.delete_table_data(db_id, table_id).await?;
    Ok(())
}
```

#### 3.6 TRUNCATE TABLE 优化

TRUNCATE TABLE 与 DROP TABLE 类似，但保留表结构：

```rust
impl TikvStore {
    /// Truncate a table efficiently using RawClient delete_range
    /// 
    /// Deletes all data and indexes but keeps the schema.
    /// Time complexity: O(1) - actual cleanup is async via GC
    pub async fn truncate_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        schema: &TableSchema,
    ) -> Result<()> {
        // 1. Delete all table data and indexes (non-blocking)
        self.delete_table_data(db_id, table_id).await?;
        
        // 2. Reset sequences for SERIAL columns (if any)
        for col in &schema.columns {
            if col.is_serial() {
                if let Some(seq_name) = col.sequence_name() {
                    self.reset_sequence(txn, db_id, seq_name).await?;
                }
            }
        }
        
        info!("Truncated table ID {} in database {}", table_id, db_id);
        Ok(())
    }
    
    /// Reset a sequence to its start value
    async fn reset_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        seq_name: &str,
    ) -> Result<()> {
        let seq_key = self.key(&encode_sequence_key_v2(db_id, seq_name));
        // Get sequence definition to find start value
        let def_key = self.key(&encode_sequence_def_key_v2(db_id, seq_name));
        if let Some(data) = txn.get(def_key).await? {
            let def: SequenceDef = bincode::deserialize(&data)?;
            txn_put(txn, seq_key, def.start.to_be_bytes().to_vec()).await?;
        }
        Ok(())
    }
}

#### 3.4 现有方法增加 db_id 参数

所有表/视图/序列等操作需要增加 `db_id: u64` 参数：

```rust
// 示例：get_schema 变更
// Before:
pub async fn get_schema(&self, txn: &mut Transaction, table_name: &str) -> Result<Option<TableSchema>>

// After:
pub async fn get_schema(&self, txn: &mut Transaction, db_id: u64, table_name: &str) -> Result<Option<TableSchema>> {
    let key = self.key(&encode_schema_key_v2(db_id, table_name));
    // ... rest same
}
```

### 4. Session 变更

```rust
// src/sql/session.rs

pub struct Session {
    // ... existing fields ...
    
    /// Current database ID (used for key construction)
    current_database_id: u64,
    
    /// Current database name (for display/queries)
    current_database_name: String,
}

impl Session {
    pub fn new_with_database(
        store: Arc<TikvStore>,
        tenant_obs: TenantObservability,
        connection_id: i32,
        database_id: u64,
        database_name: String,
    ) -> Self {
        Self {
            store,
            tenant_obs,
            connection_id,
            current_database_id: database_id,
            current_database_name: database_name,
            // ... other fields ...
        }
    }
    
    /// Get current database ID (for key construction)
    pub fn current_database_id(&self) -> u64 {
        self.current_database_id
    }
    
    /// Get current database name (for display)
    pub fn current_database(&self) -> &str {
        &self.current_database_name
    }
}
```

### 5. Protocol Handler 变更

#### 5.1 Startup 消息处理

```rust
// src/protocol/handler.rs

/// Custom metadata key for database name
const METADATA_DATABASE: &str = "database";

impl StartupHandler for DynamicPgHandler {
    async fn on_startup<C>(&self, client: &mut C, message: PgWireFrontendMessage) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        // ...
    {
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                pgwire::api::auth::save_startup_parameters_to_metadata(client, startup);
                
                // Extract database from startup parameters
                let database = client
                    .metadata()
                    .get("database")
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());
                
                client
                    .metadata_mut()
                    .insert(METADATA_DATABASE.to_string(), database);
                
                // ... existing keyspace/user handling ...
            }
            PgWireFrontendMessage::PasswordMessageFamily(pwd) => {
                // ... authentication ...
                
                let database = client
                    .metadata()
                    .get(METADATA_DATABASE)
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());
                
                // Verify database exists
                // Pass database to init_executor
                self.init_executor(keyspace, Some(actual_user), is_superuser, database).await?;
            }
            // ...
        }
    }
}
```

#### 5.2 init_executor 变更

```rust
async fn init_executor(
    &self,
    keyspace: Option<String>,
    username: Option<String>,
    is_superuser: bool,
    database_name: String,  // 新增参数
) -> Result<(), String> {
    // ... existing store initialization ...
    
    // Bootstrap default database
    let mut txn = store.begin().await.map_err(|e| e.to_string())?;
    store.bootstrap_database(&mut txn, username.as_deref().unwrap_or("admin")).await.map_err(|e| e.to_string())?;
    txn.commit().await.map_err(|e| e.to_string())?;
    
    // Look up database ID
    let mut txn = store.begin().await.map_err(|e| e.to_string())?;
    let db_id = match store.get_database_id(&mut txn, &database_name).await.map_err(|e| e.to_string())? {
        Some(id) => id,
        None => {
            txn.rollback().await.ok();
            return Err(format!("database \"{}\" does not exist", database_name));
        }
    };
    txn.rollback().await.ok();
    
    // Create session with database ID and name
    let session = Session::new_with_database(
        store.clone(),
        tenant_obs.clone(),
        self.connection_id,
        db_id,
        database_name,
    );
    
    // ... rest of initialization ...
}
```

### 6. Executor 变更

#### 6.1 SQL 解析与执行

```rust
// src/sql/executor.rs

impl Executor {
    pub async fn execute_statement_on_txn(
        &self,
        session: &mut Session,
        txn: &mut Transaction,
        statement: &Statement,
    ) -> Result<ExecuteResult> {
        match statement {
            Statement::CreateDatabase { db_name, if_not_exists, .. } => {
                self.execute_create_database(session, txn, db_name, *if_not_exists).await
            }
            Statement::Drop { object_type: ObjectType::Database, names, if_exists, .. } => {
                self.execute_drop_database(session, txn, names, *if_exists).await
            }
            Statement::AlterDatabase { db_name, operation } => {
                self.execute_alter_database(session, txn, db_name, operation).await
            }
            // ... existing cases, all need to pass session.current_database_id() ...
        }
    }
    
    async fn execute_create_database(
        &self,
        session: &Session,
        txn: &mut Transaction,
        db_name: &ObjectName,
        if_not_exists: bool,
    ) -> Result<ExecuteResult> {
        // Only superuser can create database
        if !session.is_superuser() {
            return Err(anyhow!("permission denied to create database"));
        }
        
        let name = db_name.0.last()
            .ok_or_else(|| anyhow!("invalid database name"))?
            .value
            .to_lowercase();
        
        // Validate database name
        if name.is_empty() || name.len() > 63 {
            return Err(anyhow!("invalid database name: {}", name));
        }
        
        // Validate database name (no special characters)
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(anyhow!("invalid database name: {}", name));
        }
        
        let def = self.store.create_database(
            txn,
            &name,
            session.current_user(),
            if_not_exists,
        ).await?;
        
        if let Some(db_def) = def {
            // Create default 'public' schema in the new database
            self.store.create_schema_in_database(txn, db_def.id, "public", true).await?;
        }
        
        Ok(ExecuteResult::CreateDatabase)
    }
    
    async fn execute_drop_database(
        &self,
        session: &Session,
        txn: &mut Transaction,
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<ExecuteResult> {
        if !session.is_superuser() {
            return Err(anyhow!("permission denied to drop database"));
        }
        
        for name in names {
            let db_name = name.0.last()
                .ok_or_else(|| anyhow!("invalid database name"))?
                .value
                .to_lowercase();
            
            self.store.drop_database(
                txn,
                &db_name,
                if_exists,
                session.current_database(),
            ).await?;
        }
        
        Ok(ExecuteResult::DropDatabase)
    }
    
    async fn execute_alter_database(
        &self,
        session: &Session,
        txn: &mut Transaction,
        db_name: &ObjectName,
        operation: &AlterDatabaseOperation,
    ) -> Result<ExecuteResult> {
        if !session.is_superuser() {
            return Err(anyhow!("permission denied to alter database"));
        }
        
        let name = db_name.0.last()
            .ok_or_else(|| anyhow!("invalid database name"))?
            .value
            .to_lowercase();
        
        match operation {
            AlterDatabaseOperation::RenameDatabase { new_db_name } => {
                let new_name = new_db_name.0.last()
                    .ok_or_else(|| anyhow!("invalid new database name"))?
                    .value
                    .to_lowercase();
                
                // Validate new name
                if new_name.is_empty() || new_name.len() > 63 {
                    return Err(anyhow!("invalid database name: {}", new_name));
                }
                if !new_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(anyhow!("invalid database name: {}", new_name));
                }
                
                self.store.rename_database(
                    txn,
                    &name,
                    &new_name,
                    session.current_database(),
                ).await?;
            }
            AlterDatabaseOperation::ChangeOwner { new_owner_name } => {
                // Update owner in DatabaseDef
                let db_id = self.store.get_database_id(txn, &name).await?
                    .ok_or_else(|| anyhow!("database \"{}\" does not exist", name))?;
                let mut def = self.store.get_database_by_id(txn, db_id).await?
                    .ok_or_else(|| anyhow!("database metadata corrupted"))?;
                def.owner = new_owner_name.0.last()
                    .ok_or_else(|| anyhow!("invalid owner name"))?
                    .value
                    .clone();
                // Update in storage (implementation detail)
            }
            _ => {
                return Err(anyhow!("unsupported ALTER DATABASE operation"));
            }
        }
        
        Ok(ExecuteResult::AlterDatabase)
    }
}
```

#### 6.2 ExecuteResult 扩展

```rust
// src/sql/result.rs

pub enum ExecuteResult {
    // ... existing variants ...
    CreateDatabase,
    DropDatabase,
    AlterDatabase,
}
```

### 7. 系统目录变更

#### 7.1 pg_database

```rust
// src/sql/information_schema.rs

fn generate_pg_database(&self, txn: &mut Transaction) -> Result<Vec<Row>> {
    let databases = self.store.list_databases(txn).await?;
    let mut rows = Vec::new();
    
    for db in databases {
        rows.push(Row::new(vec![
            Value::Int32(db.oid as i32),           // oid
            Value::Text(db.name.clone()),          // datname
            Value::Int32(1),                       // datdba (owner OID, simplified)
            Value::Int32(6),                       // encoding (UTF8 = 6)
            Value::Text("C".to_string()),          // datcollate
            Value::Text("C".to_string()),          // datctype
            Value::Boolean(db.is_template),        // datistemplate
            Value::Boolean(db.allow_conn),         // datallowconn
            Value::Int32(-1),                      // datconnlimit
            Value::Int64(0),                       // datlastsysoid
            Value::Int64(0),                       // datfrozenxid
            Value::Int64(0),                       // datminmxid
            Value::Int32(0),                       // dattablespace
            Value::Text("".to_string()),           // datacl
        ]));
    }
    
    Ok(rows)
}
```

#### 7.2 current_database() 函数

```rust
// src/sql/expr.rs

fn eval_function(&self, name: &str, args: &[Expr], session: &Session, ...) -> Result<Value> {
    match name.to_uppercase().as_str() {
        "CURRENT_DATABASE" => {
            Ok(Value::Text(session.current_database().to_string()))
        }
        // ...
    }
}
```

### 8. 迁移策略

#### 8.1 不兼容变更声明

**⚠️ 这是一个破坏性变更（Breaking Change）**

本设计变更了存储层的 key 布局，**不提供向后兼容**：

| 项目 | 说明 |
|------|------|
| 旧数据 | 无法被新版本读取 |
| 新数据 | 无法被旧版本读取 |
| 迁移方式 | 需要重新初始化或使用迁移脚本 |

**理由：**
1. db9-server 目前处于早期开发阶段，没有生产环境数据需要迁移
2. 双读策略增加代码复杂度，且有性能开销
3. 新的 key 布局是根本性改变（增加 `d_{db_id}_` 前缀），双读维护成本高
4. 清晰的版本边界比复杂的兼容层更易维护

**升级路径：**
```bash
# 选项 1：重新初始化（推荐，适用于开发/测试环境）
tikv-ctl unsafe-recover drop-keyspace <keyspace>
# 或直接清空 TiKV 数据目录

# 选项 2：数据导出/导入（如有需要保留的数据）
pg_dump -h old-server -d postgres > backup.sql
# 升级 db9-server
psql -h new-server -d postgres < backup.sql
```

#### 8.2 版本标识

在 keyspace 中存储格式版本，便于检测不兼容：

```rust
// 存储格式版本
const STORAGE_FORMAT_VERSION: u32 = 2;  // v1 = 无 database, v2 = 有 database
const SYS_FORMAT_VERSION_KEY: &[u8] = b"_sys_format_version";

impl TikvStore {
    /// 启动时检查存储格式版本
    pub async fn check_format_version(&self) -> Result<()> {
        let mut txn = self.begin().await?;
        let key = self.key(&SYS_FORMAT_VERSION_KEY.to_vec());
        
        match txn.get(key.clone()).await? {
            Some(data) => {
                let version = u32::from_be_bytes(data.try_into()?);
                if version != STORAGE_FORMAT_VERSION {
                    return Err(anyhow!(
                        "Incompatible storage format: found v{}, expected v{}. \
                         Please re-initialize the keyspace or migrate data.",
                        version, STORAGE_FORMAT_VERSION
                    ));
                }
            }
            None => {
                // 新 keyspace，写入版本号
                txn_put(txn, key, STORAGE_FORMAT_VERSION.to_be_bytes().to_vec()).await?;
                txn.commit().await?;
            }
        }
        
        Ok(())
    }
}
```

#### 8.3 可选：离线迁移脚本

如果未来需要迁移旧数据，可提供离线脚本：

```bash
# 迁移脚本（可选，按需实现）
./scripts/migrate_v1_to_v2.py --keyspace default --pd 127.0.0.1:2379

# 迁移步骤：
# 1. 检查旧版本格式
# 2. 创建 postgres 数据库记录 (ID=1)
# 3. 扫描所有 _sys_schema_* keys，重写为 d_{1}_sys_schema_*
# 4. 扫描所有 t_* keys，重写为 d_{1}_t_*
# 5. 扫描所有 i_* keys，重写为 d_{1}_i_*
# 6. 删除旧格式 keys
# 7. 写入新版本号
```

**注意**：此脚本为可选实现，优先级低于核心功能。

### 9. 实现步骤

#### Phase 1: 基础设施（预计 2-3 天）

1. 添加 `DatabaseDef` 类型（包含 `id` 字段）
2. 实现新的编码函数（`encode_database_name_key`, `encode_database_id_key`, `encode_database_data_prefix`）
3. 实现 `TikvStore` 的数据库操作方法（create, drop, rename, list）
4. 添加数据库 bootstrap 逻辑

#### Phase 2: 连接流程（预计 2 天）

1. 修改 `StartupHandler` 解析 database 参数
2. 修改 `Session` 存储当前数据库 ID 和名称
3. 修改 `init_executor` 验证数据库存在并获取 ID
4. 更新 `current_database()` 函数

#### Phase 3: SQL 支持（预计 3-4 天）

1. 移除 `helpers.rs` 中的 skip 逻辑
2. 实现 `CREATE DATABASE` 执行
3. 实现 `DROP DATABASE` 执行（使用 DeleteRange）
4. 实现 `ALTER DATABASE ... RENAME TO` 执行
5. 添加 `ExecuteResult` 变体

#### Phase 4: Key 格式迁移（预计 4-5 天）

1. 添加新格式编码函数 (`_v2`)
2. 修改所有存储方法增加 `db_id` 参数
3. 实现双读策略
4. 编写迁移脚本
5. 测试迁移兼容性

#### Phase 5: 系统目录（预计 1-2 天）

1. 实现 `pg_database` 虚拟表
2. 更新 `information_schema` 相关表
3. 测试 ORM 兼容性

### 10. 测试计划

#### 10.1 单元测试

```rust
#[test]
fn test_encode_database_name_key() {
    let key = encode_database_name_key("mydb");
    assert_eq!(key, b"_sys_dbname_mydb".to_vec());
}

#[test]
fn test_encode_database_id_key() {
    let key = encode_database_id_key(1);
    assert_eq!(key[..10], b"_sys_dbid_"[..]);
    assert_eq!(&key[10..], &1_u64.to_be_bytes());
}

#[test]
fn test_encode_schema_key_v2() {
    let key = encode_schema_key_v2(1, "public.users");
    // d_{db_id:8bytes}_sys_schema_public.users
    assert!(key.starts_with(b"d_"));
    assert!(key.ends_with(b"_sys_schema_public.users"));
}

#[test]
fn test_encode_database_data_range() {
    let (start, end) = encode_database_data_range(5);
    // Should produce non-overlapping ranges
    assert!(start < end);
    // start should be d_{5}_
    // end should be d_{6}_
}
```

#### 10.2 集成测试

新增 `tests/XX_create_database.sql`:

```sql
-- 基本创建/删除
CREATE DATABASE testdb;
SELECT datname FROM pg_database WHERE datname = 'testdb';
-- Expected: testdb

DROP DATABASE testdb;
SELECT datname FROM pg_database WHERE datname = 'testdb';
-- Expected: (0 rows)

-- IF NOT EXISTS / IF EXISTS
CREATE DATABASE IF NOT EXISTS testdb;
CREATE DATABASE IF NOT EXISTS testdb; -- should not error
DROP DATABASE IF EXISTS testdb;
DROP DATABASE IF EXISTS testdb; -- should not error

-- RENAME DATABASE (高效，O(1) 操作)
CREATE DATABASE renametest;
ALTER DATABASE renametest RENAME TO renameddb;
SELECT datname FROM pg_database WHERE datname = 'renameddb';
-- Expected: renameddb
SELECT datname FROM pg_database WHERE datname = 'renametest';
-- Expected: (0 rows)
DROP DATABASE renameddb;

-- 验证 RENAME 后数据仍然存在
CREATE DATABASE datatest;
-- 需要重新连接到 datatest 创建表
-- \c datatest
-- CREATE TABLE t1 (id INT);
-- INSERT INTO t1 VALUES (1);
-- \c postgres
-- ALTER DATABASE datatest RENAME TO datatest2;
-- \c datatest2
-- SELECT * FROM t1;  -- 应该返回 1
-- \c postgres
-- DROP DATABASE datatest2;

-- 不能重命名当前数据库
-- ALTER DATABASE postgres RENAME TO newname; -- should error

-- 不能重命名为已存在的名称
CREATE DATABASE existdb;
CREATE DATABASE anotherdb;
-- ALTER DATABASE anotherdb RENAME TO existdb; -- should error
DROP DATABASE existdb;
DROP DATABASE anotherdb;

-- 权限测试（需要非 superuser）
-- CREATE ROLE normaluser LOGIN PASSWORD 'test';
-- \c postgres normaluser
-- CREATE DATABASE shouldfail; -- should error: permission denied

-- current_database()
SELECT current_database();
-- Expected: postgres

-- 列出所有数据库
SELECT datname FROM pg_database ORDER BY datname;
-- Expected: postgres (plus any others created)
```

#### 10.3 连接测试

```bash
# 测试默认数据库
psql -h 127.0.0.1 -p 5433 -U admin
# Should connect to 'postgres'

# 测试指定数据库
psql -h 127.0.0.1 -p 5433 -U admin -d mydb
# Should fail if mydb doesn't exist

# 创建并连接
psql -h 127.0.0.1 -p 5433 -U admin -c "CREATE DATABASE mydb"
psql -h 127.0.0.1 -p 5433 -U admin -d mydb
# Should succeed
```

#### 10.4 ORM 测试

验证常见 ORM 的 database 参数工作正常：

```javascript
// TypeORM
const dataSource = new DataSource({
    type: "postgres",
    host: "127.0.0.1",
    port: 5433,
    username: "admin",
    password: "admin",
    database: "myapp",  // Should work
});
```

### 11. 性能考虑

1. **Key 长度增加**
   - 新格式 key 增加 `d_{db_id:8bytes}_` 前缀（10 字节）
   - 影响：存储空间略增（约 0.1%）
   - 可接受：相比数据本身，元数据 key 数量有限

2. **操作复杂度**
   | 操作 | 复杂度 | 说明 |
   |------|--------|------|
   | CREATE DATABASE | O(1) | 2 个 key 写入 |
   | DROP DATABASE | O(1)* | DeleteRange + 2 个 key 删除 |
   | DROP TABLE | O(1)* | DeleteRange + 1 个 schema key 删除 |
   | TRUNCATE TABLE | O(1)* | DeleteRange（保留 schema） |
   | RENAME DATABASE | O(1) | 3 个 key 操作 |
   | 连接验证 | O(1) | 1 个 key 读取（可缓存） |
   
   *实际数据删除由 TiKV GC 异步完成

3. **元数据缓存策略**

   大部分元数据读多写少，适合在内存中缓存：

   | 缓存层级 | 缓存内容 | 失效策略 |
   |----------|----------|----------|
   | Keyspace 级 (TikvStore) | Database name → ID 映射 | DDL 时失效 |
   | 连接级 (Session) | 当前 database_id、TableSchema | 连接关闭时释放 |
   | 语句级 | 解析后的 schema 信息 | 语句结束时释放 |

   **为什么是 Keyspace 级而不是进程级？**
   - 租户隔离：每个 keyspace 是独立租户，缓存天然隔离
   - 简化 key：无需 `keyspace:dbname` 复合 key，直接用 `dbname`
   - 内存管理：可按租户设置缓存上限
   - 生命周期：缓存跟随 TikvStore，租户下线自动清理

   **方案选择：ArcSwap（写时复制，无读锁）**
   
   数据库元数据特点：写极少（DDL），读极频繁（每次连接）。
   使用 `ArcSwap` 实现无锁读、写时复制：

   ```rust
   // src/storage/tikv_store.rs - Keyspace 级数据库映射缓存
   use arc_swap::ArcSwap;
   use std::sync::Arc;
   
   /// 不可变的数据库缓存快照
   #[derive(Default)]
   struct DatabaseCache {
       name_to_id: HashMap<String, u64>,
       id_to_def: HashMap<u64, DatabaseDef>,
   }
   
   pub struct TikvStore {
       txn_client: TransactionClient,
       raw_client: RawClient,
       keyspace_prefix: Vec<u8>,
       
       /// Database 缓存 (写时复制，无读锁)
       /// - 读：load() 返回 Arc，无锁
       /// - 写：clone 整个 HashMap，修改后 store()
       db_cache: ArcSwap<DatabaseCache>,
   }
   
   impl TikvStore {
       /// 获取数据库 ID（无锁读）
       pub async fn get_database_id_cached(&self, db_name: &str) -> Result<Option<u64>> {
           // 1. 无锁读缓存
           let cache = self.db_cache.load();
           if let Some(&id) = cache.name_to_id.get(db_name) {
               return Ok(Some(id));
           }
           
           // 2. 缓存未命中，从 TiKV 读取
           let mut txn = self.begin().await?;
           let id = self.get_database_id(&mut txn, db_name).await?;
           txn.rollback().await.ok();
           
           // 3. 写时复制更新缓存
           if let Some(id) = id {
               self.update_db_cache(|cache| {
                   cache.name_to_id.insert(db_name.to_string(), id);
               });
           }
           
           Ok(id)
       }
       
       /// 写时复制更新缓存（DDL 时调用，极少发生）
       fn update_db_cache<F>(&self, updater: F) 
       where F: FnOnce(&mut DatabaseCache)
       {
           let old = self.db_cache.load();
           let mut new_cache = DatabaseCache {
               name_to_id: old.name_to_id.clone(),
               id_to_def: old.id_to_def.clone(),
           };
           updater(&mut new_cache);
           self.db_cache.store(Arc::new(new_cache));
       }
       
       /// CREATE DATABASE 后更新缓存
       pub fn cache_database(&self, def: &DatabaseDef) {
           self.update_db_cache(|cache| {
               cache.name_to_id.insert(def.name.clone(), def.id);
               cache.id_to_def.insert(def.id, def.clone());
           });
       }
       
       /// DROP DATABASE 后失效缓存
       pub fn invalidate_database_cache(&self, db_name: &str) {
           self.update_db_cache(|cache| {
               if let Some(id) = cache.name_to_id.remove(db_name) {
                   cache.id_to_def.remove(&id);
               }
           });
       }
       
       /// RENAME DATABASE 后更新缓存
       pub fn rename_database_cache(&self, old_name: &str, new_name: &str) {
           self.update_db_cache(|cache| {
               if let Some(id) = cache.name_to_id.remove(old_name) {
                   cache.name_to_id.insert(new_name.to_string(), id);
                   // id_to_def 中的 DatabaseDef.name 也需要更新
                   if let Some(def) = cache.id_to_def.get_mut(&id) {
                       def.name = new_name.to_string();
                   }
               }
           });
       }
   }
   ```
   
   **性能特点：**
   | 操作 | 锁 | 开销 |
   |------|-----|------|
   | 读缓存 | 无 | 原子 load，~10ns |
   | 写缓存 | 无（但 clone HashMap） | DDL 时 ~1μs |
   
   **为什么不用 DashMap？**
   - DashMap 仍有分片锁，读时需要获取
   - 数据库数量通常很少（<100），clone 成本可忽略
   - ArcSwap 读路径完全无锁，更适合"写极少读极多"场景

   ```rust
   // src/sql/session.rs - 连接级 Schema 缓存
   pub struct Session {
       // ... existing fields ...
       
       /// TableSchema 缓存 (per connection)
       /// 避免同一连接内重复读取相同表的 schema
       schema_cache: HashMap<String, TableSchema>,
   }
   
   impl Session {
       pub fn get_cached_schema(&self, table_name: &str) -> Option<&TableSchema> {
           self.schema_cache.get(table_name)
       }
       
       pub fn cache_schema(&mut self, table_name: String, schema: TableSchema) {
           self.schema_cache.insert(table_name, schema);
       }
       
       /// DDL 操作后清除相关缓存
       pub fn invalidate_schema_cache(&mut self, table_name: &str) {
           self.schema_cache.remove(table_name);
       }
       
       /// DROP DATABASE / RENAME DATABASE 后清除所有缓存
       pub fn clear_schema_cache(&mut self) {
           self.schema_cache.clear();
       }
   }
   ```

   **缓存架构图：**
   ```
   ┌─────────────────────────────────────────────────────────┐
   │  TikvClientPool                                         │
   │  clients: DashMap<keyspace, Arc<TikvStore>>            │
   │                                                         │
   │  ┌───────────────────────┐  ┌───────────────────────┐  │
   │  │ TikvStore (tenant_a)  │  │ TikvStore (tenant_b)  │  │
   │  │ db_name_cache: {      │  │ db_name_cache: {      │  │
   │  │   "postgres" → 1      │  │   "postgres" → 1      │  │
   │  │   "myapp" → 2         │  │   "other" → 2         │  │
   │  │ }                     │  │ }                     │  │
   │  └───────────────────────┘  └───────────────────────┘  │
   └─────────────────────────────────────────────────────────┘
                    │                      │
           ┌───────┴───────┐      ┌───────┴───────┐
           ▼               ▼      ▼               ▼
   ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
   │ Session     │ │ Session     │ │ Session     │
   │ (conn 1)    │ │ (conn 2)    │ │ (conn 3)    │
   │ db_id: 1    │ │ db_id: 2    │ │ db_id: 1    │
   │ schema_cache│ │ schema_cache│ │ schema_cache│
   └─────────────┘ └─────────────┘ └─────────────┘
   ```

   **缓存失效时机：**
   - `CREATE/DROP/ALTER TABLE` → 失效对应表的 schema 缓存
   - `CREATE/DROP/RENAME DATABASE` → 失效数据库名称缓存 + 清空 schema 缓存
   - 连接关闭 → 自动释放连接级缓存

4. **双读开销**
   - 迁移期间仅对 postgres 数据库（db_id=1）需要双读
   - 新创建的数据库无双读开销
   - 缓解：使用 schema cache 减少元数据查询
   - 长期：迁移完成后移除双读逻辑

5. **Database Bootstrap**
   - 每次连接需验证数据库存在（1 次 key 读取）
   - 缓解：使用进程级 db_name_cache，首次连接后缓存

6. **大数据量 DROP/TRUNCATE**
   - TiKV DeleteRange 是 O(1) 标记操作
   - 实际数据由 GC 异步清理，不影响前台操作
   - 注意：大量数据删除可能增加 GC 压力
   
   | 操作 | 10M 行表 | 传统方式 |
   |------|----------|----------|
   | DROP TABLE | ~毫秒 | ~分钟 (scan+delete) |
   | TRUNCATE TABLE | ~毫秒 | ~分钟 (scan+delete) |
   | DROP DATABASE | ~毫秒 | ~小时 (取决于数据量) |

### 12. 安全考虑

1. **权限控制**
   - 只有 SUPERUSER 可以 CREATE/DROP/RENAME DATABASE
   - 普通用户只能连接已授权的数据库

2. **资源隔离**
   - 每个数据库的数据完全隔离（不同 key 前缀）
   - 无法跨数据库查询（与 PostgreSQL 一致）
   - 用户/角色是 keyspace 级别共享（可跨数据库）

3. **名称验证**
   - 数据库名长度限制（63 字符）
   - 只允许字母数字和下划线
   - 保留名称保护（postgres, template0, template1）

4. **ID 安全**
   - Database ID 单调递增，不会重用
   - 防止 DROP + CREATE 后访问到旧数据

### 13. 未来扩展

1. **ALTER DATABASE 扩展**
   - 修改 owner（已支持）
   - 修改连接限制
   - ALTER DATABASE ... SET ...

2. **Database 级别权限**
   - GRANT CONNECT ON DATABASE
   - REVOKE CONNECT ON DATABASE

3. **模板数据库**
   - CREATE DATABASE ... TEMPLATE = xxx
   - 需要复制整个数据库的数据

4. **高效批量迁移**
   - 使用 RawKV API 的 DeleteRange（比事务内扫描删除更高效）
   - 支持大数据库的快速 DROP

## 附录

### A. PostgreSQL CREATE DATABASE 完整语法

```sql
CREATE DATABASE name
    [ WITH ] [ OWNER [=] user_name ]
           [ TEMPLATE [=] template ]
           [ ENCODING [=] encoding ]
           [ LOCALE [=] locale ]
           [ LC_COLLATE [=] lc_collate ]
           [ LC_CTYPE [=] lc_ctype ]
           [ ICU_LOCALE [=] icu_locale ]
           [ ICU_RULES [=] icu_rules ]
           [ LOCALE_PROVIDER [=] locale_provider ]
           [ COLLATION_VERSION [=] collation_version ]
           [ TABLESPACE [=] tablespace_name ]
           [ ALLOW_CONNECTIONS [=] allowconn ]
           [ CONNECTION LIMIT [=] connlimit ]
           [ IS_TEMPLATE [=] istemplate ]
           [ OID [=] oid ]
           [ STRATEGY [=] strategy ]
```

MVP 仅支持：`CREATE DATABASE name [WITH OWNER = user_name]`

### B. 相关 PR/Issue

- 待创建

### C. 参考资料

- [PostgreSQL CREATE DATABASE](https://www.postgresql.org/docs/current/sql-createdatabase.html)
- [TiKV Keyspace](https://docs.pingcap.com/tidb/stable/tikv-configuration-file#api-version)
- [db9-server Multi-Tenancy](./multi-tenancy.md)
