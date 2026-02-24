# 设计：SAVEPOINT（嵌套事务） [DONE]

**Status**: Draft  
**Priority（ORM 迁移）**: P0

## 背景与动机

多数 ORM 会用 SAVEPOINT 来实现“嵌套事务”（或者把用户代码中的多层 transaction 抽象为 savepoint），例如：
- TypeORM：在已开启事务中再次 `startTransaction()` 通常会退化为 `SAVEPOINT`。
- Knex/Sequelize：在部分驱动/配置下也会通过 savepoint 模拟嵌套事务语义。

当前 db9-server 仅支持 `BEGIN/COMMIT/ROLLBACK`（见 `src/sql/session.rs`、`src/sql/executor.rs`），缺少 `SAVEPOINT / ROLLBACK TO SAVEPOINT / RELEASE SAVEPOINT`，会导致嵌套事务相关的 ORM 用例/生产逻辑失败。

## 目标

- 支持以下语句（PostgreSQL 兼容语义）：
  - `SAVEPOINT <name>`
  - `ROLLBACK TO SAVEPOINT <name>`（以及 `ROLLBACK TO <name>` 的常见变体）
  - `RELEASE SAVEPOINT <name>`（以及 `RELEASE <name>` 的常见变体）
- 语义要求：
  - 仅允许在事务块内使用（PG 行为：事务外使用报错）。
  - `ROLLBACK TO SAVEPOINT` 回滚到保存点创建时的状态，并**重新建立**该保存点（之后仍可继续使用同名 savepoint）。
  - `RELEASE SAVEPOINT` 删除保存点，但不会回滚已做的变更；并且这些变更在更外层 savepoint 回滚时仍需可被回滚。
- 性能：savepoint 未启用时不引入可感知开销；启用时开销与“修改的键数量”线性相关且可控。

## 非目标（MVP 不做）

- 事务外 `SAVEPOINT` 的隐式 BEGIN（PG 不支持；我们也不支持）。
- 分布式节点级“持久化 undo log”（依赖 TiKV 事务提供的原子性/隔离性，savepoint 仅在单事务内部实现）。
- 与 SQL 函数语言（PL/pgSQL）联动的复杂行为（先把 savepoint 作为 SQL 层事务原语补齐）。

## 现状（代码）

- `Session` 只有 `Idle/Active(Transaction)`，没有保存点栈（`src/sql/session.rs`）。
- `Executor::execute()` 仅匹配 `BEGIN/COMMIT/ROLLBACK`（`src/sql/executor.rs`），没有 savepoint 相关 Statement 分支。

## 设计概览

### 1) 写入拦截：在 `put/delete` 处记录 undo

TiKV 的 txn API 本身不提供 savepoint，因此必须在 db9-server 侧记录“写入前的旧值（before image）”，并在 `ROLLBACK TO SAVEPOINT` 时把这些 key 恢复回去。

为了避免把 `&mut Transaction` 全链路替换成新类型（侵入面大、风险高），实现采用**集中写入点 hook**：

- 把 `Transaction::put/delete` 的所有调用点收敛到 `crate::txn::{txn_put, txn_delete}`（目前仅两处：`src/storage/tikv_store.rs`、`src/auth/rbac.rs`）。
- 在 `Executor::execute()` 入口用 `tokio::task_local!` 设置 `Arc<SavepointState>`（内部是 `Mutex<SavepointManager>` + `AtomicBool active`），让 store/auth 侧无需改签名即可访问 savepoint 上下文，同时避免 raw pointer/UB 风险。

性能：savepoint 未启用（栈为空）时，`txn_put/txn_delete` 仅做一次 task-local 查询 + 原子读 + 分支，不触发 `Mutex` 加锁，避免额外分配。

### 2) Undo log：KV 级回滚（覆盖 DDL+DML）

savepoint 的本质是“在同一事务内回滚部分写入”。在 db9-server 中，所有持久化变更最终都会落到一组 TiKV KV 写入（schema key、row key、index key、权限 key 等）。

因此 MVP 推荐做 **KV 级 undo log**，天然覆盖 DDL + DML：

#### 数据结构

```text
Savepoint {
  name: String,
  // key -> prev（None 表示当时不存在，回滚时应 delete）
  // 去重：同一 savepoint 内同一 key 只记录第一次的 prev
  undo: HashMap<Vec<u8>, Option<Vec<u8>>>,
}
```

#### 记录规则

- 仅当 `savepoints.len() > 0` 时启用记录（否则完全不记录）。
- 每次 `put/delete` 之前：
  - 如果当前 savepoint 的 `undo` 中没有该 key：
    - 读取事务视角下的旧值：`prev = txn.get(key).await?`
    - 插入 `undo[key] = prev`
  - 然后执行实际的 `put/delete`

#### 回滚规则

- `ROLLBACK TO SAVEPOINT sp`：
  - 从栈顶开始弹出 savepoint，直到找到“最靠近栈顶、名字匹配”的 `sp`（PG 行为：允许同名，回滚到最近的那个）。
  - 对每个被弹出的 savepoint（内层→外层）应用其 `undo`：
    - `Some(bytes)` => `txn.put(key, bytes)`
    - `None` => `txn.delete(key)`
  - 对目标 savepoint `sp` 本身也应用其 `undo` 并清空（回到创建时状态），等价于“重新建立 savepoint”。

#### 释放规则（关键）

`RELEASE SAVEPOINT sp` 不能简单丢弃 `sp.undo`，否则会丢失“回滚到更外层 savepoint”所需的信息。

正确做法：把被释放的 savepoint（以及其上方嵌套 savepoint）的 `undo` **合并**到它的上一层 savepoint（若存在），并且按外层→内层顺序合并，确保“更早的 prev 值”优先：
- 对于每个 `(key, prev)`：
  - 如果上一层还没有该 key，则插入
  - 否则忽略（上一层记录的是更早的 prev 值，必须保留）

栈中只有一个 savepoint 时，`RELEASE` 直接丢弃即可（因为已经没有更外层 savepoint 需要回滚到）。

### 3) SQL 入口：解析/执行 SAVEPOINT 语句

优先使用 `sqlparser-rs` 的 AST（如果版本覆盖）：
- `Statement::Savepoint { ... }`
- `Statement::Rollback { ... }` + `rollback_to` 变体
- `Statement::ReleaseSavepoint { ... }`

若 `sqlparser-rs` 不支持，则在 `Executor::execute()` 入口做与现有 `REFRESH MATERIALIZED VIEW` 类似的轻量字符串识别/解析（仅限 savepoint 相关语句），避免引入复杂 parser。

### 4) 错误语义（与 ORM 兼容优先）

- 事务外执行 `SAVEPOINT/ROLLBACK TO/RELEASE`：返回 error（与 PG 一致）。
- savepoint 不存在：
  - `ROLLBACK TO`：error
  - `RELEASE`：error

## 实现步骤（建议分阶段）

1. 引入 `SavepointManager`（纯逻辑模块）+ 单元测试
2. 引入 `SavepointState`（`Mutex` + `AtomicBool`）并通过 `tokio::task_local!` 透传到写路径
3. 将写路径集中到 `crate::txn::{txn_put, txn_delete}`（覆盖 storage/auth）
4. 在 `Executor` 中加入 `SAVEPOINT/ROLLBACK TO/RELEASE` 执行分支
5. 补齐 SQL/ORM 测试（优先覆盖 ORM 的嵌套事务）

## 测试计划

### 单元测试（`cargo test`）

新增：`src/txn/savepoints.rs` 对 `SavepointManager` 做纯逻辑测试：
- `release` 合并语义：内层新增 key 不覆盖外层已有 key 的 prev
- `rollback_to`：同名 savepoint 取最近一个；回滚后该 savepoint 仍存在且 undo 清空
- 多层嵌套：A->B->C 修改不同 key，回滚到 B 只回滚 B/C 范围

### 集成测试（SQL，`./run_tests.sh`）

`./run_tests.sh` 默认只跑 `scripts/integration_test.py` 的 built-in tests，因此把 savepoint 覆盖加到 built-in 测试里：
- `scripts/integration_test.py`：新增 `test_savepoints()` 覆盖：
  - `SAVEPOINT / ROLLBACK TO / RELEASE` 正例
  - 嵌套 savepoint + `RELEASE` 后回滚外层仍能撤销内层变更
  - 事务外 `SAVEPOINT/ROLLBACK TO/RELEASE` 报错

（可选）DDL 场景（如果采用 KV 级 undo log）：
- `BEGIN; CREATE TABLE ...; SAVEPOINT a; ALTER TABLE ...; ROLLBACK TO a;` 验证 schema 回滚

### ORM 测试（`./run_tests.sh` 的 ORM 阶段）

新增/补齐：
- `orm-tests/typeorm/transaction.test.ts`：嵌套事务触发 savepoint（或直接执行 `SAVEPOINT` SQL）验证回滚范围正确
- `orm-tests/knex/transaction.test.ts`：嵌套 transaction 的行为（若当前测试覆盖不足，补最小用例）
