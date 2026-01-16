# 设计：LISTEN / NOTIFY

**Status**: Draft  
**Priority（ORM 迁移）**: P3

## 背景与动机

`LISTEN/NOTIFY` 是 PostgreSQL 常用的轻量 pub/sub 原语，常用于：
- 缓存失效通知
- 任务触发/事件广播

它通常不是 ORM migration 的硬依赖，但在部分应用迁移/初始化脚本中会出现，因此需要明确设计与实现边界。

## 目标（MVP）

- 支持：
  - `LISTEN channel`
  - `UNLISTEN channel` / `UNLISTEN *`
  - `NOTIFY channel [, 'payload']`
- 多租户隔离：不同 TiKV keyspace（tenant）之间互不影响

## 非目标（MVP 不做）

- 跨进程/跨节点的通知（pg-tikv 多实例时的分布式 NOTIFY）
- 通知持久化（进程重启后丢失，符合 PG 的“非持久化”直觉）

## 设计概览

### 1) In-process pub/sub Registry（按 keyspace 隔离）

实现一个全局 registry（进程内）：
- Key：`(keyspace, channel)`
- Value：`tokio::sync::broadcast::Sender<Notification>`

连接在 `LISTEN` 时为该 channel 拿到一个 `Receiver` 并注册到 connection/session 上。

### 2) pgwire 支持：异步 NotificationResponse

PG 协议的 NOTIFY 会以异步消息（`NotificationResponse`）发给客户端。

需要评估 pgwire crate 是否提供：
- 主动向 client sink 发送异步消息的能力
- 或者在 connection loop 中注入消息

若 pgwire 不支持“server push”，则 LISTEN/NOTIFY 需要在协议层做更底层的实现（优先级较低，可延后）。

### 3) SQL 执行路径

在 `Executor::execute()` 的 statement dispatch 中补齐：
- `Statement::Listen` / `Statement::Notify` / `Statement::Unlisten`（若 sqlparser 支持）
或采用字符串前缀解析（与现有 REFRESH/CALL 等策略一致）。

执行时更新 session 的监听集合，并向 registry 发送通知。

## 测试计划

### 集成测试（推荐 Python/psql，`./run_tests.sh`）

新增一个 Python 集成测试（需要两条连接并发）：
1. 连接 A：`LISTEN ch;`
2. 连接 B：`NOTIFY ch, 'payload';`
3. 连接 A：应收到通知（psql 会打印 `Asynchronous notification`）

并覆盖：
- `UNLISTEN` 后不再收到
- keyspace 隔离：tenant_a listen，tenant_b notify 不应触达

