# 扩展机制与 HTTP 扩展（Supabase 风格）设计

## 摘要

本文设计一个适用于分布式、多租户（TiKV keyspace 隔离）的 `db9-server` 扩展机制，并在该机制之上实现一个类似 Supabase `http` 扩展（基于 `pgsql-http` 语义）的 HTTP 客户端扩展。

核心原则：
- **扩展代码内置（编译进二进制）**：不做运行时动态加载第三方二进制。
- **扩展启用状态按租户持久化**：存储在 TiKV 的 `_sys_*` 元数据键中，天然 keyspace 隔离。
- **最小侵入的执行挂钩**：只在现有的 SQL 执行分发点加入扩展路由，不引入复杂抽象。
- **安全与稳定优先**：网络能力默认仅限超级用户；强制超时、并发与响应大小限制；默认防 SSRF。

---

## 背景与现状（当前代码路径）

### 1) 语句入口与“非 sqlparser 语法”拦截

`Executor::execute()` 在解析 AST 前，会用字符串前缀匹配拦截一部分语句（用于 `CREATE FUNCTION/TRIGGER` 等自定义解析），位置：
- `src/sql/executor.rs:177`（见 `starts_with("CREATE FUNCTION")` 等分支）

这提供了扩展 DDL（`CREATE EXTENSION` / `DROP EXTENSION`）的自然落点：无需依赖 `sqlparser` 对扩展语法的支持。

### 2) 表达式函数执行的两条路径

当前函数相关机制大致分为：
- **纯内置函数（同步）**：`Expr::Function` → `eval_function()`（`src/sql/expr.rs:1601` 起）
- **需要异步重写的函数（序列/用户函数）**：先在表达式层做 rewrite，再用同步 evaluator 计算
  - “是否需要异步”的判定：`src/sql/sequences.rs:66`（`expr_needs_async_eval()`）
  - 重写逻辑包含：序列函数、以及“可能是用户函数”的调用（`src/sql/sequences.rs:728` 起）
  - 用户函数执行入口：`src/sql/plpgsql.rs:789`（`try_execute_user_function()`）

这意味着：**若要支持扩展的“标量函数”**，可以复用 `sequences.rs` 的 rewrite 流程，在用户函数之后加入扩展函数分发（可选 v1.1）。

### 3) `FROM` 子句中的函数/虚拟表

当前 `FROM` 的特殊处理：
- `generate_series`：已作为 SRF 支持（非 JOIN 路径 `src/sql/executor_select.rs:147`；JOIN 路径 `src/sql/executor_join.rs:585`）
- 少数“标量当表用”的函数：JOIN 路径把 `FROM current_schema()` 当成一个单行单列的虚拟表（`src/sql/executor_join.rs:366` 起）
- 可观测性虚拟表：`_db9_sys_observability` / `_db9_sys_query_samples`（`src/sql/executor_join.rs:59` 起）

但目前 JOIN 路径存在一个关键限制：只要 `args.is_some()` 就会被当成“标量函数”，并把 `FROM f(a,b)` 退化成 `f()` 丢弃参数（`src/sql/executor_join.rs:597` 和 `src/sql/executor_join.rs:605`）。

结论：要做 Supabase 风格的 `extensions.http_get('...')`（表函数返回一行多列），必须引入**带参数的 Table Function 分发**，并修复 JOIN 路径对参数的丢弃。

### 4) 类型系统限制：缺少“复合类型值”

`Value` 只支持标量/数组（数组元素也是 `Value`），不支持 record/tuple 值（`src/model/mod.rs:228`）。

因此类似 `pgsql-http` 的 `http_header[]`（数组元素为复合类型 `(field,value)`）在现有 Value 模型下无法自然表达。

结论：HTTP 扩展返回值需要采用 `jsonb` 等“可表达结构化数据”的列类型，或把 headers 平铺成多行（但 Supabase 示例是单行 record）。本文选择：**headers 以 JSONB 数组返回**，保持一行结果且可保留重复 header。

---

## 目标与非目标

### 目标（MVP）

1. 支持按租户启用/禁用扩展：
   - `CREATE EXTENSION [IF NOT EXISTS] extname`
   - `DROP EXTENSION [IF EXISTS] extname`
2. 扩展可提供：
   - **表函数（Table Function）**：`FROM extensions.http_get(...)`
   - （可选 v1.1）标量函数：在表达式中调用
3. 扩展安装状态、配置 **keyspace 隔离** 持久化到 TiKV。
4. `http` 扩展在启用后提供 Supabase 风格的 `http_*` 表函数，并具备生产级安全与稳定限制。

### 非目标（v1 不做）

- 运行时加载外部二进制/脚本（Postgres C 扩展 ABI 兼容）。
- 完整的 Postgres `pg_extension` 依赖/升级脚本体系。
- 在表达式里返回/传递真正的复合类型（需要扩展 `Value` 体系）。

---

## 总体方案

### 核心思路：内置扩展 + 租户安装态

```
           +--------------------+
SQL Client | pgwire / Executor  |
---------->+--------------------+
                    |
                    v
           +--------------------+
           | ExtensionManager   |
           |  - registry        |
           |  - install state   |
           |  - policy/limits   |
           +---------+----------+
                     |
          +----------+-----------+
          |                      |
          v                      v
   +--------------+     +------------------+
   | TiKV (_sys_) |     | Built-in Exts    |
   |  _sys_ext_*  |     |  http, ...       |
   +--------------+     +------------------+
```

关键点：
- **registry（可用扩展列表）**：编译期静态存在，不随租户变化。
- **install state（已安装扩展）**：按租户持久化在 TiKV `_sys_ext_*`。
- **policy/limits（安全与限流）**：执行时由 ExtensionManager 统一校验。

---

## 扩展机制设计（Core）

### 1) 扩展注册表（Available Extensions）

在二进制内维护一个静态 registry（概念性结构）：

- `ExtensionDescriptor`：
  - `name`: 例如 `"http"`
  - `version`: 例如 `"1.0.0"`
  - `default_schema`: 例如 `"extensions"`
  - `capabilities`: 例如 `[NetworkEgress]`
  - `table_functions`: 函数签名与实现入口
  - （可选）`scalar_functions`

说明：registry **不是** per-tenant；它描述“二进制支持哪些扩展”，而不是“租户装了哪些扩展”。

### 2) 扩展安装态持久化（Per-tenant Catalog）

新增系统键前缀（与现有 `_sys_*` 风格一致，位置：`src/storage/encoding.rs:18`）：

- `_sys_ext_{extname}` → `InstalledExtension`（bincode）
- `_sys_extcfg_{extname}` → `ExtensionConfig`（json/bincode）

`InstalledExtension`（概念字段）：
- `name`, `version`, `schema`
- `installed_at_ms`
- `enabled: bool`

关键约束：所有 TiKV key 都必须通过 `TikvStore` 写入，以保证 keyspace 隔离（见项目的多租户原则）。

### 3) DDL：CREATE/DROP EXTENSION

采用“字符串拦截 + 手写解析”的方式（沿用 `CREATE FUNCTION` 的模式）：
- 拦截点：`src/sql/executor.rs:177`（在 `parse_sql(sql)` 之前）

语义（MVP）：
- `CREATE EXTENSION http`：
  - 仅超级用户允许（与 `Session::is_superuser()` 一致）
  - 写入 `_sys_ext_http`
  - 确保 schema `extensions` 可用（建议作为内置 schema，见下文）
- `DROP EXTENSION http`：
  - 仅超级用户允许
  - 删除 `_sys_ext_http` 和 `_sys_extcfg_http`

事务性：在同一个 TiKV txn 中执行，保证安装态变更与其它 DDL 一致提交/回滚。

### 4) 运行时分发：Table Function（MVP 必需）

**目标**：支持 `FROM extensions.http_get('...')`，返回一张虚拟表（单行多列）。

必须修改两个路径：

1) 非 JOIN 路径：
   - 入口：`src/sql/executor_select.rs:140`（`TableFactor::Table { name, args, .. }`）
   - 现状：只特殊处理 `generate_series`，其它默认按 table/view 查找
   - 方案：若 `args.is_some()`，尝试作为 **table function** 分发（先扩展，再退回 table lookup）。

2) JOIN 路径：
   - 入口：`src/sql/executor_join.rs:563`（`resolve_table_factor()`）
   - 现状：`args.is_some()` 会导致 `table_name = format!(\"{}()\", obj_name)` 丢参（`src/sql/executor_join.rs:597` / `src/sql/executor_join.rs:605`）
   - 方案：把 `name + args` 当作 table function 调用分发；不再把“有参数”统一视为标量函数。

统一抽象（概念）：
- `ExtensionTableFunction::execute(ctx, args) -> (TableSchema, Vec<Row>)`
- `ctx` 包含：tenant、用户、是否超级用户、语句级限制等

### 5) （可选 v1.1）标量函数分发：表达式 rewrite

若需要在表达式中调用扩展标量函数，建议复用现有异步 rewrite 框架：
- 位置：`src/sql/sequences.rs:728`（在 `plpgsql::try_execute_user_function()` 之后增加扩展 lookup）

注意：标量扩展函数如果涉及 I/O（HTTP），会把网络等待嵌入 SQL 执行路径；除非有强限制，否则不建议在 v1 直接开放。

### 6) schema：`extensions` 命名空间

为贴近 Supabase 体验，HTTP 扩展默认暴露在 `extensions` schema 下：

建议把 `extensions` 作为**内置 schema**，避免安装时额外写 schema 元数据：
- `TikvStore::is_builtin_schema`：`src/storage/tikv_store.rs:169`
- `list_schema_oids`：`src/storage/tikv_store.rs:213`（为 `extensions` 分配稳定 OID）

### 7) 系统目录可见性（最低限度）

为兼容工具链，可增加：
- `pg_extension` 虚拟表（在 `src/sql/information_schema.rs` 的 catalog 映射中注册；参考 `pg_proc` / `pg_type` 的实现）
- `pg_proc` 里暴露已安装扩展的函数（类似现有 builtin set + user functions，位置：`src/sql/information_schema.rs:2222`）

---

## HTTP 扩展设计（基于上述机制）

### 1) SQL API（Supabase 风格）

表函数（返回 1 行）：
- `extensions.http_get(url text)`
- `extensions.http_post(url text, body text, content_type text)`
- `extensions.http_put(url text, body text, content_type text)`
- `extensions.http_delete(url text)`（MVP 只做无 body 版本）
- `extensions.http_head(url text)`

（可选增强）统一入口：
- `extensions.http(req jsonb)`：支持 method/headers/body/timeouts 等

### 2) 返回结构（无复合类型的实现方式）

返回列（与 Supabase 文档字段对齐，但 headers 用 jsonb）：
- `status`：`INT`
- `content_type`：`TEXT`（可空）
- `headers`：`JSONB`（形如 `[{ \"field\": \"...\", \"value\": \"...\" }, ...]`，保留重复 header）
- `content`：`TEXT`（UTF-8）

理由：当前 `Value` 不支持 `http_header[]` 的“数组元素为 tuple”的表达（见 `src/model/mod.rs:228`），用 JSONB 最直接且兼容 SQL 侧解析。

---

## 安全与稳定（HTTP）

MVP 必须包含以下保护（避免把数据库变成 SSRF/DoS 工具）：

1. **权限**：仅超级用户可执行（默认禁止普通用户出网）。
2. **协议与端口**：仅允许 `https`，默认仅允许 443。
3. **DNS/IP 过滤**：
   - 禁止直连内网/loopback/link-local/unspecified
   - 域名解析结果若落到上述网段，同样禁止（防 DNS rebinding）
4. **超时**：
   - connect timeout（例如 1s）
   - total timeout（例如 5s）
5. **大小限制**：
   - request body max（例如 256KiB）
   - response max（例如 1MiB）
6. **并发与次数限制**：
   - per-tenant in-flight 请求数限制（例如每节点 20）
   - 每条语句最多允许 N 次 HTTP 请求（例如 5）
7. **重定向限制**：
   - 最大重定向次数（例如 3）
   - HEAD 默认不跟随重定向

---

## 测试计划（MVP）

1. 单元测试：
   - `CREATE EXTENSION` / `DROP EXTENSION` 的解析覆盖：大小写、`IF [NOT] EXISTS`、schema 前缀等。
   - SSRF：`is_ip_forbidden()` 等纯逻辑函数。
2. 集成测试（SQL）：
   - `CREATE EXTENSION http;` 后 `SELECT ... FROM extensions.http_get(...)` 返回列完整、可 `content::jsonb`。
   - 未安装扩展调用时报错。
   - 非超级用户调用报错。
   - SSRF：对 `https://127.0.0.1`、`https://localhost` 等报错。
3. ORM/工具链：
   - `SELECT * FROM pg_extension` 能返回已安装扩展记录。
   - `pg_proc` 中存在扩展函数行，满足 introspection join。

