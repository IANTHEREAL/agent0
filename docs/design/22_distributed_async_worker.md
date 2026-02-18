# 分布式异步任务引擎设计（Cron / Triggers / Background Jobs）

## 状态

- **阶段**：设计中
- **作者**：pg-tikv team
- **日期**：2026-02-17
- **关联文件**：`src/cron/`、`src/sql/trigger_worker.rs`、`src/pool.rs`、`src/storage/tikv_store/cron.rs`

---

## 问题陈述

当前 cron worker 是单进程单例（`OnceLock`），每 60 秒全量扫描所有 keyspace 的所有 job。在百万级 tenant 的分布式部署下有以下硬伤：

| 问题 | 现状 | 百万级影响 |
|------|------|-----------|
| 发现 | `pool.list_all_keyspaces()` — 只有内存缓存 | 重启后丢失，无法发现所有 tenant |
| 扫描 | O(keyspace × db × job) / tick | 1万 cron tenant × 2 job = 2万次 TiKV scan/分钟 |
| 执行 | 串行 `for job in due_jobs` | 500 due job × 100ms = 50s，超过 tick 间隔 |
| 分布 | 单实例 OnceLock 单例 | 无水平扩展，单点故障 |
| Claim | `_sys_cron_claim_{db}_{job}_{min}` 持续累积 | 1万 job × 1440 min/天 = 1440万 key/天 |
| GC | 全量扫描所有 keyspace 的 run | 同扫描问题 |

**规模假设**：

```
总 tenant:              1,000,000
启用 cron 的 tenant:     10,000   (1%)
总 cron job:             20,000   (avg 2/tenant)
峰值 due job/分钟:       200-500
pg-tikv 实例:            10+
```

---

## 设计概览

核心思路：**从进程内单例变为独立微服务**，用 TiKV 本身做协调层，不引入外部依赖。

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
                    ┌───────────────────┬┴──────────────────┐
                    │                   │                    │
              ┌─────┴─────┐      ┌─────┴─────┐      ┌─────┴─────┐
              │  Worker 1  │      │  Worker 2  │      │  Worker N  │
              │            │      │            │      │            │
              │ 1. scan    │      │ 1. scan    │      │ 1. scan    │
              │    queue   │      │    queue   │      │    queue   │
              │ 2. claim   │      │ 2. claim   │      │ 2. claim   │
              │    (txn)   │      │    (txn)   │      │    (txn)   │
              │ 3. execute │      │ 3. execute │      │ 3. execute │
              │    via     │      │    via     │      │    via     │
              │    pgwire  │      │    pgwire  │      │    pgwire  │
              └─────┬──────┘      └─────┬──────┘      └──────┬────┘
                    │                   │                     │
                    └───────────────────┼─────────────────────┘
                                        │ pgwire (SQL)
                    ┌───────────────────┼─────────────────────┐
                    │                   │                     │
              ┌─────┴─────┐      ┌─────┴─────┐      ┌───────┴───┐
              │ pg-tikv 1  │      │ pg-tikv 2  │      │ pg-tikv M  │
              │ (SQL only) │      │ (SQL only) │      │ (SQL only) │
              └────────────┘      └────────────┘      └───────────┘
```

**关键决策**：

1. Worker 通过 **pgwire 连回 pg-tikv** 执行 SQL，不直接操作 TiKV 数据。这保证了事务语义、权限检查、trigger 等完整 SQL 路径。
2. Worker 通过 **TiKV 悲观事务** 做 claim，天然分布式锁，不需要额外的 leader election。
3. 同一套引擎同时服务 cron job、async trigger、未来的 background job。

---

## 详细设计

### 1. 全局任务注册表（Task Registry）

**问题**：当前需要扫描每个 keyspace 的每个 db 才能知道哪里有 cron job。

**方案**：在一个 **系统 keyspace**（`_sys_worker`，不属于任何 tenant）中维护全局注册表。

#### 注册表 key layout

```
_worker_registry_{keyspace}_{db_id}  →  TaskRegistryEntry (bincode)
```

```rust
struct TaskRegistryEntry {
    keyspace: String,
    db_id: u64,
    task_types: u8,       // bitmask: 0x01=cron, 0x02=async_trigger, 0x04=bg_job
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

#### 影响文件

- `src/sql/executor/cron.rs`：schedule/unschedule 时写 registry
- `src/extensions/mod.rs`：CREATE/DROP EXTENSION 时写 registry
- `src/storage/tikv_store/cron.rs`：新增 registry 读写方法

#### 扫描代价

Worker tick 时只需 **一次 prefix scan** `_worker_registry_` → 拿到所有有任务的 keyspace 列表。1 万条 entry ≈ 1MB，单次 scan < 50ms。

---

### 2. 预计算 Next-Fire-Time 索引

**问题**：当前每 tick 对每个 job 解析 cron 表达式并检查 `is_due()`。

**方案**：每个 job 维护一个 `next_fire_time`，写入全局有序队列。tick 时只 range scan `fire_time <= now`。

#### 队列 key layout

```
_worker_queue_{next_fire_time_ms}_{keyspace}_{db_id}_{job_id}  →  TaskQueueEntry (bincode)
```

`next_fire_time_ms` 使用 **big-endian i64**，自然有序。

```rust
struct TaskQueueEntry {
    keyspace: String,
    db_id: u64,
    job_id: i64,
    task_type: TaskType,   // Cron | AsyncTrigger | BgJob
    command: String,        // SQL to execute
    username: String,       // 执行身份
    schedule: String,       // cron 表达式（用于计算下一次 fire time）
}
```

#### 写入时机

| 操作 | 动作 |
|------|------|
| `cron.schedule()` | 计算 next_fire_time，写入 queue |
| job 执行完成后 | 计算下一次 next_fire_time，写入新 queue entry，删除旧 entry |
| `cron.alter_job()` 修改 schedule | 删除旧 entry，计算新 next_fire_time，写入新 entry |
| `cron.unschedule()` | 删除 queue entry |
| `cron.alter_job(active=false)` | 删除 queue entry（不再触发） |
| `cron.alter_job(active=true)` | 重新计算 next_fire_time 并写入 |

#### Tick 流程（新）

```
every tick_interval:
  now = current_time_ms()
  entries = range_scan("_worker_queue_" .. "_worker_queue_{now}")
  // 只返回 fire_time <= now 的 entry，O(due_jobs)
  for entry in entries:
    spawn_claim_and_execute(entry)
```

#### 扫描代价

峰值 500 due jobs/分钟 → 每次 scan 最多返回 ~500 条，< 100KB。

#### 影响文件

- `src/storage/tikv_store/cron.rs`：新增 queue 读写方法
- `src/sql/executor/cron.rs`：schedule/unschedule/alter_job 时维护 queue
- `src/cron/worker.rs`：tick 改为 queue range scan

---

### 3. 多实例 Work Stealing

**问题**：单实例单例，无法水平扩展。

**方案**：所有 worker 实例平等，通过 TiKV 悲观事务竞争 claim。

#### Claim 机制（已有，复用）

```
_worker_claim_{keyspace}_{db_id}_{job_id}_{fire_time_min}  →  WorkerClaim
```

```rust
struct WorkerClaim {
    worker_id: String,     // 实例标识（hostname:pid 或 UUID）
    claimed_at: i64,       // epoch ms
}
```

#### 竞争流程

```rust
async fn try_claim(txn, entry) -> bool {
    let key = claim_key(entry.keyspace, entry.db_id, entry.job_id, fire_minute);
    if txn.get(key).await?.is_some() {
        return false;  // 已被其他 worker claim
    }
    txn.put(key, serialize(WorkerClaim { ... }));
    true  // TiKV 悲观事务保证只有一个 writer 成功
}
```

多个 worker 同时 scan 到同一批 due jobs，各自尝试 claim。TiKV 悲观事务保证同一个 key 只有一个 writer 成功 commit，其余自动 abort。**不需要 leader election。**

#### 负载均衡

每个 worker 维护本地 `active_jobs` 计数器（AtomicU32）。当 `active_jobs >= max_concurrent_jobs` 时跳过本轮 claim，让其他 worker 接手。

无需复杂的分片或一致性哈希 — 随机竞争 + 背压足以在 10-50 个 worker 间均匀分布。

#### 影响文件

- `src/cron/worker.rs`：去掉 `OnceLock` 单例，改为可配置启动
- `src/cron/config.rs`：新增 `worker_id` 配置

---

### 4. 并发执行

**问题**：串行执行 due jobs。

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

#### 执行路径（新）：通过 pgwire 连回 pg-tikv

```rust
async fn execute_job_sql(entry: &TaskQueueEntry) -> Result<()> {
    // 连接到对应 tenant 的 pg-tikv
    let connstr = format!(
        "host={} port={} user={}.{} password={} dbname=postgres",
        pg_host, pg_port, entry.keyspace, entry.username, service_password
    );
    let (client, conn) = tokio_postgres::connect(&connstr, NoTls).await?;
    tokio::spawn(conn);
    client.simple_query(&entry.command).await?;
    Ok(())
}
```

**为什么走 pgwire 而不是直接操作 TiKV**：
- 保证完整 SQL 语义（事务、trigger、权限检查、search_path）
- Worker 无需理解 pg-tikv 内部状态，纯无状态
- 可以用标准连接池（deadpool-postgres）管理连接

#### 影响文件

- `src/cron/worker.rs`：执行路径从 `Executor::execute_statement_on_txn` 改为 pgwire 连接
- `Cargo.toml`：如果 worker 独立 binary 则新增 crate

---

### 5. Claim 生命周期

**问题**：claim key 无限累积。

**方案**：双重清理 — 执行完成后立即删除 + GC 兜底。

#### 正常流程

```
job 执行完成 → 写入 run_details → 删除 claim key → 写入下一次 queue entry
```

一个成功的 job 执行后 claim key 立即被删除，不累积。

#### 异常流程（orphan）

Worker crash 或 job 超时 → claim key 残留。GC 处理：

```
GC 每 10 分钟:
  scan _worker_claim_ prefix
  for claim in claims:
    if claim.claimed_at < now - orphan_timeout:
      delete claim
      // 不重新入队 — 等下一次 fire time 自然触发
```

#### 代价估算

正常情况下 claim key 存活时间 < 1 分钟。峰值 500 job/min → 瞬时最多 500 个 claim key。GC 只需处理异常残留。

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

## 新 Key Layout（完整）

### 系统 keyspace（`_sys_worker`，跨 tenant）

```
_worker_registry_{keyspace}_{db_id}                              → TaskRegistryEntry
_worker_queue_{next_fire_time_ms}_{keyspace}_{db_id}_{job_id}   → TaskQueueEntry
_worker_claim_{keyspace}_{db_id}_{job_id}_{fire_time_min}       → WorkerClaim
```

### Tenant keyspace（保持不变）

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
└── CronWorker (改用 queue scan + concurrent execution)
```

- 不拆分进程，最小改动
- 仍然走进程内 `Executor` 执行 SQL
- `list_all_keyspaces()` 改为 scan registry
- 加并发执行

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
  BgJob         → 用户提交的一次性后台任务（future）
```

同一套 queue + claim + execute 机制，不同 task_type 只是入队方式不同：
- Cron：schedule 时写入 queue
- AsyncTrigger：trigger fire 时写入 queue（`next_fire_time = now`）
- BgJob：用户 `SELECT pg_background_launch(...)` 时写入 queue

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
| `PGTIKV_CRON_ENABLED` | `true` | 是否启动进程内 worker |
| `PGTIKV_CRON_POLL_MS` | `60000` | tick 间隔（不变） |
| `PGTIKV_CRON_MAX_CONCURRENT_JOBS` | `32` | 单 worker 最大并发执行数 |
| `PGTIKV_CRON_WORKER_ID` | `{hostname}:{pid}` | Worker 实例标识 |
| `PGTIKV_CRON_USE_QUEUE` | `false` | Phase 2+ 开关：使用全局 queue 而非全量扫描 |
| `PGTIKV_CRON_ORPHAN_TIMEOUT_SEC` | `300` | orphan claim 超时 |
| `PGTIKV_CRON_GC_BATCH_SIZE` | `100` | 每轮 GC 处理的 keyspace 数 |
| `PGTIKV_CRON_PG_HOST` | （同 pg-tikv） | Phase 2：worker 连回的 pg-tikv 地址 |
| `PGTIKV_CRON_PG_PORT` | （同 pg-tikv） | Phase 2：worker 连回的 pg-tikv 端口 |

---

## 故障模式

| 故障 | 影响 | 恢复 |
|------|------|------|
| Worker crash | 已 claim 的 job 不会执行 | orphan GC 在 5 分钟后清理 claim；下一个 fire time 自动重新入队 |
| TiKV 分区 | worker 无法 scan queue 或 claim | tick 失败，warn 日志；TiKV 恢复后自动继续 |
| pg-tikv 不可用 | worker claim 成功但 SQL 执行失败 | run_details 记录 failed；下次 fire time 重试 |
| Queue entry 丢失 | job 不再触发 | 定期 reconcile：比对 registry 中的 keyspace 和 tenant keyspace 中的 job，补写缺失的 queue entry |
| 时钟漂移 | job 早触发或晚触发 | claim 的 `fire_time_min` 做 minute 级截断，容忍 ±30s |
| 所有 worker 下线 | 没有 cron 执行 | queue 中的 entry 不会丢失，worker 恢复后从 queue 中 catch up |

---

## 测试计划

### 现有覆盖（保持不变）

| 层级 | 文件 | 覆盖内容 |
|------|------|----------|
| Unit | `src/cron/parser.rs` (24 tests) | cron 表达式解析、拒绝非法格式、`is_due`、`next_occurrence` |
| Unit | `src/cron/types.rs` (5 tests) | CronJob/CronRun bincode 序列化往返、CronRunStatus display |
| Unit | `src/cron/config.rs` (1 test) | CronConfig 环境变量解析和默认值 |
| SQL | `tests/170_cron_basic.sql` | CREATE/DROP EXTENSION、schedule、unschedule、alter_job、cron.job/cron.job_run_details 虚拟表 |
| SQL | `tests/171_cron_validation.sql` | schedule 参数验证、非法 cron 表达式、缺失参数 |
| SQL | `tests/173_cron_upsert.sql` | 同名 job upsert 语义 |
| SQL | `tests/174_cron_alter_job.sql` | alter_job 各参数组合、错误处理 |
| SQL | `tests/175_cron_error_handling.sql` | 错误码、权限、边界条件 |
| SQL | `tests/176_cron_extension_lifecycle.sql` | CREATE/DROP EXTENSION 生命周期、重复操作 |
| SQL | `tests/177_cron_virtual_table.sql` | cron.job / cron.job_run_details 虚拟表查询模式 |
| SQL | `tests/178_cron_expressions.sql` | 各种 cron 表达式验证 |
| SQL | `tests/179_cron_mixed_operations.sql` | 混合操作序列（schedule + alter + unschedule 交叉） |
| SQL | `tests/180_cron_query_patterns.sql` | WHERE/ORDER BY/JOIN 等查询模式 |
| SQL | `tests/181_cron_boundary.sql` | 边界条件（引号转义、长命令、批量 job、幂等操作） |

---

### 新增测试：Phase 1（进程内改进）

#### 1.1 Unit Tests — 新类型序列化

文件：`src/cron/types.rs`

```rust
// TaskRegistryEntry bincode 往返
#[test] fn task_registry_entry_roundtrip()
#[test] fn task_registry_entry_bitmask_operations()  // 0x01|0x02, clear bit, check zero

// TaskQueueEntry bincode 往返
#[test] fn task_queue_entry_roundtrip()
#[test] fn task_queue_entry_all_task_types()  // Cron, AsyncTrigger, BgJob

// WorkerClaim bincode 往返
#[test] fn worker_claim_roundtrip()
```

#### 1.2 Unit Tests — Queue Key 排序

文件：`src/storage/encoding.rs`

```rust
// next_fire_time 编码后的字节序必须保持时间排序
#[test]
fn queue_key_ordering() {
    let k1 = encode_worker_queue_key(1000, "ks_a", 1, 1);
    let k2 = encode_worker_queue_key(2000, "ks_a", 1, 1);
    let k3 = encode_worker_queue_key(3000, "ks_b", 1, 2);
    assert!(k1 < k2);
    assert!(k2 < k3);
}

// 负时间戳（理论上不应出现，但需防御）
#[test]
fn queue_key_negative_timestamp_handled()

// 相同 fire_time 不同 keyspace 不冲突
#[test]
fn queue_key_same_time_different_keyspace()
```

#### 1.3 Unit Tests — next_fire_time 计算

文件：`src/cron/parser.rs`

```rust
// 从 cron 表达式 + 当前时间计算下一次触发时间
#[test] fn next_fire_time_every_minute()         // "* * * * *" → 下一分钟整
#[test] fn next_fire_time_every_5_minutes()      // "*/5 * * * *"
#[test] fn next_fire_time_daily_at_3am()         // "0 3 * * *"
#[test] fn next_fire_time_weekday_only()         // "0 9 * * 1-5"
#[test] fn next_fire_time_crosses_midnight()     // 23:59 → 次日
#[test] fn next_fire_time_crosses_month()        // 月末 → 次月
#[test] fn next_fire_time_idempotent()           // 对同一时间点调用两次结果一致
```

#### 1.4 Unit Tests — Claim 去重

文件：`src/cron/worker.rs` 或独立 `src/cron/claim.rs`

```rust
// minute 级截断：不同秒数映射到相同 claim key
#[test]
fn claim_key_minute_truncation() {
    let k1 = claim_key("ks", 1, 42, ts_to_minute(1700000010));  // :10 秒
    let k2 = claim_key("ks", 1, 42, ts_to_minute(1700000050));  // :50 秒
    assert_eq!(k1, k2);  // 同一分钟
}

// 不同分钟生成不同 claim key
#[test]
fn claim_key_different_minutes()

// 不同 job_id 生成不同 claim key
#[test]
fn claim_key_different_jobs()
```

#### 1.5 Unit Tests — 并发控制

文件：`src/cron/worker.rs`

```rust
// Semaphore 限制并发数
#[tokio::test]
async fn concurrent_execution_respects_limit() {
    // 设置 max_concurrent = 4
    // 提交 10 个 job
    // 验证同时执行数不超过 4
}

// 空 queue 不阻塞
#[tokio::test]
async fn empty_queue_returns_immediately()
```

---

### 新增测试：Phase 1 — Storage 层

#### 1.6 Storage Tests — Registry CRUD

文件：`src/storage/tikv_store/cron.rs`（需要 TiKV，标记 `#[ignore]` 或用 mock）

```rust
// 写入 registry entry → 读回验证
#[tokio::test] async fn registry_put_and_get()

// 更新 task_types bitmask
#[tokio::test] async fn registry_update_bitmask()

// 删除 registry entry
#[tokio::test] async fn registry_delete()

// scan 全部 registry entry（验证 prefix scan）
#[tokio::test] async fn registry_scan_all()

// 空 registry scan 返回空 vec
#[tokio::test] async fn registry_scan_empty()
```

#### 1.7 Storage Tests — Queue CRUD

```rust
// 写入 queue entry → 读回
#[tokio::test] async fn queue_put_and_get()

// range scan 按 fire_time 排序
#[tokio::test] async fn queue_range_scan_ordering() {
    // 写入 fire_time = 1000, 3000, 2000
    // scan(..2500) → 返回 [1000, 2000]
}

// 删除旧 queue entry
#[tokio::test] async fn queue_delete_entry()

// 删除 + 重新写入（job 完成后 reschedule）
#[tokio::test] async fn queue_reschedule_cycle()
```

#### 1.8 Storage Tests — Claim

```rust
// 首次 claim 成功
#[tokio::test] async fn claim_first_writer_succeeds()

// 重复 claim 同一 key 失败
#[tokio::test] async fn claim_duplicate_returns_false()

// claim 删除后可重新 claim
#[tokio::test] async fn claim_after_delete_succeeds()
```

---

### 新增测试：SQL 集成测试

#### 1.9 Cron 执行端到端（已有 fix 验证基础）

文件：`tests/182_cron_execution_e2e.sql`

```sql
-- E2E: cron job 实际执行验证
CREATE EXTENSION IF NOT EXISTS pg_cron;
CREATE TABLE cron_e2e_test (id SERIAL, val TEXT, ts TIMESTAMP DEFAULT NOW());

-- 调度每分钟执行的 job
SELECT cron.schedule('e2e_insert', '* * * * *', 
  'INSERT INTO cron_e2e_test (val) VALUES (''cron_fired'')');

-- 验证 job 已注册
SELECT jobname, schedule, active FROM cron.job WHERE jobname = 'e2e_insert';

-- 注意：实际执行验证需要等待 >60s，不适合 regression gate
-- 此测试仅验证 schedule + 状态，执行验证走独立 e2e 脚本
SELECT cron.unschedule('e2e_insert');
DROP TABLE cron_e2e_test;
DROP EXTENSION pg_cron;
```

#### 1.10 Registry 写入验证（Phase 1 就绪后）

文件：`tests/183_cron_registry_integration.sql`

```sql
-- 验证 CREATE EXTENSION 注册到全局 registry
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- 验证 schedule 更新 registry job_count
SELECT cron.schedule('reg_test1', '0 * * * *', 'SELECT 1');
SELECT cron.schedule('reg_test2', '0 * * * *', 'SELECT 2');

-- 验证 unschedule 递减 job_count
SELECT cron.unschedule('reg_test1');

-- 验证 DROP EXTENSION 清除 registry
SELECT cron.unschedule('reg_test2');
DROP EXTENSION pg_cron;
```

#### 1.11 Queue 写入验证（Phase 1 就绪后）

文件：`tests/184_cron_queue_integration.sql`

```sql
-- 验证 schedule 写入 queue entry
CREATE EXTENSION IF NOT EXISTS pg_cron;
SELECT cron.schedule('q_test', '*/5 * * * *', 'SELECT 1');

-- 验证 alter_job 更新 queue entry（新 schedule → 新 fire_time）
SELECT cron.alter_job(1, '0 3 * * *');

-- 验证 disable 删除 queue entry
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, false);

-- 验证 re-enable 重新写入 queue entry
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, true);

-- 验证 unschedule 删除 queue entry
SELECT cron.unschedule('q_test');
DROP EXTENSION pg_cron;
```

---

### 新增测试：Phase 2（独立 Worker 微服务）

#### 2.1 Worker 进程 — 集成测试脚本

文件：`scripts/test_cron_worker.sh`

```bash
#!/bin/bash
# 前置条件：pg-tikv 运行中，TiKV 运行中

# 1. 创建 cron job
psql -c "CREATE EXTENSION IF NOT EXISTS pg_cron"
psql -c "SELECT cron.schedule('worker_test', '* * * * *', 'INSERT INTO t VALUES (1)')"

# 2. 等待 2 分钟
sleep 120

# 3. 验证 job 执行了至少 1 次
COUNT=$(psql -t -c "SELECT count(*) FROM cron.job_run_details WHERE status = 'succeeded'")
if [ "$COUNT" -lt 1 ]; then
  echo "FAIL: cron job did not execute" && exit 1
fi

# 4. 验证数据写入
ROW_COUNT=$(psql -t -c "SELECT count(*) FROM t WHERE val = 1")
if [ "$ROW_COUNT" -lt 1 ]; then
  echo "FAIL: cron SQL not applied" && exit 1
fi

echo "PASS"
```

#### 2.2 多 Worker 竞争（手动 / CI 测试）

文件：`scripts/test_cron_multi_worker.sh`

```bash
#!/bin/bash
# 测试目标：两个 worker 不会重复执行同一个 job

# 1. 启动两个 pg-tikv 实例（不同端口，共享 TiKV）
# 2. 创建 cron job（counter 表递增）
# 3. 等待 3 分钟（3 次 tick）
# 4. 验证 counter = 3（不是 6 — 每分钟只执行一次，不重复）

psql -c "CREATE TABLE counter (n INT DEFAULT 0); INSERT INTO counter VALUES (0)"
psql -c "SELECT cron.schedule('dedup_test', '* * * * *', 'UPDATE counter SET n = n + 1')"
sleep 200

N=$(psql -t -c "SELECT n FROM counter")
if [ "$N" -ne 3 ]; then
  echo "FAIL: expected 3, got $N (dedup broken?)" && exit 1
fi
echo "PASS: counter=$N"
```

#### 2.3 Worker Crash Recovery

```bash
# 1. 启动 worker，创建 cron job
# 2. 在 job claim 后、execute 前 kill worker（模拟 crash）
# 3. 等待 orphan_timeout（5 min）
# 4. 验证 GC 清理了 orphan claim
# 5. 验证下一次 tick 重新执行 job
```

---

### 新增测试：Phase 3（统一异步任务引擎）

#### 3.1 Async Trigger 入队

```sql
-- 验证 AFTER trigger 写入 queue（next_fire_time = now）
CREATE TABLE audit (id SERIAL, action TEXT);
CREATE TABLE orders (id SERIAL, status TEXT);
CREATE TRIGGER order_audit AFTER INSERT ON orders
  FOR EACH ROW EXECUTE FUNCTION audit_insert();  -- 异步执行

INSERT INTO orders (status) VALUES ('new');
-- 验证 audit 表在短时间内收到记录
```

#### 3.2 Background Job 入队

```sql
-- 验证用户提交的一次性后台任务
SELECT pg_background_launch('VACUUM ANALYZE orders');
-- 验证 queue 中有 BgJob 类型 entry
-- 验证 job 执行完成后 queue entry 被清除
```

---

### 测试矩阵汇总

| 阶段 | 类型 | 数量 | 自动化 |
|------|------|------|--------|
| 现有 | Unit (parser/types/config) | 30 | `cargo test` |
| 现有 | SQL regression (170-181) | 11 | `regression_gate.sh` |
| Phase 1 | Unit — 新类型序列化 | ~5 | `cargo test` |
| Phase 1 | Unit — queue key 排序 | ~4 | `cargo test` |
| Phase 1 | Unit — next_fire_time | ~7 | `cargo test` |
| Phase 1 | Unit — claim 去重 | ~3 | `cargo test` |
| Phase 1 | Unit — 并发控制 | ~2 | `cargo test` |
| Phase 1 | Storage — registry CRUD | ~5 | `cargo test` (需 TiKV) |
| Phase 1 | Storage — queue CRUD | ~4 | `cargo test` (需 TiKV) |
| Phase 1 | Storage — claim | ~3 | `cargo test` (需 TiKV) |
| Phase 1 | SQL regression (182-184) | 3 | `regression_gate.sh` |
| Phase 2 | E2E — worker 执行 | 1 | `scripts/test_cron_worker.sh` |
| Phase 2 | E2E — 多 worker 去重 | 1 | `scripts/test_cron_multi_worker.sh` |
| Phase 2 | E2E — crash recovery | 1 | `scripts/test_cron_crash_recovery.sh` |
| Phase 3 | SQL — async trigger | 1 | `regression_gate.sh` |
| Phase 3 | SQL — background job | 1 | `regression_gate.sh` |
| **合计** | | **~82** | |

### 验收标准

1. **Phase 1 完成条件**：所有现有 30 unit + 11 SQL 测试 pass，新增 ~33 unit + 3 SQL 测试 pass
2. **Phase 2 完成条件**：Phase 1 全部 pass + 3 个 E2E 脚本 pass（含多 worker 去重验证）
3. **Phase 3 完成条件**：Phase 2 全部 pass + async trigger + background job SQL 测试 pass
4. **回归守门**：每次 PR 必须通过 `regression_gate.sh`（含所有 SQL 测试）+ `cargo test`（含所有 unit 测试）

---

## Future Work

- **优先级队列**：不同 tenant 的 job 优先级不同（付费 tier）。可通过在 queue key 中加入 priority 字段实现。
- **执行超时**：单个 job 的 SQL 执行超时（当前无限制）。需要在 pgwire 连接上设置 `statement_timeout`。
- **监控指标**：queue depth、claim success rate、execution latency 暴露为 Prometheus metrics。
- **跨 region**：Worker 亲和性 — 优先 claim 同 region 的 job，减少跨 region SQL 执行延迟。
