# fs9 WebSocket API 设计

## 1. 摘要

本文设计一个基于 WebSocket 的文件系统接口，将 db9-server 内置的 `fs9` 嵌入式文件系统（`EmbeddedPageFs`）暴露给非 SQL 客户端（CLI 工具、Web 应用、IDE 插件等）。

核心原则：

- **POSIX 风格 API 形状**：提供 `stat`、`readdir`、`read`、`write`、`mkdir`、`unlink` 等操作；但语义以 **fs9/EmbeddedPageFs 的现有行为为准**（例如：写入会自动创建父目录；不支持 symlink；mtime 精度为秒级）。
- **多租户原生**：db9 是多租户服务。每个 WebSocket 连接通过 `parse_tenant_username()` 绑定到一个 TiKV keyspace，FsBackend 实例按租户创建，文件数据完全隔离。连接数、读预算等均按租户追踪。
- **无状态路径模型**：不引入文件描述符（fd），每次操作以绝对路径定位目标。WebSocket 连接本身已提供会话状态，无需额外 fd 管理。
- **认证复用**：WebSocket 认证**完全复用** db9 现有的 `AuthManager`（`src/auth/rbac.rs`），包括租户解析（`parse_tenant_username`）和 TiKV 客户端池（`TikvClientPool`）。不引入任何新的认证机制。
- **最小依赖**：仅新增 `tokio-tungstenite` crate，不引入 HTTP 框架（axum/actix 等），保持 db9-server 作为 pgwire-only 服务器的架构定位。

---

## 2. 背景与现状

### 2.1 db9-server 服务架构

db9-server 是一个纯 PostgreSQL 协议服务器，使用 `pgwire 0.28` crate 在 TCP 端口 5433 上提供服务。**服务器内不存在 HTTP 服务器**——`src/extensions/http.rs` 中的 HTTP 扩展是一个出站 HTTP **客户端**（SQL 表函数 `http_get`/`http_post` 等），不是入站服务器。`Cargo.toml` 中也没有任何 WebSocket 相关依赖。

### 2.2 fs9 现状

`fs9` 是 db9-server 的内置扩展，提供嵌入式文件系统功能。当前仅通过 SQL 函数暴露：

| SQL 函数 | 功能 |
|---------|------|
| `fs9_read(path)` | 读取文件内容 |
| `fs9_write(path, data)` | 写入文件 |
| `fs9_exists(path)` | 检查路径是否存在 |
| `fs9_size(path)` | 获取文件大小 |
| `fs9_mtime(path)` | 获取修改时间 |
| `FROM extensions.fs9(path)` | 表函数（目录/文件/glob 查询） |

**权限控制**：所有 fs9 SQL 函数要求 **superuser** 权限（`ensure_permissions()` 检查 `is_superuser()` 和 `is_backend_available()`）。

### 2.3 存储层：EmbeddedPageFs

fs9 底层使用 `EmbeddedPageFs`（`src/extensions/fs/embedded/pagefs.rs`），一个基于 inode 的文件系统，数据存储在 TiKV 中：

- **页大小**：`PAGE_SIZE = 16KB`
- **Inode 模型**：`InodeType::File` / `InodeType::Directory`，包含 `size`、`mode`、`mtime`、`atime`、`ctime`、`nlink` 等属性
- **Superblock**：记录 `next_inode`、`page_size`、`total_pages`、`used_pages`

### 2.4 FsBackend Trait

`src/extensions/fs/backend.rs` 定义了 `FsBackend` trait，提供 13 个异步方法：

```rust
#[async_trait]
pub(crate) trait FsBackend: Send + Sync {
    async fn stat(&self, path: &str) -> Result<FsFileInfo>;
    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>>;
    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>>;
    async fn read_file_stream(&self, path: &str, max_bytes: usize)
        -> Result<Box<dyn AsyncBufRead + Unpin + Send>>;
    async fn remove(&self, path: &str) -> Result<()>;
    async fn remove_recursive(&self, path: &str) -> Result<u64>;
    async fn mkdir(&self, path: &str, recursive: bool) -> Result<()>;
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<usize>;
    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>>;
    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize>;
    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize>;
    async fn truncate(&self, path: &str, size: u64) -> Result<()>;
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()>;
}
```

`FsFileInfo` 结构：

```rust
pub(crate) struct FsFileInfo {
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub mtime: u64,
}
```

### 2.5 现有限制

| 限制 | 值 | 位置 |
|------|---|------|
| 单文件最大读写 | 10 MB | `MAX_BYTES_PER_FILE`（`src/extensions/fs/mod.rs`） |
| Glob 查询最大总字节 | 100 MB | `MAX_TOTAL_BYTES` |
| Glob 最大文件数 | 10,000 | `MAX_FILES_PER_GLOB` |
| 全局并发读预算 | 128 MB | `FS9_READ_BUDGET`（`src/sql/expr/functions/fs9.rs`） |
| 权限要求 | superuser | `ensure_permissions()` |

---

## 3. 目标与非目标

### 3.1 目标（MVP）

1. 提供 WebSocket 接口，支持 12 个 POSIX 风格文件操作（`auth`、`stat`、`readdir`、`mkdir`、`unlink`、`rm`、`read`、`write`、`pwrite`、`append`、`truncate`、`rename`）。
2. **认证完全复用** db9 现有的 `AuthManager`，通过 `parse_tenant_username()` 解析租户、`TikvClientPool::acquire()` 获取 TiKV 客户端。
3. 多租户 keyspace 隔离——每个 WebSocket 连接绑定到一个 keyspace，与 pgwire 连接的隔离模型一致。
4. 大文件（≥1MB）流式传输协议，支持分块读写。
5. 生产级安全与限制（连接数、超时、消息大小等）。

### 3.2 非目标（v1 不做）

- **不引入文件描述符**：采用无状态路径模型。WebSocket 连接本身已提供会话上下文，fd 管理增加复杂度但对嵌入式 fs 无显著收益。
- **不支持 FUSE 挂载**：fs9 是嵌入式文件系统，不暴露为操作系统级别的挂载点。
- **不提供 HTTP REST 替代**：v1 仅支持 WebSocket。REST 可作为后续扩展。
- **不引入文件级 ACL**：延续 fs9 SQL 函数的 superuser-only 权限模型。细粒度 ACL 可在后续版本中添加。
- **不做 Watch/监听**：文件变更通知（类似 `inotify`）不在 v1 范围内。

---

## 4. 传输层设计

### 4.1 监听与端口

WebSocket 服务在**独立端口**上运行，不与 pgwire（端口 5433）复用。

| 配置项 | 环境变量 | 默认值 | 说明 |
|--------|---------|--------|------|
| WebSocket 监听地址 | `FS9_WS_LISTEN_ADDR` | `127.0.0.1` | 默认仅监听 loopback，避免在未启用 TLS 的情况下暴露明文密码认证 |
| WebSocket 端口 | `FS9_WS_PORT` | `5480` | 设为 `0` 则禁用 WebSocket 服务 |
| TLS 证书 | `PG_TLS_CERT` | （空） | 复用 pgwire 的 TLS 证书 |
| TLS 私钥 | `PG_TLS_KEY` | （空） | 复用 pgwire 的 TLS 私钥 |

当 `PG_TLS_CERT` 和 `PG_TLS_KEY` 均已配置时，WebSocket 服务自动升级为 WSS（WebSocket over TLS）。

实现细节注记：pgwire 的 `tls::setup_tls()` 会将 ALPN 固定为 `postgresql`；WSS 握手走 HTTP/1.1 Upgrade，建议为 WebSocket listener 单独构造 `rustls::ServerConfig`（复用同一套 cert/key，但 ALPN 使用 `http/1.1` 或留空），避免客户端因 ALPN 不匹配连接失败。

安全默认值与 pgwire 一致：
- 若 `FS9_WS_LISTEN_ADDR` 为非 loopback，且 TLS 未配置，则除非显式启用 `DB9_DEV=1` 或 `DB9_INSECURE=1`，否则应拒绝启动 WebSocket listener（避免非 TLS 明文密码暴露）。
- `PG_REQUIRE_TLS=1` 时，WebSocket 也必须使用 WSS。

### 4.2 启动流程

在 `src/main.rs` 中，与 pgwire TCP listener 并行启动 WebSocket listener：

```rust
// 伪代码：main.rs 中新增
let ws_listen_addr = std::env::var("FS9_WS_LISTEN_ADDR")
    .unwrap_or_else(|_| "127.0.0.1".to_string());
let ws_port = std::env::var("FS9_WS_PORT")
    .unwrap_or_else(|_| "5480".to_string())
    .parse::<u16>()?;

if ws_port > 0 {
    let ws_listener = TcpListener::bind((ws_listen_addr.as_str(), ws_port)).await?;
    info!(
        "fs9 WebSocket listening on {}:{}",
        ws_listen_addr, ws_port
    );
    
    tokio::spawn(async move {
        loop {
            let (stream, addr) = match ws_listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("fs9 WebSocket accept error: {}", e);
                    continue;
                }
            };
            tokio::spawn(ws::handle_connection(stream, addr, client_pool.clone()));
        }
    });
}
```

### 4.3 连接生命周期

```
Client                                Server
  |                                     |
  |--- TCP connect ------------------>  |
  |<-- TCP accept --------------------  |
  |--- WebSocket upgrade ------------>  |
  |<-- WebSocket accept --------------  |
  |                                     |
  |--- auth message ----------------->  | ← 必须在 AUTH_TIMEOUT (10s) 内完成
  |<-- auth response (ok/error) ------  |
  |                                     |
  |--- operation (stat/read/...) ---->  | ← 认证后可发送任意操作
  |<-- operation response ------------  |
  |    ...                              |
  |                                     | ← IDLE_TIMEOUT (300s) 无消息则断开
  |--- close ----------------------->   |
  |<-- close -------------------------  |
```

### 4.4 依赖选择

使用 `tokio-tungstenite`（`Cargo.toml` 新增）：

```toml
[dependencies]
tokio-tungstenite = "0.24"
```

选择 `tokio-tungstenite` 而非 axum 的理由：
- db9-server 不包含 HTTP 框架，引入 axum 仅为 WebSocket 过于重量级
- `tokio-tungstenite` 直接在 TCP stream 上做 WebSocket 升级，与现有 pgwire 架构一致
- 依赖少、编译快、API 简洁

---

## 5. 认证设计——复用 AuthManager

**这是本设计最关键的部分。** WebSocket 认证**完全复用** db9 现有的认证机制，不引入任何新的用户体系或密码验证逻辑。

### 5.1 现有认证组件

| 组件 | 位置 | 作用 |
|------|------|------|
| `AuthManager` | `src/auth/rbac.rs` | 无状态认证管理器，`authenticate()` 方法验证用户名/密码 |
| `User` | `src/auth/rbac.rs` | 用户模型，包含 `is_superuser`、`can_login`、`password_hash`、`password_salt` 等 |
| `parse_tenant_username()` | `src/protocol/handler/tenant.rs` | 从 `"tenant.user"` / `"tenant:user"` 格式解析出 `(keyspace, actual_user)`（**注**：当前函数为 `pub(super)`；若 WebSocket 模块放在 `src/extensions/` 下，需要提升为 `pub(crate)` 或抽到共享模块以保持单一来源） |
| `TikvClientPool` | `src/pool.rs` | 管理按 keyspace 隔离的 TiKV 客户端连接池 |
| `verify_password()` | `src/auth/password.rs` | SHA-256 + salt 密码哈希验证 |

### 5.2 pgwire 认证流程（参考）

当前 pgwire 认证流程（`src/protocol/handler/dynamic/startup.rs`）：

```
1. 客户端发送 Startup，raw_user = "myapp.admin"（或 "myapp:admin"）
2. parse_tenant_username(raw_user) → (Some("db9_tenant_myapp"), "admin")
3. 发送 CleartextPassword 挑战
4. 客户端返回密码
5. pool.get_client(keyspace) → Arc<TikvStore>
6. AuthManager::new().bootstrap(&mut txn) （幂等，首次创建默认管理员）
7. AuthManager::new().authenticate(&mut txn, username, password) → Option<User>
8. 成功：初始化 Executor + Session，存储 is_superuser
9. 失败：返回 SQLSTATE 28P01 错误
```

### 5.3 WebSocket 认证流程（复用）

WebSocket 认证**直接复用上述组件**，流程如下：

```
Client                                  Server
  |                                       |
  |--- WebSocket connect ---------------> |
  |                                       |
  |--- {"op":"auth",                      |
  |     "username":"myapp.admin",    ---> | 1. parse_tenant_username("myapp.admin")
  |     "password":"xxx"}                 |    → (Some("db9_tenant_myapp"), "admin")
  |                                       |
  |                                       | 2. client_pool.acquire(keyspace)
  |                                       |    → TenantHandle { store, ... }
  |                                       |
  |                                       | 3. store.begin() → txn
  |                                       |    auth_manager.bootstrap(&mut txn)
  |                                       |    txn.commit()
  |                                       |
  |                                       | 4. store.begin() → txn
  |                                       |    auth_manager.authenticate(
  |                                       |      &mut txn, "admin", "xxx"
  |                                       |    ) → Option<User>
  |                                       |
  |                                       | 5. Some(user):
  |                                       |    - user.is_superuser == false?
  |                                       |      → 拒绝 (EACCES, fs9 需要 superuser)
  |                                       |    - user.is_superuser == true?
  |                                       |      → 创建 WsSession，存储 TenantHandle
  |                                       |
  |<-- {"ok":true, "data":{...}} -------- | 6. 返回认证结果
  |                                       |
  | ← 认证成功，后续操作在此 WsSession 上执行 |
```

### 5.4 认证实现伪代码

```rust
use crate::auth::rbac::AuthManager;
use crate::pool::TikvClientPool;
use crate::protocol::handler::tenant::parse_tenant_username;

/// WebSocket 会话状态（认证成功后持有）
struct WsSession {
    tenant_handle: TenantHandle,   // 持有 TiKV 连接引用（RAII，Drop 时释放）
    backend: Box<dyn FsBackend>,   // 该租户的 EmbeddedFsBackend 实例
    user: String,                  // 实际用户名（去除租户前缀后）
    is_superuser: bool,            // 超级用户标志
    keyspace: String,              // 所属 keyspace
}

/// 处理 WebSocket auth 消息——完全复用 AuthManager
async fn handle_auth(
    username: &str,
    password: &str,
    pool: &TikvClientPool,
) -> Result<WsSession, WsError> {
    // 1. 租户解析——复用 parse_tenant_username
    let (keyspace, actual_user) = parse_tenant_username(username);
    let effective_ks = keyspace.unwrap_or_else(|| "default".to_string());

    // 2. 获取租户 TiKV 客户端——复用 TikvClientPool
    let tenant = pool.acquire(Some(effective_ks.clone())).await
        .map_err(|e| WsError::Internal(format!("pool acquire failed: {}", e)))?;
    let store = tenant.store().clone();

    // 3. Bootstrap（幂等）——与 pgwire startup.rs 行为一致
    let auth_manager = AuthManager::new();
    {
        let mut txn = store.begin().await?;
        auth_manager.bootstrap(&mut txn).await?;
        txn.commit().await?;
    }

    // 4. 认证——复用 AuthManager::authenticate
    let mut txn = store.begin().await?;
    match auth_manager.authenticate(&mut txn, &actual_user, password).await? {
        Some(user) => {
            txn.commit().await?;

            // 5. 权限检查——fs9 要求 superuser
            if !user.is_superuser {
                return Err(WsError::PermissionDenied(
                    "fs9: permission denied (superuser required)".to_string()
                ));
            }

            // 6. 创建租户专属 FsBackend
            let tikv_client = store.transaction_client()
                .ok_or_else(|| WsError::Internal("no tikv client".into()))?;
            let backend = EmbeddedFsBackend::new(tikv_client).await
                .map_err(|e| WsError::Internal(format!("fs backend init: {}", e)))?;

            Ok(WsSession {
                tenant_handle: tenant,
                backend: Box::new(backend),
                user: actual_user,
                is_superuser: true,
                keyspace: effective_ks,
            })
        None => {
            txn.rollback().await.ok();
            Err(WsError::AuthFailed(format!(
                "password authentication failed for user \"{}\"", actual_user
            )))
        }
    }
}
```

### 5.5 认证约束

| 约束 | 说明 |
|------|------|
| 认证超时 | 连接建立后 **10 秒** 内必须完成 `auth` 操作，否则服务端主动断开 |
| auth 必须是首条消息 | 未认证状态下发送非 `auth` 操作，立即返回 `EPROTO` 并断开 |
| auth 失败处理 | 返回 `EAUTH`（或非 superuser 场景返回 `EACCES`）后**立即断开连接**，与 pgwire 的“认证失败即终止连接”一致 |
| 密码传输 | 明文（与 pgwire CleartextPassword 一致）。生产环境应启用 WSS (TLS) |
| 多次 auth | 已认证后再发 `auth` 消息，返回 `EPROTO` 错误（不支持重新认证） |
| can_login 检查 | `AuthManager::authenticate` 内部已检查 `user.can_login`，不可登录的用户直接拒绝 |

### 5.6 多租户隔离设计

**db9 是一个多租户服务**，WebSocket 层必须在每个环节保证租户隔离。本节详细说明隔离机制。

#### 5.6.1 隔离架构总览

```
租户 A (keyspace: db9_tenant_appA)          租户 B (keyspace: db9_tenant_appB)
  │                                           │
  │ WS connect + auth("appA.admin")           │ WS connect + auth("appB.admin")
  ▼                                           ▼
┌─────────────────────┐                ┌─────────────────────┐
│ WsSession A         │                │ WsSession B         │
│  tenant_handle ──┐  │                │  tenant_handle ──┐  │
│  backend (fs) ─┐ │  │                │  backend (fs) ─┐ │  │
│                │ │  │                │                │ │  │
└────────────────│─│──┘                └────────────────│─│──┘
                 │ │                                    │ │
                 │ └──► TenantEntry A                   │ └──► TenantEntry B
                 │       store: TikvStore(ks=appA)      │       store: TikvStore(ks=appB)
                 │                                      │
                 └──► EmbeddedPageFs(client_A)           └──► EmbeddedPageFs(client_B)
                       inode 空间独立                          inode 空间独立
                       TiKV key 前缀隔离                      TiKV key 前缀隔离
```

#### 5.6.2 隔离保证——逐层分析

| 层 | 隔离机制 | 代码位置 | 说明 |
|---|---------|---------|------|
| **连接层** | `parse_tenant_username()` | `src/protocol/handler/tenant.rs` | 从 `"tenant.user"` / `"tenant:user"` 格式中提取 keyspace，绑定到 WsSession 生命周期。连接一旦建立，keyspace 不可变 |
| **认证层** | `TikvClientPool::acquire(keyspace)` | `src/pool.rs` | 获取该 keyspace 专属的 `TikvStore`，底层 `TransactionClient` 绑定了 TiKV Keyspace API——所有 key 操作自动加 keyspace 前缀 |
| **文件系统层** | `EmbeddedFsBackend::new(client)` | `src/extensions/fs/embedded/mod.rs` | 使用该租户的 `TransactionClient` 创建 `EmbeddedPageFs`。inode、superblock、数据页的 TiKV key 均在该 keyspace 内——租户 A 的 `/data/file.csv` 与租户 B 的 `/data/file.csv` 是完全独立的文件，存储在不同的 TiKV key 范围中 |
| **操作层** | WsSession 持有 `Box<dyn FsBackend>` | `ws/handler.rs` | 每次操作使用 session 内的 backend 实例，不可能跨 keyspace 访问。即使两个租户使用相同的文件路径（如 `/data/report.csv`），数据也完全隔离 |

#### 5.6.3 FsBackend 按租户实例化

当前 fs9 SQL 函数通过 task-local `ExtensionContext` 获取 `tikv_client()`，再调用 `get_backend(tenant)` 创建 `EmbeddedFsBackend`。WebSocket 层**不使用 task-local context**，而是在认证时从 `TenantHandle` 直接获取 `TransactionClient`：

```rust
// 关键代码路径：
// 1. TenantHandle.store() → &Arc<TikvStore>
// 2. TikvStore::transaction_client() → Option<Arc<TransactionClient>>
// 3. EmbeddedFsBackend::new(client) → Result<EmbeddedFsBackend>

let store = tenant.store().clone();
let tikv_client = store.transaction_client()  // src/storage/tikv_store/mod.rs:103
    .expect("TikvStore must have client");
let backend = EmbeddedFsBackend::new(tikv_client).await?;  // src/extensions/fs/embedded/mod.rs:19
```

**与 SQL 路径的对比**：

| | SQL 函数路径（现有） | WebSocket 路径（新增） |
|---|---|---|
| TiKV 客户端来源 | task-local `context::tikv_client()` | `TenantHandle.store().transaction_client()` |
| FsBackend 生命周期 | 每次 SQL 语句执行时创建 | 每个 WsSession 创建时创建，连接期间复用 |
| keyspace 绑定 | `ExtensionContext.tenant_keyspace` | `WsSession.keyspace`（来自 `parse_tenant_username`） |
| 效果 | 完全等价——都是用该租户的 `TransactionClient` 创建 `EmbeddedPageFs` |

#### 5.6.4 每租户连接数追踪

WebSocket 层需要 **按 keyspace** 追踪活跃连接数，拒绝超限连接：

```rust
/// 全局 WebSocket 连接计数器（按 keyspace 隔离）
struct WsConnectionTracker {
    counts: RwLock<HashMap<String, AtomicU32>>,  // keyspace → active count
    max_per_tenant: u32,                          // 默认 50
}

impl WsConnectionTracker {
    /// 尝试注册新连接。超限时返回 Err。
    fn try_acquire(&self, keyspace: &str) -> Result<WsConnectionGuard> {
        let counts = self.counts.read();
        let counter = counts.get(keyspace);
        let current = counter.map(|c| c.load(Ordering::Relaxed)).unwrap_or(0);
        if current >= self.max_per_tenant {
            return Err(WsError::TooManyConnections(keyspace.to_string()));
        }
        // 递增计数，返回 RAII guard
        // ...
    }
}

/// RAII guard: Drop 时自动递减该 keyspace 的连接计数
struct WsConnectionGuard {
    keyspace: String,
    tracker: Arc<WsConnectionTracker>,
}
```

注意：`TenantHandle` 本身已通过 `active_connections` 原子计数器追踪 pgwire + WebSocket 的总连接数，确保空闲租户可以被 reaper 正确回收。WebSocket 连接持有 `TenantHandle`（RAII），断开时自动递减 `active_connections`——防止已无活跃连接的 `TenantEntry` 长期驻留内存。

#### 5.6.5 每租户限制与配额

| 限制 | 作用域 | 说明 |
|------|--------|------|
| 最大 WS 连接数 | **每 keyspace** | 默认 50，通过 `WsConnectionTracker` 追踪。防止单租户占用所有服务端资源 |
| 空闲超时 | **每连接** | 300s 无消息则断开。同时 `TenantHandle` Drop 后触发 pool reaper |
| 文件大小限制 | **全局** | `MAX_BYTES_PER_FILE = 10MB`，所有租户共享此硬限制 |
| 读预算 | **全局** | `FS9_READ_BUDGET = 128MB` 全局并发读预算。后续版本可扩展为每租户独立预算 |
| 认证超时 | **每连接** | 10s，防止未认证连接长期占用资源 |

#### 5.6.6 租户生命周期边界场景

| 场景 | 行为 |
|------|------|
| 租户 A 写入 `/data/file.csv`，租户 B 读取 `/data/file.csv` | 各自独立：A 的文件在 keyspace `db9_tenant_A` 的 TiKV key 范围内；B 的读取在 `db9_tenant_B` 范围内，返回 `ENOENT`（如果 B 没有同名文件） |
| 不带租户前缀的用户名（如 `admin`） | 使用 default keyspace（与 pgwire 行为一致）。所有未指定租户的连接共享 default keyspace 的文件系统 |
| 多个 WebSocket 连接同一租户 | 共享同一个 `TenantEntry`（`TikvStore`），通过 TiKV 事务保证并发一致性。EmbeddedPageFs 当前使用 TiKV **乐观事务**（OCC）；并发写冲突可能以事务提交失败的形式暴露给上层，需要客户端/调用方按需重试 |
| 租户 TikvStore 被 pool reaper 回收后新连接到达 | `TikvClientPool::acquire()` 会重新创建 `TenantEntry`（含新的 `TikvStore` + `TransactionClient`），文件数据仍在 TiKV 中，不受影响 |
| pgwire 和 WebSocket 同时操作同一租户的文件 | 共享同一个 `TenantEntry`（通过 `TikvClientPool`）。pgwire 的 fs9 SQL 函数和 WebSocket 操作访问相同的 TiKV keyspace，数据实时可见。并发控制由 TiKV 事务保证 |

---

## 6. 协议设计

### 6.1 帧类型

| WebSocket 帧类型 | 用途 |
|------------------|------|
| Text（JSON） | 控制消息：请求、响应、流式控制帧 |
| Binary | 文件数据传输：流式读写的 chunk 数据 |
| Ping/Pong | 心跳保活（需要应用层显式定期发送 Ping 并处理 Pong/超时；库不会“自动”提供存活策略） |
| Close | 连接关闭 |

### 6.2 请求格式

所有请求为 JSON text frame：

```json
{
  "id": "req-1",
  "op": "stat",
  "path": "/data/file.csv"
}
```

| 字段 | 类型 | 必须 | 说明 |
|------|------|------|------|
| `id` | string | 是 | 请求 ID，客户端生成，响应中原样返回，用于请求/响应关联 |
| `op` | string | 是 | 操作名称（`auth`/`stat`/`readdir`/`mkdir`/`unlink`/`rm`/`read`/`write`/`pwrite`/`append`/`truncate`/`rename`） |
| `path` | string | 视操作 | 目标文件/目录的绝对路径 |
| 其他 | - | 视操作 | 各操作的特有参数（见 §7） |

### 6.3 成功响应

```json
{
  "id": "req-1",
  "ok": true,
  "data": {
    "path": "/data/file.csv",
    "type": "file",
    "size": 1024,
    "mode": 33188,
    "mtime": "2026-01-15T08:30:00Z"
  }
}
```

### 6.4 错误响应

```json
{
  "id": "req-1",
  "ok": false,
  "error": {
    "code": "ENOENT",
    "message": "No such file or directory: /data/file.csv"
  }
}
```

### 6.5 并发请求

客户端可以在同一连接上并发发送多个请求（不同 `id`）。服务端可以并发执行这些请求，因此响应顺序不保证（通过 `id` 关联）。建议客户端合理控制并发度（≤10 个 inflight 请求）。

---

## 7. 操作定义

### 7.1 `auth` — 认证

**必须是连接后的第一条消息。**

请求：
```json
{"id": "1", "op": "auth", "username": "myapp.admin", "password": "secret"}
```

成功响应：
```json
{"id": "1", "ok": true, "data": {"user": "admin", "tenant": "myapp", "keyspace": "db9_tenant_myapp"}}
```

失败响应：
```json
{"id": "1", "ok": false, "error": {"code": "EAUTH", "message": "password authentication failed for user \"admin\""}}
```

认证失败（含非 superuser）后服务端应立即关闭连接，避免暴露可被暴力尝试的长连接认证面。

映射：`parse_tenant_username()` → `TikvClientPool::acquire()` → `AuthManager::authenticate()`

### 7.2 `stat` — 获取文件/目录元数据

请求：
```json
{"id": "2", "op": "stat", "path": "/data/file.csv"}
```

成功响应：
```json
{
  "id": "2", "ok": true,
  "data": {
    "path": "/data/file.csv",
    "type": "file",
    "size": 1024,
    "mode": 33188,
    "mtime": "2026-01-15T08:30:00Z"
  }
}
```

映射：`FsBackend::stat(path)` → `FsFileInfo`

`type` 字段值：`"file"` 或 `"dir"`。`mode` 为 Unix 权限位（如 `33188` = `0100644`）。`mtime` 为 RFC 3339 格式时间戳（秒级精度，对应 inode 的 Unix epoch seconds）。

### 7.3 `readdir` — 列出目录内容

请求：
```json
{"id": "3", "op": "readdir", "path": "/data/"}
```

成功响应：
```json
{
  "id": "3", "ok": true,
  "data": {
    "entries": [
      {"path": "/data/a.csv", "type": "file", "size": 100, "mode": 33188, "mtime": "2026-01-15T08:30:00Z"},
      {"path": "/data/subdir", "type": "dir", "size": 0, "mode": 16877, "mtime": "2026-01-14T12:00:00Z"}
    ]
  }
}
```

映射：`FsBackend::readdir(path)` → `Vec<FsFileInfo>`

### 7.4 `mkdir` — 创建目录

请求：
```json
{"id": "4", "op": "mkdir", "path": "/data/subdir", "recursive": true}
```

成功响应：
```json
{"id": "4", "ok": true, "data": {}}
```

映射：`FsBackend::mkdir(path, recursive)`

`recursive` 可选，默认 `false`。当 `recursive: true` 时，自动创建中间目录（类似 `mkdir -p`）。

### 7.5 `unlink` — 删除文件

请求：
```json
{"id": "5", "op": "unlink", "path": "/data/file.csv"}
```

成功响应：
```json
{"id": "5", "ok": true, "data": {}}
```

映射：`FsBackend::remove(path)`

仅删除文件。对目录执行 `unlink` 将返回 `EISDIR`。

### 7.6 `rm` — 删除目录/文件（可递归）

请求：
```json
{"id": "6", "op": "rm", "path": "/data/subdir", "recursive": true}
```

成功响应：
```json
{"id": "6", "ok": true, "data": {"removed": 42}}
```

映射：
- `recursive: true` → `FsBackend::remove_recursive(path)`，返回删除的条目数
- `recursive: false` → `FsBackend::remove(path)`，目录非空时返回 `ENOTEMPTY`

### 7.7 `read` — 读取文件

请求：
```json
{"id": "7", "op": "read", "path": "/data/file.csv"}
```

**小文件（< 1MB）** — 内联 base64 响应：
```json
{
  "id": "7", "ok": true,
  "data": {
    "content": "aGVsbG8gd29ybGQ=",
    "size": 11,
    "encoding": "base64"
  }
}
```

**大文件（≥ 1MB）** — 流式传输（见 §8）。

映射：`FsBackend::read_file(path, MAX_BYTES_PER_FILE)`

可选参数 `offset` 和 `length` 用于部分读取：
```json
{"id": "7b", "op": "read", "path": "/data/big.bin", "offset": 1024, "length": 4096}
```
映射：`FsBackend::read_file_at(path, offset, length)`

### 7.8 `write` — 写入文件（覆盖）

请求：
```json
{
  "id": "8", "op": "write",
  "path": "/data/file.csv",
  "content": "aGVsbG8gd29ybGQ=",
  "encoding": "base64"
}
```

成功响应：
```json
{"id": "8", "ok": true, "data": {"written": 11}}
```

映射：`FsBackend::write_file(path, data)`

文件不存在则创建；已存在则覆盖。`encoding` 默认为 `"base64"`。
写入会自动创建缺失的父目录（类似 `mkdir -p`），与 `EmbeddedPageFs::write_file()` 当前实现一致。
如需获取写入后的文件大小，请再调用一次 `stat`。

大文件写入使用流式协议（见 §8）。

### 7.9 `pwrite` — 偏移写入

请求：
```json
{
  "id": "9", "op": "pwrite",
  "path": "/data/file.csv",
  "offset": 512,
  "content": "bmV3IGRhdGE=",
  "encoding": "base64"
}
```

成功响应：
```json
{"id": "9", "ok": true, "data": {"written": 8}}
```

映射：`FsBackend::write_file_at(path, offset, data)`

### 7.10 `append` — 追加写入

请求：
```json
{
  "id": "10", "op": "append",
  "path": "/data/log.txt",
  "content": "bmV3IGxpbmUK",
  "encoding": "base64"
}
```

成功响应：
```json
{"id": "10", "ok": true, "data": {"written": 9}}
```

映射：`FsBackend::append_file(path, data)`

### 7.11 `truncate` — 截断文件

请求：
```json
{"id": "11", "op": "truncate", "path": "/data/file.csv", "size": 512}
```

成功响应：
```json
{"id": "11", "ok": true, "data": {}}
```

映射：`FsBackend::truncate(path, size)`

`size` 为截断后的目标大小（字节）。`size: 0` 表示清空文件内容。

### 7.12 `rename` — 重命名/移动文件或目录

请求：
```json
{"id": "12", "op": "rename", "old_path": "/data/a.csv", "new_path": "/data/b.csv"}
```

成功响应：
```json
{"id": "12", "ok": true, "data": {}}
```

映射：`FsBackend::rename(old_path, new_path)`

语义：
- 同目录重命名和跨目录移动均为原子操作（单 TiKV 事务）。
- 目标父目录必须已存在（不自动创建，缺失时返回 `ENOENT`）。
- 目标为已有文件时，源文件替换目标文件。
- 目标为已有目录时，返回 `EEXIST`。
- 源为目录、目标为文件时，返回 `ENOTDIR`。
- 将目录移入自身子树时，返回 `EINVAL`（防止目录循环）。
- 源路径不存在时，返回 `ENOENT`。
- 重命名根路径 `/` 时，返回 `EACCES`。
- 源路径与目标路径相同时，为 no-op（直接成功）。

---

## 8. 流式传输协议

对于 ≥ 1MB 的文件，使用分块 binary frame 传输，避免单个 WebSocket 消息过大。

### 8.1 流式读取

```
Client                                    Server
  |                                         |
  |--- {"id":"r1","op":"read",              |
  |     "path":"/big.bin"}           -----> |
  |                                         |
  |<-- {"id":"r1","ok":true,"data":{        | ← 流式开始标记（服务端分配 stream_id）
  |     "streaming":true,                   |
  |     "stream_id":1,                      |
  |     "size":5242880,                     |
  |     "chunk_size":65536}} ------------- |
  |                                         |
  |<-- [Binary: 8-byte stream_id + chunk_0] | ← 64KB chunk
  |<-- [Binary: 8-byte stream_id + chunk_1] |
  |<-- ...                                  |
  |<-- [Binary: 8-byte stream_id + chunk_79]|
  |                                         |
  |<-- {"id":"r1","ok":true,"data":{        | ← 流式结束标记
  |     "stream":"end",                     |
  |     "stream_id":1,                      |
  |     "checksum":"sha256:abcdef..."}} --- |
```

### 8.2 流式写入

```
Client                                    Server
  |                                         |
  |--- {"id":"w1","op":"write",             |
  |     "path":"/big.bin",                  |
  |     "streaming":true,                   |
  |     "size":5242880}              -----> |
  |                                         |
  |<-- {"id":"w1","ok":true,"data":{        | ← 服务端就绪（分配 stream_id）
  |     "ready":true,                       |
  |     "stream_id":2,                      |
  |     "chunk_size":65536}} ------------- |
  |                                         |
  |--- [Binary: 8-byte stream_id + chunk_0] | ← 客户端发送 chunk
  |--- [Binary: 8-byte stream_id + chunk_1] |
  |--- ...                                  |
  |                                         |
  |--- {"id":"w1","stream":"end",           | ← 客户端发送结束标记
  |     "stream_id":2,                      |
  |     "checksum":"sha256:abcdef..."} ---> |
  |                                         |
  |<-- {"id":"w1","ok":true,                | ← 服务端确认
  |     "data":{"written":5242880}} ------- |
```

实现语义建议（与现有 `FsBackend` 行为对齐）：
- **流式写入在服务端内存缓冲**（累计到 `size` 或收到 `end`），在校验通过后一次性调用 `FsBackend::write_file()` 提交，避免分块多事务写入导致的“半成品文件可见”。
- 连接中断/校验失败/协议错误时，直接丢弃缓冲区并返回错误（或断开连接），**不落盘**，以匹配“中断不产生残留文件”的测试期望。

### 8.3 Binary Frame 格式

```
[8 bytes: stream_id][N bytes: chunk_data]
```

- `stream_id`：服务端在进入流式模式时分配的 `u64`（网络字节序 / big-endian），用于将 binary frame 关联到对应的流式请求；允许多个流在同一连接上交错传输
- `chunk_data`：文件数据块，默认最大 64KB

### 8.4 Checksum 验证

流式传输结束时，发送方在 `end` 帧中附带 `checksum`（`sha256:<hex>`）。接收方可选择验证：
- 校验不匹配 → 返回 `EIO` 错误
- 校验缺失 → 不验证（向后兼容）

### 8.5 流式传输配置

| 配置项 | 环境变量 | 默认值 |
|--------|---------|--------|
| Chunk 大小 | `FS9_WS_CHUNK_SIZE` | 65536（64KB） |
| 流式传输阈值 | （硬编码） | 1MB |

---

## 9. 错误码映射

WebSocket API 使用 POSIX 风格错误码，映射自 `EmbeddedFsError`（`src/extensions/fs/embedded/types.rs`）和其他运行时错误：

| 错误码 | 含义 | 映射来源 |
|--------|------|---------|
| `ENOENT` | 文件或目录不存在 | `EmbeddedFsError::NotFound` |
| `EISDIR` | 对目录执行了文件操作（如 `unlink` 目录） | `EmbeddedFsError::IsDirectory` |
| `ENOTDIR` | 对文件执行了目录操作（如 `readdir` 文件） | `EmbeddedFsError::NotDirectory` |
| `EEXIST` | 文件或目录已存在 | `EmbeddedFsError::AlreadyExists` |
| `ENOTEMPTY` | 目录非空（非递归删除时） | `EmbeddedFsError::DirectoryNotEmpty` |
| `EACCES` | 权限不足（非 superuser 或未认证） | `EmbeddedFsError::PermissionDenied` / 权限检查 |
| `EFBIG` | 文件过大（超过 `MAX_BYTES_PER_FILE` 10MB） | 大小检查 |
| `EAGAIN` | 读预算耗尽（超过 `FS9_READ_BUDGET` 128MB） | 全局读预算检查（资源暂时不可用，建议稍后重试） |
| `EAUTH` | 认证失败 | `AuthManager::authenticate` 返回 `None` |
| `EINVAL` | 无效参数（路径格式错误、缺少必须字段、目录循环等） | `EmbeddedFsError::InvalidInput` / 参数校验 |
| `EPROTO` | 协议错误（JSON 解析失败、未认证先操作、非法帧类型） | 协议校验 |
| `EIO` | 内部错误（TiKV 通信异常、checksum 不匹配等） | `EmbeddedFsError::Internal` / 运行时错误 |

---

## 10. 安全与限制

### 10.1 限制参数总表

| 限制 | 值 | 环境变量 | 说明 |
|------|---|---------|------|
| 单文件最大读写 | 10 MB | （硬编码） | 沿用 `MAX_BYTES_PER_FILE` |
| 全局并发读预算 | 128 MB | （硬编码） | 沿用 `FS9_READ_BUDGET` |
| 每租户最大 WS 连接数 | 50 | `FS9_WS_MAX_CONNECTIONS_PER_TENANT` | 超出后拒绝新连接 |
| 空闲超时 | 300s | `FS9_WS_IDLE_TIMEOUT` | 无消息交互后自动断开 |
| 认证超时 | 10s | （硬编码） | 连接后必须在 10s 内完成 auth |
| 最大 JSON 控制帧大小 | 2 MB | `FS9_WS_MAX_JSON_BYTES` | 覆盖 `< 1MB` 内联 base64 的开销；≥ 1MB 必须走流式协议 |
| 流式 chunk 大小 | 64 KB | `FS9_WS_CHUNK_SIZE` | 流式传输每块大小 |
| 最大并发 inflight 请求 | 10 | （硬编码） | 单连接上的最大并发请求数 |
| 权限要求 | superuser | （硬编码） | 与 fs9 SQL 函数一致 |

### 10.2 安全措施

1. **认证强制**：所有操作（除 `auth` 本身）必须在认证成功后执行。未认证状态下发送操作消息，返回 `EPROTO` 并立即断开连接。
2. **superuser 强制**：认证成功但非 superuser 的用户，返回 `EACCES` 错误（`"fs9: permission denied (superuser required)"`），与 fs9 SQL 函数行为一致。
3. **TLS/WSS**：密码以明文在 `auth` 消息中传输。生产环境应启用 WSS（通过 `PG_TLS_CERT`/`PG_TLS_KEY` 配置）；若监听在非 loopback 地址，服务端应默认拒绝在无 TLS 的情况下启动（除非 `DB9_DEV=1` 或 `DB9_INSECURE=1`）。
4. **路径校验**：所有路径必须为绝对路径（以 `/` 开头），不允许 `..` 路径穿越。包含 `..` 的路径返回 `EINVAL`。
5. **keyspace 隔离**：每个 WebSocket 连接绑定到一个 keyspace（通过 `TenantHandle` 持有），不同租户的文件系统数据完全隔离。
6. **连接限制**：每个租户（keyspace）最多 50 个并发 WebSocket 连接，防止资源耗尽。

---

## 11. 模块结构与实现计划

### 11.1 新增模块

```
src/extensions/fs/ws/
├── mod.rs         — WebSocket 服务入口：TCP listener、accept loop、TLS 支持
├── handler.rs     — 消息分发：JSON 解析 → 操作路由 → FsBackend 调用 → 响应构建
├── protocol.rs    — 请求/响应类型定义：WsRequest、WsResponse、WsError 等 serde 结构
├── auth.rs        — 认证逻辑：复用 AuthManager + parse_tenant_username + TikvClientPool
└── stream.rs      — 流式传输：chunked binary frame 的读写实现、checksum 校验
```

### 11.2 Cargo.toml 变更

```toml
[dependencies]
tokio-tungstenite = "0.24"
# sha2 已作为 password.rs 的依赖存在，无需新增
```

### 11.3 main.rs 变更

在 `main()` 中新增 WebSocket listener 启动逻辑（与 pgwire listener 并行，共享 `TikvClientPool`）。

### 11.4 实现阶段

| 阶段 | 范围 | 说明 |
|------|------|------|
| **P0** | 传输层 + 认证 + `stat`/`readdir`/`mkdir`/`unlink`/`rm` | 建立基础框架，验证 AuthManager 复用可行性。只读和元数据操作优先 |
| **P1** | `read`/`write`（内联 base64，< 1MB） | 文件读写核心功能。base64 编码用于 JSON 帧内传输 |
| **P2** | `pwrite`/`append`/`truncate` | 补全偏移写入、追加、截断操作 |
| **P3** | 流式传输协议（≥ 1MB） | chunked binary frame、checksum 校验、大文件读写 |
| **P4** | TLS/WSS 支持、连接池优化 | 生产级部署所需的安全和性能增强 |

---

## 12. 测试计划

### 12.1 单元测试

| 测试对象 | 位置 | 内容 |
|---------|------|------|
| 协议序列化 | `ws/protocol.rs` | `WsRequest`/`WsResponse` 的 JSON 序列化/反序列化 round-trip |
| 错误码映射 | `ws/handler.rs` | `EmbeddedFsError` → POSIX 错误码映射正确性 |
| 路径校验 | `ws/handler.rs` | 绝对路径检查、`..` 穿越拒绝、空路径处理 |
| Binary frame 解析 | `ws/stream.rs` | stream_id 提取、chunk 拼接、checksum 验证 |

### 12.2 集成测试

| 场景 | 说明 |
|------|------|
| 完整生命周期 | connect → auth → stat → write → read → unlink → close |
| 目录操作 | mkdir → readdir → rm（递归） |
| 认证失败 | 错误密码 → EAUTH；非 superuser → EACCES |
| 未认证操作 | 跳过 auth 直接发 stat → EPROTO + 断开 |
| 多租户隔离 | 租户 A 写入 `/data/a.txt`，租户 B 读取 → ENOENT |

### 12.3 流式传输测试

| 场景 | 说明 |
|------|------|
| 大文件 round-trip | 写入 5MB 文件（流式） → 读取（流式） → 内容一致 |
| Checksum 验证 | 发送错误 checksum → EIO 错误 |
| 中断恢复 | 流式传输中途断开 → 不产生残留文件 |

### 12.4 安全测试

| 场景 | 说明 |
|------|------|
| 认证超时 | 连接后不发 auth，10s 后自动断开 |
| 空闲超时 | 认证后不操作，300s 后自动断开 |
| 连接限制 | 超过 50 个并发连接 → 拒绝新连接 |
| 路径穿越 | `"path": "/../etc/passwd"` → EINVAL |
| 文件大小限制 | 写入 > 10MB 文件 → EFBIG |

### 12.5 兼容性测试

| 场景 | 说明 |
|------|------|
| pgwire 共存 | WebSocket 和 pgwire 同时运行，互不干扰 |
| fs9 SQL 一致性 | WebSocket 写入的文件可通过 `fs9_read()` SQL 函数读取，反之亦然 |
| 多客户端并发 | 多个 WebSocket 客户端并发读写同一文件，数据一致 |

---

## 附录 A：操作与 FsBackend 方法映射总表

| WebSocket 操作 | FsBackend 方法 | 请求关键参数 | 响应关键字段 |
|---------------|---------------|-------------|-------------|
| `auth` | `AuthManager::authenticate` | `username`, `password` | `user`, `tenant`, `keyspace` |
| `stat` | `stat(path)` | `path` | `path`, `type`, `size`, `mode`, `mtime` |
| `readdir` | `readdir(path)` | `path` | `entries[]` |
| `mkdir` | `mkdir(path, recursive)` | `path`, `recursive?` | — |
| `unlink` | `remove(path)` | `path` | — |
| `rm` | `remove(path)` / `remove_recursive(path)` | `path`, `recursive?` | `removed?` |
| `read` | `read_file(path, max)` / `read_file_at(path, off, len)` | `path`, `offset?`, `length?` | `content`, `size`, `encoding` |
| `write` | `write_file(path, data)` | `path`, `content`, `encoding?` | `written` |
| `pwrite` | `write_file_at(path, offset, data)` | `path`, `offset`, `content` | `written` |
| `append` | `append_file(path, data)` | `path`, `content` | `written` |
| `truncate` | `truncate(path, size)` | `path`, `size` | — |
| `rename` | `rename(old_path, new_path)` | `old_path`, `new_path` | — |
