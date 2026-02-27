# Testing Asset Guard（防误删说明）

## 1. 目的

这份文档用于告诉人和 agent：哪些测试资产是**机器流程必需**，不能随意删除；哪些是运行产物，可清理不入库。

## 2. 机器流程必需（禁止删除）

这些文件是“coverage map + skill workflow”主链路的一部分，删除会导致自动测试编排失效：

1. `auto_testing/coverage_map.yaml`
- 测试规则总源：mode、lanes、rules、阈值。

2. `auto_testing/area_map.yaml`
- 区域覆盖映射：代码路径 -> area。

3. `auto_testing/path_map.yaml`
- 关键路径定义：required statement/scenario/area。

4. `auto_testing/gap_action_map.yaml`
- gap 到补测动作映射（建议测试与回归 lane）。

5. `auto_testing/workflow.yaml`
- 机器可读 workflow 协议（skills、触发、输入输出、转移规则）。

6. `auto_testing/skills/`
- 核心 skill 规范与执行说明。

## 3. 相关执行器（禁止删除）

以下脚本与上面 map/workflow 构成完整闭环：

1. `scripts/test_orchestrator.sh`
- 总入口：选 lane、执行、汇总 coverage、输出报告。

2. `scripts/coverage_line.sh`
3. `scripts/coverage_statement_protocol.py`
4. `scripts/coverage_scenario_from_vitest.py`
5. `scripts/coverage_area_path.py`
6. `scripts/generate_agent_backlog.py`
- 覆盖采集与 backlog 生成链路。

## 4. 运行产物（可清理，不建议入库）

1. `artifacts/`
- 中间产物：`test_report`、coverage json、agent backlog 快照。

2. `target/`
- Rust/Cargo 构建与 llvm-cov 产物。

3. `test-reports/`
- 回归门禁日志目录。

## 5. 删除前检查规则（给 agent）

任何文件删除前，必须至少通过以下检查：

1. `rg -n "<filename>|<logical_id>" auto_testing scripts` 无引用。
2. 不在 `auto_testing/workflow.yaml` 的 `skills/outputs/policies/source_of_truth` 中。
3. 不在 `scripts/test_orchestrator.sh` 调用链中。
4. 删除后可完成一次最小 dry-run：
```bash
DRY_RUN=1 bash scripts/test_orchestrator.sh HEAD~1 HEAD pr
```

若以上任一不满足，视为高风险删除，禁止执行。

## 6. 维护约定

1. 新增 map/skill/collector 时，必须同步更新本文件。
2. 若流程迁移目录，先改 `workflow.yaml` 与 orchestrator，再改本文件。
3. 以 `auto_testing/coverage_map.yaml` + `auto_testing/workflow.yaml` 为机器真源。
