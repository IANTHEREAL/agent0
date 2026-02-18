# 统一异步任务引擎设计

## 状态

- **阶段**：设计中
- **作者**：pg-tikv team
- **日期**：2026-02-18
- **关联文件**：`src/cron/`、`src/sql/triggers/`、`src/sql/executor/`、`src/storage/tikv_store/`

---

## 问题陈述

pg-tikv 当前有多个独立的异步执行系统，各自为政，存在以下问题：

### 现状分析

| 系统 | 实现位置 | 问题 |
|------|--------|------|
| **Cron** | `src/cron/worker.rs` | OnceLock 单例（line 506），每 60s 全量扫描所有 keyspace，串行执行，无法水平扩展 |
| **Async Trigger** | `src/sql/triggers/worker.rs` | 独立 OnceLock（line 553），DashMap 内存队列，进程重启丢失，无持久化 |
| **ANALYZE** | `src/sql/executor/core/analyze.rs` | 同步单事务（line 25），10K 批流式扫描（line 316），阻塞用户查询 |
| **CREATE INDEX** | `src/sql/ddl.rs:1208-1514` | 同步回填，无 CONCURRENTLY 支持，长时间锁表 |
| **REFRESH MATERIALIZED VIEW** | `src/sql/executor/procedure.rs:704` | 同步执行，无后台选项 |

### 规模假设

```
总 tenant:                1,000,000
启用 cron 的 tenant:       10,000   (1%)
启用 async trigger 的 tenant: 50,000 (5%)
总 cron job:              20,000   (avg 2/tenant)
总 async trigger:         100,000  (avg 2/tenant)
峰值 due job/分钟:        200-500
峰值 async trigger/秒:    50-200
pg-tikv 实例:             10+
```

### 核心痛点

1. **多个独立系统** — 无法统一管理、监控、扩展
2. **内存队列** — 进程重启丢失任务，无持久化保证
3. **单点故障** — OnceLock 单例无法水平扩展
4. **扫描效率低** — O(keyspace × db × job)，百万级 tenant 下不可接受
5. **阻塞用户** — ANALYZE、CREATE INDEX 同步执行，影响 OLTP 性能
6. **无优先级** — 所有任务平等对待，无法区分付费 tier

---

## 设计概览

### 核心思路

**统一异步任务引擎**：用 TiKV 本身做协调层，不引入外部依赖。所有异步任务（cron、async trigger、auto-ANALYZE、CREATE INDEX CONCURRENTLY、background SQL）共用一套 queue + claim + execute 机制。

```
                           ┌─────────────────────────┐
                           │      TiKV Cluster        │
                           │                         │
                           │  ┌───────────────────┐  │
                           │  │ Global Task Queue  │  │
                           │  │ (sorted by         │  │
                           │  │  next_fire_time)   │  │
                           │  └─────────┬─────────┘  │
                           │            │             │
                           │  ┌─────────┴─────────┐  │
                           │  │ Task Registry      │  │
                           │  │ (keyspace→tasks)   │  │
                           │  └───────────────────┘  │
                           └────────────┬────────────┘
                                        │
                    ┌────────────────────┬┴─────────────────────┐
                    │                    │                      │
              ┌─────┴─────┐       ┌─────┴─────┐       ┌─────┴─────┐
              │  Worker 1  │       │  Worker 2  │       │  Worker N  │
              │            │       │            │       │            │
              │ 1. scan    │       │ 1. scan    │       │ 1. scan    │
              │    queue   │       │    queue   │       │    queue   │
              │ 2. claim   │       │ 2. claim   │       │ 2. claim   │
              │    (txn)   │       │    (txn)   │       │    (txn)   │
              │ 3. execute │       │ 3. execute │       │ 3. execute │
              │    via     │       │    via     │       │    via     │
              │    pgwire  │       │    pgwire  │       │    pgwire  │
              └─────┬──────┘       └─────┬──────┘       └──────┬────┘
                    │                    │                     │
                    └────────────────────┼─────────────────────┘
                                         │ pgwire (SQL)
                    ┌────────────────────┼─────────────────────┐
                    │                    │                     │
              ┌─────┴─────┐       ┌─────┴─────┐       ┌───────┴───┐
              │ pg-tikv 1  │       │ pg-tikv 2  │       │ pg-tikv M  │
              │ (SQL only) │       │ (SQL only) │       │ (SQL only) │
              └────────────┘       └────────────┘       └───────────┘
```

### 关键决策

1. **Worker 通过 pgwire 连回 pg-tikv** 执行 SQL，不直接操作 TiKV 数据。保证完整 SQL 语义（事务、trigger、权限检查、search_path）。
2. **TiKV 悲观事务做 claim**，天然分布式锁，不需要额外的 leader election。
3. **同一套引擎服务所有异步任务**，通过 TaskType 区分。

---

## 任务类型

### TaskType 枚举

```rust
enum TaskType {
    Cron,           // 定时 SQL（cron.schedule）
    AsyncTrigger,   // AFTER trigger 异步执行
    AutoAnalyze,    // 自动 ANALYZE（表修改超过阈值）
    BgDdl,          // CREATE INDEX CONCURRENTLY、REFRESH MATERIALIZED VIEW
    BgSql,          // 用户提交的一次性后台任务（future）
}
```

### 各任务类型的入队方式

| TaskType | 入队时机 | next_fire_time | 优先级 |
|----------|--------|----------------|--------|
| **Cron** | `cron.schedule()` 时 | 根据 cron 表达式计算 | 低 |
| **AsyncTrigger** | trigger fire 时 | `now` | 高 |
| **AutoAnalyze** | 表修改计数超过阈值 | `now` | 中 |
| **BgDdl** | `CREATE INDEX CONCURRENTLY` / `REFRESH MATERIALIZED VIEW` 时 | `now` | 中 |
| **BgSql** | `SELECT pg_background_launch(...)` 时 | `now` | 低 |

---

## 详细设计

### 1. 全局任务注册表（Task Registry）

**问题**：当前需要扫描每个 keyspace 的每个 db 才能知道哪里有任务。

**方案**：在系统 keyspace（`_sys_worker`，不属于任何 tenant）中维护全局注册表。

#### 注册表 key layout

```
_worker_registry_{keyspace}_{db_id}  →  TaskRegistryEntry (bincode)
```

```rust
struct TaskRegistryEntry {
    keyspace: String,
    db_id: u64,
    task_types: u8,       // bitmask: 0x01=cron, 0x02=async_trigger, 0x04=auto_analyze, 0x08=bg_ddl, 0x10=bg_sql
    job_count: u32,       // hint for load balancing, 不需要精确
    registered_at: i64,   // epoch ms
}
```

#### 注册/注销时机

| 操作 | 动作 |
|------|------|
| `CREATE EXTENSION pg_cron` | 写入 registry entry（`task_types \|= 0x01`） |
| `DROP EXTENSION pg_cron` | 清除 cron bit；若 `task_types == 0` 则删除 entry |
| `cron.schedule()` | 更新 `job_count += 1` |
| `cron.unschedule()` | 更新 `job_count -= 1`；若 `job_count == 0` 清除 cron bit |
| 首次 async trigger 创建 | 写入 registry entry（`task_types \|= 0x02`） |
| 最后一个 async trigger 删除 | 清除 async_trigger bit |
| 表首次启用 auto-ANALYZE | 写入 registry entry（`task_types \|= 0x04`） |
| `CREATE INDEX CONCURRENTLY` | 写入 registry entry（`task_types \|= 0x08`） |

#### 影响文件

- `src/sql/executor/cron.rs`：schedule/unschedule 时写 registry
- `src/sql/triggers/enqueue.rs`：首次 async trigger 时写 registry
- `src/sql/executor/core/analyze.rs`：auto-ANALYZE 启用时写 registry
- `src/sql/ddl.rs`：CREATE INDEX CONCURRENTLY 时写 registry
- `src/extensions/mod.rs`：CREATE/DROP EXTENSION 时写 registry
- `src/storage/tikv_store/worker.rs`：新增 registry 读写方法

#### 扫描代价

Worker tick 时只需 **一次 prefix scan** `_worker_registry_` → 拿到所有有任务的 keyspace 列表。1 万条 entry ≈ 1MB，单次 scan < 50ms。

---

### 2. 预计算 Next-Fire-Time 索引

**问题**：当前每 tick 对每个 job 解析 cron 表达式并检查 `is_due()`。

**方案**：每个任务维护一个 `next_fire_time`，写入全局有序队列。tick 时只 range scan `fire_time <= now`。

#### 队列 key layout

```
_worker_queue_{next_fire_time_ms}_{keyspace}_{db_id}_{task_id}  →  TaskQueueEntry (bincode)
```

`next_fire_time_ms` 使用 **big-endian i64**，自然有序。

```rust
struct TaskQueueEntry {
    keyspace: String,
    db_id: u64,
    task_id: i64,           // job_id / trigger_id / table_id / index_id
    task_type: TaskType,    // Cron | AsyncTrigger | AutoAnalyze | BgDdl | BgSql
    command: String,        // SQL to execute
    username: String,       // 执行身份
    schedule: Option<String>, // cron 表达式（仅 Cron 类型）
    priority: u8,           // 0-255，高优先级先执行
}
```

#### 写入时机

| 操作 | 动作 |
|------|------|
| `cron.schedule()` | 计算 next_fire_time，写入 queue |
| job 执行完成后 | 计算下一次 next_fire_time，写入新 queue entry，删除旧 entry |
| `cron.alter_job()` 修改 schedule | 删除旧 entry，计算新 next_fire_time，写入新 entry |
| `cron.unschedule()` | 删除 queue entry |
| trigger fire（async） | 写入 queue entry（`next_fire_time = now`） |
| 表修改计数超过阈值 | 写入 queue entry（`next_fire_time = now`） |
| `CREATE INDEX CONCURRENTLY` | 写入 queue entry（`next_fire_time = now`） |
| `REFRESH MATERIALIZED VIEW CONCURRENTLY` | 写入 queue entry（`next_fire_time = now`） |

#### Tick 流程（新）

```
every tick_interval:
  now = current_time_ms()
  entries = range_scan("_worker_queue_" .. "_worker_queue_{now}")
  // 只返回 fire_time <= now 的 entry，O(due_jobs)
  // 按 priority 排序
  for entry in entries:
    spawn_claim_and_execute(entry)
```

#### 扫描代价

峰值 500 due jobs/分钟 + 50-200 async trigger/秒 → 每次 scan 最多返回 ~1000 条，< 500KB。

#### 影响文件

- `src/storage/tikv_store/worker.rs`：新增 queue 读写方法
- `src/sql/executor/cron.rs`：schedule/unschedule/alter_job 时维护 queue
- `src/sql/triggers/enqueue.rs`：trigger fire 时写入 queue
- `src/sql/executor/core/analyze.rs`：auto-ANALYZE 时写入 queue
- `src/sql/ddl.rs`：CREATE INDEX CONCURRENTLY 时写入 queue
- `src/cron/worker.rs`：tick 改为 queue range scan

---

### 3. 多实例 Work Stealing

**问题**：单实例单例，无法水平扩展。

**方案**：所有 worker 实例平等，通过 TiKV 悲观事务竞争 claim。

#### Claim 机制

```
_worker_claim_{keyspace}_{db_id}_{task_id}_{fire_time_min}  →  WorkerClaim
```

```rust
struct WorkerClaim {
    worker_id: String,     // 实例标识（hostname:pid 或 UUID）
    claimed_at: i64,       // epoch ms
    task_type: TaskType,   // 用于 GC 时区分
}
```

#### 竞争流程

```rust
async fn try_claim(txn, entry) -> bool {
    let key = claim_key(entry.keyspace, entry.db_id, entry.task_id, fire_minute);
    if txn.get(key).await?.is_some() {
        return false;  // 已被其他 worker claim
    }
    txn.put(key, serialize(WorkerClaim { ... }));
    true  // TiKV 悲观事务保证只有一个 writer 成功
}
```

多个 worker 同时 scan 到同一批 due tasks，各自尝试 claim。TiKV 悲观事务保证同一个 key 只有一个 writer 成功 commit，其余自动 abort。**不需要 leader election。**

#### 负载均衡

每个 worker 维护本地 `active_jobs` 计数器（AtomicU32）。当 `active_jobs >= max_concurrent_jobs` 时跳过本轮 claim，让其他 worker 接手。

无需复杂的分片或一致性哈希 — 随机竞争 + 背压足以在 10-50 个 worker 间均匀分布。

#### 影响文件

- `src/cron/worker.rs`：去掉 `OnceLock` 单例，改为可配置启动
- `src/cron/config.rs`：新增 `worker_id` 配置
- `src/sql/triggers/worker.rs`：同样改造

---

### 4. 并发执行

**问题**：串行执行 due tasks。

**方案**：`tokio::JoinSet` + `Semaphore` 控制并发。

```rust
let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs)); // 默认 32
let mut join_set = JoinSet::new();

for entry in due_entries {
    let permit = semaphore.clone().acquire_owned().await?;
    join_set.spawn(async move {
        let _permit = permit;  // 持有 permit 直到 job 完成
        claim_and_execute(entry).await
    });
}

// 等待所有 job 完成（或超时）
while let Some(result) = join_set.join_next().await {
    handle_result(result);
}
```

#### 执行路径：通过 pgwire 连回 pg-tikv

```rust
async fn execute_task_sql(entry: &TaskQueueEntry) -> Result<()> {
    // 连接到对应 tenant 的 pg-tikv
    let connstr = format!(
        "host={} port={} user={}.{} password={} dbname=postgres",
        pg_host, pg_port, entry.keyspace, entry.username, service_password
    );
    let (client, conn) = tokio_postgres::connect(&connstr, NoTls).await?;
    tokio::spawn(conn);
    
    // 设置执行超时（可配置）
    client.execute("SET statement_timeout = ?", &[&config.statement_timeout_ms]).await?;
    
    client.simple_query(&entry.command).await?;
    Ok(())
}
```

**为什么走 pgwire 而不是直接操作 TiKV**：
- 保证完整 SQL 语义（事务、trigger、权限检查、search_path）
- Worker 无需理解 pg-tikv 内部状态，纯无状态
- 可以用标准连接池（deadpool-postgres）管理连接
- 支持 statement_timeout、search_path 等 SQL 参数

#### 影响文件

- `src/cron/worker.rs`：执行路径从 `Executor::execute_statement_on_txn` 改为 pgwire 连接
- `Cargo.toml`：如果 worker 独立 binary 则新增 crate

---

### 5. Claim 生命周期

**问题**：claim key 无限累积。

**方案**：双重清理 — 执行完成后立即删除 + GC 兜底。

#### 正常流程

```
task 执行完成 → 写入 run_details → 删除 claim key → 写入下一次 queue entry（仅 Cron）
```

一个成功的 task 执行后 claim key 立即被删除，不累积。

#### 异常流程（orphan）

Worker crash 或 task 超时 → claim key 残留。GC 处理：

```
GC 每 10 分钟:
  scan _worker_claim_ prefix
  for claim in claims:
    if claim.claimed_at < now - orphan_timeout:
      delete claim
      // 不重新入队 — 等下一次 fire time 自然触发（Cron）
      // 或标记为 failed（AsyncTrigger/BgDdl/BgSql）
```

#### 代价估算

正常情况下 claim key 存活时间 < 1 分钟。峰值 1000 task/min → 瞬时最多 1000 个 claim key。GC 只需处理异常残留。

---

### 6. GC 优化

**问题**：当前 GC 全量扫描所有 keyspace。

**方案**：

1. **走注册表**：GC 只扫描 `_worker_registry_` 中的 keyspace
2. **分批处理**：每个 GC 周期处理一批（如 100 个 keyspace），下次继续
3. **Jitter**：每个 worker 的 GC 定时器加随机偏移（0-60s），避免同时执行
4. **Run retention 就地删除**：run 数据按 `_sys_cron_run_{db_id}_{run_id}` 存储，`run_id` 单调递增。保留最近 N 条只需 scan + 删除前缀。

```rust
async fn gc_tick(&self, cursor: &mut Option<String>) {
    let batch = scan_registry(cursor, 100);
    for entry in batch {
        gc_keyspace(entry.keyspace, entry.db_id).await;
    }
    if batch.is_empty() {
        *cursor = None;  // 重新开始
    }
}
```

---

### 7. Auto-ANALYZE 设计

**问题**：ANALYZE 同步执行，阻塞用户查询。

**方案**：

1. **修改计数器**：每个表维护 `mod_since_analyze` 计数器（原子操作）
2. **阈值判断**：`mod_since_analyze > 50 + 0.1 × reltuples` 时触发
3. **异步入队**：写入 queue entry（`next_fire_time = now`，`task_type = AutoAnalyze`）
4. **后台执行**：worker 执行 `ANALYZE table_name`

#### 影响文件

- `src/sql/executor/dml_analyzed.rs`：INSERT/UPDATE/DELETE 时递增 `mod_since_analyze`
- `src/sql/executor/core/analyze.rs`：ANALYZE 完成后重置计数器
- `src/sql/stats.rs`：TableStatsCache 新增 `mod_since_analyze` 字段

---

### 8. CREATE INDEX CONCURRENTLY 设计

**问题**：CREATE INDEX 同步回填，长时间锁表。

**方案**：两阶段执行

#### Phase 1：同步（schema 注册）

```sql
CREATE INDEX CONCURRENTLY idx_name ON table_name (col);
```

1. 创建 index 元数据（`_sys_index_{db_id}_{index_id}`）
2. 设置 `index_state = BUILDING`
3. 返回成功给用户

#### Phase 2：异步（后台回填）

1. Worker 从 queue 中取出 `BgDdl` 任务
2. 执行 `REINDEX INDEX idx_name` 或等价的回填逻辑
3. 完成后设置 `index_state = READY`

#### 新增类型

```rust
enum IndexState {
    Building,   // 正在回填
    Ready,      // 可用
    Invalid,    // 需要重建
}
```

#### 影响文件

- `src/sql/ddl.rs`：CREATE INDEX CONCURRENTLY 改为两阶段
- `src/sql/catalog/`：index 虚拟表返回 `index_state`
- `src/sql/executor/core/index.rs`：新增 backfill 逻辑

---

### 9. 新 Key Layout（完整）

#### 系统 keyspace（`_sys_worker`，跨 tenant）

```
_worker_registry_{keyspace}_{db_id}                              → TaskRegistryEntry
_worker_queue_{next_fire_time_ms}_{keyspace}_{db_id}_{task_id}   → TaskQueueEntry
_worker_claim_{keyspace}_{db_id}_{task_id}_{fire_time_min}       → WorkerClaim
```

#### Tenant keyspace（保持不变）

```
_sys_cron_enabled_{db_id}                     → [1]
_sys_cron_job_{db_id}_{job_id}                → CronJob
_sys_cron_run_{db_id}_{run_id}                → CronRun
_sys_next_cron_job_id_{db_id}                 → i64
_sys_next_cron_run_id_{db_id}                 → i64
```

注意：`_sys_cron_claim_` 前缀**废弃**，claim 迁移到系统 keyspace。

---

## 部署模型

### Phase 1：进程内 worker（当前架构改进）

```
pg-tikv process
├── SQL handler (pgwire)
├── CronWorker (改用 queue scan + concurrent execution)
└── TriggerWorker (保持独立，内存队列 → TiKV queue)
```

- 不拆分进程，最小改动
- 仍然走进程内 `Executor` 执行 SQL
- `list_all_keyspaces()` 改为 scan registry
- 加并发执行
- 触发器队列从内存迁移到 TiKV

### Phase 2：独立 worker 微服务

```
pg-tikv process       ← 只做 SQL
worker process(es)    ← 独立 binary，可独立扩缩
```

- Worker 通过 pgwire 连回 pg-tikv
- Worker 可以用 Kubernetes HPA 按 queue 深度自动扩缩
- pg-tikv 不再内置 `CronWorker`（配置 `PGTIKV_CRON_ENABLED=false`）

### Phase 3：统一异步任务引擎

```
TaskQueueEntry.task_type:
  Cron          → 定时 SQL
  AsyncTrigger  → AFTER trigger 异步执行
  AutoAnalyze   → 自动 ANALYZE
  BgDdl         → CREATE INDEX CONCURRENTLY / REFRESH MATERIALIZED VIEW
  BgSql         → 用户提交的一次性后台任务
```

同一套 queue + claim + execute 机制，不同 task_type 只是入队方式不同。

---

## 迁移方案（零停机）

### Step 1：添加 registry + queue（写双份）

1. 部署新版本 pg-tikv
2. `cron.schedule()` 同时写 tenant keyspace 的 job **和** 系统 keyspace 的 queue entry
3. Worker 仍然从 `list_all_keyspaces()` 扫描（旧路径），同时 **也** 从 queue 扫描（新路径）
4. 对两个来源 dedup（claim key 相同）

### Step 2：回填存量数据

后台任务扫描所有 keyspace（一次性），将已有的 cron job 写入 registry + queue：

```sql
-- 管理员手动触发或自动执行
SELECT _sys_backfill_cron_registry();
```

### Step 3：切换到新路径

1. 确认 queue 中的 job 覆盖所有 tenant（对比 registry entry count vs 旧路径 scan count）
2. 配置 `PGTIKV_CRON_USE_QUEUE=true`，worker 只从 queue 读取
3. 观察 1-2 天确认稳定

### Step 4：清理旧路径

1. 删除 `list_all_keyspaces()` 扫描逻辑
2. 删除 `_sys_cron_claim_` 前缀（迁移到系统 keyspace）
3. 保留 tenant keyspace 的 job/run 数据（cron.job / cron.job_run_details 虚拟表仍需要）

---

## 配置

| 环境变量 | 默认值 | 说明 |
|----------|--------|------|
| `PGTIKV_WORKER_ENABLED` | `true` | 是否启动进程内 worker |
| `PGTIKV_WORKER_POLL_MS` | `60000` | tick 间隔 |
| `PGTIKV_WORKER_MAX_CONCURRENT_JOBS` | `32` | 单 worker 最大并发执行数 |
| `PGTIKV_WORKER_ID` | `{hostname}:{pid}` | Worker 实例标识 |
| `PGTIKV_WORKER_STATEMENT_TIMEOUT_MS` | `300000` | 单个 task 执行超时（5 分钟） |
| `PGTIKV_WORKER_ORPHAN_TIMEOUT_SEC` | `300` | orphan claim 超时 |
| `PGTIKV_WORKER_GC_BATCH_SIZE` | `100` | 每轮 GC 处理的 keyspace 数 |
| `PGTIKV_WORKER_PG_HOST` | （同 pg-tikv） | Phase 2：worker 连回的 pg-tikv 地址 |
| `PGTIKV_WORKER_PG_PORT` | （同 pg-tikv） | Phase 2：worker 连回的 pg-tikv 端口 |
| `PGTIKV_AUTO_ANALYZE_ENABLED` | `true` | 是否启用自动 ANALYZE |
| `PGTIKV_AUTO_ANALYZE_THRESHOLD` | `50` | 自动 ANALYZE 基础阈值 |

---

## 故障模式

| 故障 | 影响 | 恢复 |
|------|------|------|
| Worker crash | 已 claim 的 task 不会执行 | orphan GC 在 5 分钟后清理 claim；下一个 fire time 自动重新入队 |
| TiKV 分区 | worker 无法 scan queue 或 claim | tick 失败，warn 日志；TiKV 恢复后自动继续 |
| pg-tikv 不可用 | worker claim 成功但 SQL 执行失败 | run_details 记录 failed；下次 fire time 重试 |
| Queue entry 丢失 | task 不再触发 | 定期 reconcile：比对 registry 中的 keyspace 和 tenant keyspace 中的 job，补写缺失的 queue entry |
| 时钟漂移 | task 早触发或晚触发 | claim 的 `fire_time_min` 做 minute 级截断，容忍 ±30s |
| 所有 worker 下线 | 没有 task 执行 | queue 中的 entry 不会丢失，worker 恢复后从 queue 中 catch up |

---

## 监控指标

暴露以下 Prometheus metrics：

```
# Gauge
pg_tikv_worker_queue_depth{task_type}           # 队列深度
pg_tikv_worker_active_jobs{worker_id}           # 活跃 job 数
pg_tikv_worker_claim_success_rate{task_type}    # claim 成功率

# Counter
pg_tikv_worker_tasks_executed_total{task_type, status}  # 执行总数
pg_tikv_worker_tasks_failed_total{task_type, reason}    # 失败总数

# Histogram
pg_tikv_worker_task_duration_seconds{task_type}  # 执行耗时
pg_tikv_worker_queue_latency_seconds{task_type}  # 队列延迟
```

---

## 测试计划

### 现有覆盖（保持不变）

- `src/cron/parser.rs` (24 tests) — cron 表达式解析
- `src/cron/types.rs` (5 tests) — 序列化往返
- `tests/170_cron_basic.sql` 等 (11 tests) — SQL regression

### 新增测试：Phase 1

#### Unit Tests

- TaskRegistryEntry / TaskQueueEntry / WorkerClaim bincode 往返
- Queue key 排序（big-endian i64）
- next_fire_time 计算（各种 cron 表达式）
- Claim 去重（minute 级截断）
- 并发控制（Semaphore 限制）

#### Storage Tests

- Registry CRUD（需 TiKV）
- Queue CRUD（需 TiKV）
- Claim 竞争（需 TiKV）

#### SQL Integration Tests

- `tests/182_cron_execution_e2e.sql` — cron job 执行验证
- `tests/183_cron_registry_integration.sql` — registry 写入验证
- `tests/184_cron_queue_integration.sql` — queue 写入验证

### 新增测试：Phase 2

- E2E：worker 进程执行 task
- 多 worker 竞争：验证去重
- Worker crash recovery：orphan GC 清理

### 新增测试：Phase 3

- Async trigger 入队
- Auto-ANALYZE 入队
- CREATE INDEX CONCURRENTLY 两阶段
- Background job 入队

---

## 实现优先级

### P0（必须）

1. Task registry + queue 基础设施
2. Worker claim 机制（TiKV 悲观事务）
3. Cron 迁移到新 queue
4. 并发执行（Semaphore）

### P1（重要）

1. Async trigger 迁移到 TiKV queue
2. Auto-ANALYZE 设计实现
3. GC 优化（registry 扫描）
4. 监控指标

### P2（可选）

1. CREATE INDEX CONCURRENTLY
2. REFRESH MATERIALIZED VIEW CONCURRENTLY
3. Background job API
4. 优先级队列

---

## Future Work

- **优先级队列**：不同 tenant 的 task 优先级不同（付费 tier）。可通过在 queue key 中加入 priority 字段实现。
- **执行超时**：单个 task 的 SQL 执行超时。需要在 pgwire 连接上设置 `statement_timeout`。
- **跨 region**：Worker 亲和性 — 优先 claim 同 region 的 task，减少跨 region SQL 执行延迟。
- **任务依赖**：支持 task 之间的依赖关系（如 ANALYZE 完成后更新统计信息）。
- **重试策略**：可配置的重试次数和退避策略。

---

## 总结

统一异步任务引擎通过以下设计实现高效、可扩展的异步任务执行：

1. **全局 queue**：TiKV 本身做协调层，无需外部依赖
2. **Work stealing**：多 worker 通过悲观事务竞争，无需 leader election
3. **并发执行**：Semaphore 控制并发度，充分利用资源
4. **持久化**：所有 task 持久化到 TiKV，进程重启不丢失
5. **可扩展**：支持多种 task 类型，统一管理和监控

TiKV 本身就是最好的协调层。
