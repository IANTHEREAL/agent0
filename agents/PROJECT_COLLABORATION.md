# Project Collaboration Primitives (Minimal)

## 最小协作原语（确定版）

### North Star
- 由项目经理创建，不随意更新。
- 字段：Why / What / Not‑Doing / Success Definition / Out of Scope。

### Grand Plan
- 由管理者与团队讨论形成，项目经理定期汇总更新。
- 字段：阶段列表、里程碑、依赖、风险、节奏。

### Stage Packet（阶段执行包）
- 定义：让阶段“可执行”的唯一载体。
- 最小字段：
  - SG（阶段目标）
  - SP（计划：任务/owner/依赖/风险）
  - SRP（发布：窗口/回滚/发布清单）
  - DoD（验收标准）
- 由项目经理牵头，架构师/Reviewer 提供输入。

### Shared State Log（在任务帖内维护）
- 单一事实源：owner、测试修复、冲突、仲裁、回归守卫。
- 放在**对应任务的 Minibook 帖子/留言**中维护（不另建索引）。
- 开发者持续更新；管理者与架构师必须定期查看。

### Decision Record (DR)
- 关键决策追溯（PRFAQ/ADR）。
- 所有影响行为/契约/测试策略的决策都要有 DR。

### Conflict Log（在任务帖内维护）
- 可单独存在，也可作为 Shared State Log 的一部分。
- 记录矛盾点、证据、参与人、问题、状态、仲裁结论。

## 角色与协作规则（按当前协作要求）

### 任务分配
- 项目经理与架构师可以分配新任务。

### Review 协作
- 开发者可以 @Reviewer 做 code review。
- Reviewer 可以 @Developer 和 @Triage 处理 issue。
- Triage 可以 @Developer 修复 issue。

## 冲突/不一致处理（强制）

- 开发中发现任何不一致，必须在 Minibook 发帖说明矛盾点并 @相关人员。
- 如果无法达成一致，引入管理员（项目经理）+ 架构师仲裁。
- 最终结论必须记录在任务帖的 Conflict Log / Shared State Log + DR。

## 测试与标准

- 测试执行与标准由 Reviewer 主导。
- 管理者与架构师提供建议，但 Reviewer 负责验收。

## Issue Triage

- Reviewer 发现新 bug → Triage 复现与归因 → 分派修复。
- Triage 需要看历史 github PR/issue，判断引入责任并建立修复链路。

## 管理者/架构师的持续职责

- 定期查看任务帖的 Shared State Log / DR / Conflict Log。
- 跟踪 GitHub issue/PR，发现遗漏风险并重新排期。

## Minibook 规则（必须）

- 所有活动讨论与结论必须在 Minibook 中出现。
- Minibook 的维护尽可能符合“准饭”。
- 北极星、Grand Plan 与最新 Stage Packet 可置顶。
- 共享状态与冲突记录放在对应任务帖的留言中，不额外维护索引帖。

## 本地 Worklog（个人使用）

- 本地 worklog 仅用于个人复盘与记录，不作为协作依据。
- 不进入团队协作流程，也不替代 Minibook 的讨论与结论。
