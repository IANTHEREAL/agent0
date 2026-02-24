# 设计：异步 Trigger Queue（分布式 AFTER 触发器）

**Status**: Draft  
**Priority**: P2  
**依赖**: 现有 trigger DDL 存储（已实现）

## 背景与动机

db9-server 当前支持 BEFORE/AFTER ROW trigger 的同步执行（in-process），但在分布式多租户场景下存在局限：

1. **AFTER trigger 阻塞 DML**：同步执行增加响应延迟
2. **trigger 执行失败影响事务**：一个 trigger 失败导致整个 DML 回滚
3. **无法跨节点观测**：trigger 执行状态不可查

本设计提出基于内部系统表的异步 trigger 队列，无需外部 CDC 依赖，在保证事务一致性的同时实现非阻塞执行。

## 目标

### MVP（Phase 1）
- AFTER ROW trigger 异步执行（入队与 DML 同事务）
- 多租户隔离（keyspace 级别队列）
- 公平调度（防止大租户饿死小租户）
- 失败重试 + 死信处理
- 自动垃圾清理

### 非目标（MVP 不做）
- BEFORE trigger 异步化（需同步修改 NEW row）
- STATEMENT level trigger
- 跨 keyspace trigger
- 分布式事务语义（trigger 执行与原 DML 不在同一事务）

## 架构概览

```
┌─────────────────────────────────────────────────────────────┐
│                        db9-server node                         │
│                                                             │
│  ┌──────────────┐     ┌─────────────────────────────────┐  │
│  │ SQL Handler  │     │        Trigger Worker           │  │
│  │              │     │                                 │  │
│  │  INSERT ─────┼──┐  │  ┌───────────────────────────┐  │  │
│  │  UPDATE      │  │  │  │ Active Keyspace Registry │  │  │
│  │  DELETE      │  │  │  └───────────────────────────┘  │  │
│  └──────────────┘  │  │              │                   │  │
│                    │  │              ▼                   │  │
│                    │  │  ┌───────────────────────────┐  │  │
│                    │  │  │   Fair Scheduler          │  │  │
│                    │  │  │   (N events/tenant/batch) │  │  │
│                    │  │  └───────────────────────────┘  │  │
│                    │  │              │                   │  │
│                    │  │              ▼                   │  │
│                    │  │  ┌───────────────────────────┐  │  │
│                    │  │  │   Executor Pool           │  │  │
│                    │  │  └───────────────────────────┘  │  │
│                    │  │              │                   │  │
│                    │  │              ▼                   │  │
│                    │  │  ┌───────────────────────────┐  │  │
│                    │  │  │   GC Worker               │  │  │
│                    │  │  └───────────────────────────┘  │  │
│                    │  └─────────────────────────────────┘  │
│                    │                                        │
│                    ▼                                        │
│  ┌──────────────────────────────────────────────────────┐  │
│  │                      TiKV                             │  │
│  │  ┌────────────────┐  ┌────────────────┐              │  │
│  │  │ _sys_tq_ks_a   │  │ _sys_tq_ks_b   │  ...        │  │
│  │  │ (trigger queue)│  │ (trigger queue)│              │  │
│  │  └────────────────┘  └────────────────┘              │  │
│  │                                                       │  │
│  │  ┌────────────────────────────────────┐              │  │
│  │  │ _sys_tq_dlq_{keyspace}_{id}        │ (死信队列)   │  │
│  │  └────────────────────────────────────┘              │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## 详细设计

### 1) 队列表结构

Key 格式：`_sys_tq_{keyspace}_{id}`

ID 设计（时间戳 + 序列号，便于 GC）：
- 高 42 位：毫秒时间戳（可用 139 年）
- 低 22 位：序列号（每毫秒 400 万事件）

```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct TriggerEvent {
    pub id: u64,                     // 时间戳编码的单调 ID
    pub trigger_name: String,
    pub table_name: String,
    pub operation: TriggerOp,        // Insert / Update / Delete
    pub old_row: Option<Vec<u8>>,    // bincode 序列化
    pub new_row: Option<Vec<u8>>,    // bincode 序列化
    pub created_at: i64,             // unix timestamp ms
    pub status: EventStatus,
    pub retry_count: u8,
    pub error_msg: Option<String>,
    pub worker_id: Option<String>,
    pub claimed_at: Option<i64>,     // Processing 开始时间
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub enum EventStatus {
    Pending,
    Processing,
    Done,
    Failed,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum TriggerOp {
    Insert,
    Update,
    Delete,
}

fn generate_event_id() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    (ts << 22) | (seq & 0x3FFFFF)
}
```

### 2) 入队逻辑

在 DML 执行后、事务提交前写入队列：

```rust
// src/sql/trigger_queue.rs

pub async fn enqueue_after_triggers(
    txn: &mut Transaction,
    store: &TikvStore,
    keyspace: &str,
    table: &str,
    op: TriggerOp,
    old_row: Option<&Row>,
    new_row: Option<&Row>,
    triggers: &[TriggerDef],
) -> Result<()> {
    let quota = TRIGGER_WORKER.get_quota(keyspace);
    
    // 检查队列深度
    let depth = quota.current_depth.load(Ordering::Relaxed);
    if depth >= quota.max_queue_depth {
        warn!("Trigger queue full for keyspace {}, skipping", keyspace);
        metrics::increment_counter!("trigger_queue_overflow", "keyspace" => keyspace);
        return Ok(()); // 不阻塞 DML
    }
    
    for trigger in triggers.iter().filter(|t| t.timing == After && t.level == Row) {
        let event = TriggerEvent {
            id: generate_event_id(),
            trigger_name: trigger.name.clone(),
            table_name: table.to_string(),
            operation: op.clone(),
            old_row: old_row.map(|r| bincode::serialize(r)).transpose()?,
            new_row: new_row.map(|r| bincode::serialize(r)).transpose()?,
            created_at: now_millis(),
            status: EventStatus::Pending,
            retry_count: 0,
            error_msg: None,
            worker_id: None,
            claimed_at: None,
        };
        
        let key = format!("_sys_tq_{}_{}", keyspace, event.id);
        txn.put(key.as_bytes(), bincode::serialize(&event)?).await?;
        quota.current_depth.fetch_add(1, Ordering::Relaxed);
    }
    
    // 注册到活跃 keyspace（通知 worker）
    TRIGGER_WORKER.mark_active(keyspace);
    
    Ok(())
}
```

### 3) Worker 设计

#### 主结构

```rust
// src/sql/trigger_worker.rs

pub struct TriggerWorker {
    worker_id: String,
    active_keyspaces: DashSet<String>,
    quotas: DashMap<String, KeyspaceQuota>,
    config: TriggerWorkerConfig,
    shutdown: AtomicBool,
}

pub struct KeyspaceQuota {
    pub max_queue_depth: usize,
    pub max_events_per_batch: usize,
    pub max_retries: u8,
    pub current_depth: AtomicUsize,
}

pub struct TriggerWorkerConfig {
    pub poll_interval_ms: u64,
    pub gc_interval_sec: u64,
    pub done_retention_sec: u64,
    pub dlq_retention_days: u64,
    pub orphan_timeout_sec: u64,
}

impl Default for KeyspaceQuota {
    fn default() -> Self {
        Self {
            max_queue_depth: 10_000,
            max_events_per_batch: 10,
            max_retries: 3,
            current_depth: AtomicUsize::new(0),
        }
    }
}

impl Default for TriggerWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 100,
            gc_interval_sec: 60,
            done_retention_sec: 3600,
            dlq_retention_days: 7,
            orphan_timeout_sec: 300,
        }
    }
}

lazy_static! {
    pub static ref TRIGGER_WORKER: TriggerWorker = TriggerWorker::new();
}
```

#### 活跃租户注册（避免空轮询）

```rust
impl TriggerWorker {
    pub fn mark_active(&self, keyspace: &str) {
        self.active_keyspaces.insert(keyspace.to_string());
    }
    
    pub fn get_quota(&self, keyspace: &str) -> Arc<KeyspaceQuota> {
        self.quotas
            .entry(keyspace.to_string())
            .or_insert_with(|| Arc::new(KeyspaceQuota::default()))
            .clone()
    }
}
```

#### 主循环

```rust
impl TriggerWorker {
    pub async fn run(&self, store: Arc<TikvStore>) {
        let poll_interval = Duration::from_millis(self.config.poll_interval_ms);
        let gc_interval = Duration::from_secs(self.config.gc_interval_sec);
        
        let store_clone = store.clone();
        
        // 启动 GC 协程
        let gc_handle = tokio::spawn(async move {
            self.gc_loop(store_clone).await;
        });
        
        // 主处理循环
        let mut interval = tokio::time::interval(poll_interval);
        
        while !self.shutdown.load(Ordering::Relaxed) {
            interval.tick().await;
            
            let keyspaces: Vec<_> = self.active_keyspaces
                .iter()
                .map(|k| k.clone())
                .collect();
            
            for ks in keyspaces {
                if let Err(e) = self.process_keyspace(&store, &ks).await {
                    warn!("Trigger worker error for {}: {}", ks, e);
                }
            }
        }
        
        gc_handle.abort();
    }
    
    async fn process_keyspace(&self, store: &TikvStore, keyspace: &str) -> Result<()> {
        let quota = self.get_quota(keyspace);
        
        // 获取并锁定一批待处理事件
        let mut txn = store.begin_pessimistic().await?;
        let events = self.claim_events(&mut txn, keyspace, quota.max_events_per_batch).await?;
        
        if events.is_empty() {
            self.active_keyspaces.remove(keyspace);
            return Ok(());
        }
        
        txn.commit().await?;
        
        // 逐个执行 trigger
        for event in events {
            let result = self.execute_trigger(store, keyspace, &event).await;
            self.update_event_status(store, keyspace, &event, result).await?;
        }
        
        Ok(())
    }
}
```

#### 事件认领（公平调度）

```rust
impl TriggerWorker {
    async fn claim_events(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        limit: usize,
    ) -> Result<Vec<TriggerEvent>> {
        let prefix = format!("_sys_tq_{}_", keyspace);
        let mut events = Vec::new();
        let now = now_millis();
        
        // 扫描队列
        let pairs = txn.scan(prefix.as_bytes().to_vec().., limit * 2).await?;
        
        for (key, value) in pairs {
            if events.len() >= limit {
                break;
            }
            
            let mut event: TriggerEvent = bincode::deserialize(&value)?;
            
            if event.status != EventStatus::Pending {
                continue;
            }
            
            // 认领事件
            event.status = EventStatus::Processing;
            event.worker_id = Some(self.worker_id.clone());
            event.claimed_at = Some(now);
            txn.put(key, bincode::serialize(&event)?).await?;
            
            events.push(event);
        }
        
        Ok(events)
    }
}
```

#### Trigger 执行

```rust
impl TriggerWorker {
    async fn execute_trigger(
        &self,
        store: &TikvStore,
        keyspace: &str,
        event: &TriggerEvent,
    ) -> Result<()> {
        let start = Instant::now();
        
        // 获取 trigger 定义
        let trigger_key = format!("_sys_trigger_{}_{}", keyspace, event.trigger_name);
        let mut txn = store.begin_pessimistic().await?;
        
        let trigger_data = txn.get(trigger_key.as_bytes()).await?
            .ok_or_else(|| anyhow!("Trigger {} not found", event.trigger_name))?;
        let trigger: TriggerDef = bincode::deserialize(&trigger_data)?;
        
        // 反序列化行数据
        let old_row = event.old_row.as_ref()
            .map(|b| bincode::deserialize::<Row>(b))
            .transpose()?;
        let new_row = event.new_row.as_ref()
            .map(|b| bincode::deserialize::<Row>(b))
            .transpose()?;
        
        // 执行 trigger 函数
        // 这里调用现有的 plpgsql 执行器
        execute_trigger_function(
            &mut txn,
            store,
            keyspace,
            &trigger,
            old_row.as_ref(),
            new_row.as_ref(),
        ).await?;
        
        txn.commit().await?;
        
        // 记录指标
        let duration = start.elapsed();
        metrics::histogram!(
            "trigger_execution_duration_ms",
            duration.as_millis() as f64,
            "keyspace" => keyspace,
            "trigger" => event.trigger_name.clone()
        );
        
        Ok(())
    }
}
```

#### 状态更新与重试

```rust
impl TriggerWorker {
    async fn update_event_status(
        &self,
        store: &TikvStore,
        keyspace: &str,
        event: &TriggerEvent,
        result: Result<()>,
    ) -> Result<()> {
        let mut txn = store.begin_pessimistic().await?;
        let key = format!("_sys_tq_{}_{}", keyspace, event.id);
        let quota = self.get_quota(keyspace);
        
        match result {
            Ok(()) => {
                // 成功：删除事件（或标记 Done，由 GC 清理）
                txn.delete(key.as_bytes()).await?;
                quota.current_depth.fetch_sub(1, Ordering::Relaxed);
                
                metrics::increment_counter!(
                    "trigger_events_completed",
                    "keyspace" => keyspace
                );
            }
            Err(e) => {
                let mut updated = event.clone();
                updated.retry_count += 1;
                updated.error_msg = Some(e.to_string());
                
                if updated.retry_count >= quota.max_retries {
                    // 移入死信队列
                    updated.status = EventStatus::Failed;
                    let dlq_key = format!("_sys_tq_dlq_{}_{}", keyspace, event.id);
                    txn.put(dlq_key.as_bytes(), bincode::serialize(&updated)?).await?;
                    txn.delete(key.as_bytes()).await?;
                    
                    warn!(
                        "Trigger event {} moved to DLQ after {} retries: {}",
                        event.id, updated.retry_count, e
                    );
                    
                    metrics::increment_counter!(
                        "trigger_events_dlq",
                        "keyspace" => keyspace
                    );
                } else {
                    // 重试：状态改回 Pending
                    updated.status = EventStatus::Pending;
                    updated.worker_id = None;
                    updated.claimed_at = None;
                    txn.put(key.as_bytes(), bincode::serialize(&updated)?).await?;
                    
                    metrics::increment_counter!(
                        "trigger_events_retried",
                        "keyspace" => keyspace
                    );
                }
            }
        }
        
        txn.commit().await?;
        Ok(())
    }
}
```

### 4) 垃圾清理

#### GC 主循环

```rust
impl TriggerWorker {
    async fn gc_loop(&self, store: Arc<TikvStore>) {
        let mut interval = tokio::time::interval(
            Duration::from_secs(self.config.gc_interval_sec)
        );
        
        while !self.shutdown.load(Ordering::Relaxed) {
            interval.tick().await;
            
            // 获取所有 keyspace（包括不活跃的，可能有残留数据）
            if let Ok(keyspaces) = self.list_all_keyspaces(&store).await {
                for ks in keyspaces {
                    if let Err(e) = self.gc_keyspace(&store, &ks).await {
                        warn!("Trigger GC error for {}: {}", ks, e);
                    }
                }
            }
        }
    }
    
    async fn list_all_keyspaces(&self, store: &TikvStore) -> Result<Vec<String>> {
        let mut keyspaces = HashSet::new();
        
        // 从活跃列表
        for ks in self.active_keyspaces.iter() {
            keyspaces.insert(ks.clone());
        }
        
        // 从配额表（可能有不活跃但有数据的）
        for entry in self.quotas.iter() {
            keyspaces.insert(entry.key().clone());
        }
        
        Ok(keyspaces.into_iter().collect())
    }
}
```

#### Keyspace GC

```rust
impl TriggerWorker {
    async fn gc_keyspace(&self, store: &TikvStore, keyspace: &str) -> Result<GcStats> {
        let mut stats = GcStats::default();
        let now = now_millis();
        let config = &self.config;
        
        let mut txn = store.begin_pessimistic().await?;
        
        // 1. 恢复孤儿事件（Processing 超时）
        stats.orphans_recovered = self.recover_orphans(
            &mut txn,
            keyspace,
            now - (config.orphan_timeout_sec * 1000) as i64,
        ).await?;
        
        // 2. 清理已完成事件（如果保留）
        stats.done_deleted = self.delete_old_events(
            &mut txn,
            keyspace,
            EventStatus::Done,
            now - (config.done_retention_sec * 1000) as i64,
        ).await?;
        
        // 3. 清理死信队列
        stats.dlq_deleted = self.delete_old_dlq(
            &mut txn,
            keyspace,
            now - (config.dlq_retention_days * 24 * 3600 * 1000) as i64,
        ).await?;
        
        txn.commit().await?;
        
        if stats.has_activity() {
            info!(
                "GC for {}: recovered {} orphans, deleted {} done, {} dlq",
                keyspace, stats.orphans_recovered, stats.done_deleted, stats.dlq_deleted
            );
        }
        
        Ok(stats)
    }
}

#[derive(Default)]
struct GcStats {
    orphans_recovered: usize,
    done_deleted: usize,
    dlq_deleted: usize,
}

impl GcStats {
    fn has_activity(&self) -> bool {
        self.orphans_recovered > 0 || self.done_deleted > 0 || self.dlq_deleted > 0
    }
}
```

#### 孤儿事件恢复

```rust
impl TriggerWorker {
    async fn recover_orphans(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        cutoff: i64,
    ) -> Result<usize> {
        let prefix = format!("_sys_tq_{}_", keyspace);
        let pairs = txn.scan(prefix.as_bytes().to_vec().., 1000).await?;
        
        let mut recovered = 0;
        
        for (key, value) in pairs {
            let mut event: TriggerEvent = bincode::deserialize(&value)?;
            
            // Processing 状态且认领时间超过阈值
            if event.status == EventStatus::Processing {
                if let Some(claimed_at) = event.claimed_at {
                    if claimed_at < cutoff {
                        event.status = EventStatus::Pending;
                        event.worker_id = None;
                        event.claimed_at = None;
                        event.retry_count += 1;
                        
                        txn.put(key, bincode::serialize(&event)?).await?;
                        recovered += 1;
                        
                        // 重新标记为活跃
                        self.active_keyspaces.insert(keyspace.to_string());
                    }
                }
            }
        }
        
        Ok(recovered)
    }
}
```

#### 基于 ID 的高效删除

```rust
impl TriggerWorker {
    async fn delete_old_events(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        status: EventStatus,
        cutoff: i64,
    ) -> Result<usize> {
        // ID 编码了时间戳，可以直接按范围删除
        let cutoff_id = (cutoff as u64) << 22;
        let prefix = format!("_sys_tq_{}_", keyspace);
        
        let pairs = txn.scan(prefix.as_bytes().to_vec().., 1000).await?;
        let mut deleted = 0;
        
        for (key, value) in pairs {
            let event: TriggerEvent = bincode::deserialize(&value)?;
            
            if event.status == status && event.id < cutoff_id {
                txn.delete(key).await?;
                deleted += 1;
                
                self.get_quota(keyspace)
                    .current_depth
                    .fetch_sub(1, Ordering::Relaxed);
            }
        }
        
        Ok(deleted)
    }
    
    async fn delete_old_dlq(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        cutoff: i64,
    ) -> Result<usize> {
        let cutoff_id = (cutoff as u64) << 22;
        let prefix = format!("_sys_tq_dlq_{}_", keyspace);
        
        let pairs = txn.scan(prefix.as_bytes().to_vec().., 1000).await?;
        let mut deleted = 0;
        
        for (key, value) in pairs {
            let event: TriggerEvent = bincode::deserialize(&value)?;
            
            if event.id < cutoff_id {
                txn.delete(key).await?;
                deleted += 1;
            }
        }
        
        Ok(deleted)
    }
}
```

### 5) 多租户性能保障

#### 配额机制

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `max_queue_depth` | 10,000 | 每租户队列上限 |
| `max_events_per_batch` | 10 | 每轮处理数（公平调度） |
| `max_retries` | 3 | 最大重试次数 |

#### 分 Tier Worker Pool（可选增强）

```rust
pub struct TieredTriggerWorker {
    free_tier: WorkerPool,       // 2 threads, batch=10
    pro_tier: WorkerPool,        // 4 threads, batch=50
    enterprise_tier: WorkerPool, // 8 threads, batch=100
    tier_mapping: DashMap<String, Tier>,
}

#[derive(Clone, Copy)]
pub enum Tier {
    Free,
    Pro,
    Enterprise,
}

impl TieredTriggerWorker {
    fn get_pool(&self, keyspace: &str) -> &WorkerPool {
        match self.tier_mapping.get(keyspace).map(|t| *t) {
            Some(Tier::Enterprise) => &self.enterprise_tier,
            Some(Tier::Pro) => &self.pro_tier,
            _ => &self.free_tier,
        }
    }
}
```

### 6) 可观测性

#### 系统函数

```sql
-- 查询 trigger 队列状态
SELECT * FROM _db9_sys_trigger_queue_stats();

-- 返回:
-- keyspace | pending | processing | failed | dlq_count | avg_latency_ms | events_per_min
-- ---------+---------+------------+--------+-----------+----------------+---------------
-- tenant_a |      15 |          2 |      0 |         3 |           12.5 |           450
-- tenant_b |       0 |          0 |      0 |         0 |            8.2 |            20

-- 查询死信队列
SELECT * FROM _db9_sys_trigger_dlq() WHERE keyspace = 'tenant_a';

-- 返回:
-- id | trigger_name | table_name | operation | error_msg | retry_count | created_at
```

#### Prometheus 指标

| 指标 | 类型 | 标签 | 说明 |
|------|------|------|------|
| `trigger_queue_depth` | Gauge | keyspace | 当前队列深度 |
| `trigger_events_completed` | Counter | keyspace | 完成事件数 |
| `trigger_events_retried` | Counter | keyspace | 重试事件数 |
| `trigger_events_dlq` | Counter | keyspace | 进入死信数 |
| `trigger_queue_overflow` | Counter | keyspace | 队列溢出次数 |
| `trigger_execution_duration_ms` | Histogram | keyspace, trigger | 执行耗时 |
| `trigger_gc_runs` | Counter | keyspace | GC 运行次数 |

### 7) 配置

环境变量：

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `DB9_TRIGGER_ENABLED` | `true` | 启用异步 trigger |
| `DB9_TRIGGER_POLL_MS` | `100` | 轮询间隔 |
| `DB9_TRIGGER_BATCH_SIZE` | `10` | 每租户每批次事件数 |
| `DB9_TRIGGER_MAX_RETRIES` | `3` | 最大重试次数 |
| `DB9_TRIGGER_QUEUE_LIMIT` | `10000` | 每租户队列上限 |
| `DB9_TRIGGER_GC_INTERVAL_SEC` | `60` | GC 检查间隔 |
| `DB9_TRIGGER_DONE_RETENTION_SEC` | `3600` | 已完成事件保留时间 |
| `DB9_TRIGGER_DLQ_RETENTION_DAYS` | `7` | 死信保留天数 |
| `DB9_TRIGGER_ORPHAN_TIMEOUT_SEC` | `300` | Processing 超时阈值 |

## 与现有 Trigger 的关系

| Trigger 类型 | 执行方式 | 事务一致性 | 可修改数据 |
|--------------|----------|------------|------------|
| BEFORE ROW | 同步 in-process | 强一致 | 可修改 NEW |
| AFTER ROW | 异步队列 | 最终一致（入队原子） | 否 |
| BEFORE STATEMENT | 不支持 | - | - |
| AFTER STATEMENT | 不支持 | - | - |

## 测试计划

### 单元测试

```rust
#[tokio::test]
async fn test_event_id_ordering() {
    let id1 = generate_event_id();
    tokio::time::sleep(Duration::from_millis(1)).await;
    let id2 = generate_event_id();
    assert!(id2 > id1);
}

#[tokio::test]
async fn test_trigger_enqueue() { ... }

#[tokio::test]
async fn test_fair_scheduling_multiple_keyspaces() { ... }

#[tokio::test]
async fn test_retry_on_failure() { ... }

#[tokio::test]
async fn test_dlq_after_max_retries() { ... }

#[tokio::test]
async fn test_queue_overflow_handling() { ... }

#[tokio::test]
async fn test_orphan_recovery() { ... }

#[tokio::test]
async fn test_gc_deletes_old_events() { ... }
```

### 集成测试

`tests/86_async_triggers.sql`:

```sql
-- Setup
CREATE TABLE audit_log (id SERIAL, action TEXT, ts TIMESTAMP DEFAULT NOW());
CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT);

CREATE OR REPLACE FUNCTION log_user_insert() RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO audit_log (action) VALUES ('user_created: ' || NEW.name);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER user_insert_audit
    AFTER INSERT ON users
    FOR EACH ROW EXECUTE FUNCTION log_user_insert();

-- Test async execution
INSERT INTO users (name) VALUES ('Alice');

-- Wait for async processing
SELECT pg_sleep(0.3);

-- Verify
SELECT action FROM audit_log ORDER BY id;
-- Expected: 'user_created: Alice'

-- Cleanup
DROP TRIGGER user_insert_audit ON users;
DROP FUNCTION log_user_insert;
DROP TABLE users, audit_log;
```

## 实现步骤

### Phase 1：基础队列（2-3 天）
- [ ] TriggerEvent 结构定义
- [ ] 入队逻辑（enqueue_after_triggers）
- [ ] Worker 主循环
- [ ] 事件认领与执行

### Phase 2：可靠性（1-2 天）
- [ ] 重试机制
- [ ] 死信队列
- [ ] 孤儿恢复

### Phase 3：GC（1 天）
- [ ] GC Worker
- [ ] 基于 ID 的高效删除
- [ ] 配置化保留策略

### Phase 4：可观测性（1 天）
- [ ] _db9_sys_trigger_queue_stats()
- [ ] _db9_sys_trigger_dlq()
- [ ] Prometheus 指标

### Phase 5：多租户优化（可选）
- [ ] 分 Tier Worker Pool
- [ ] 动态配额调整

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| Worker 单点故障 | 事件堆积 | 多节点部署 + 孤儿恢复 |
| TiKV 写入热点 | 延迟增加 | 按 keyspace 分散 key |
| 队列无限增长 | OOM | 深度限制 + 监控告警 |
| Trigger 执行慢 | 吞吐下降 | 超时机制 + 熔断 |

## 参考

- PostgreSQL Trigger 文档: https://www.postgresql.org/docs/current/trigger-definition.html
- TiKV 事务模型: https://tikv.org/docs/dev/concepts/transactions/
