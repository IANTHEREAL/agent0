# auto_testing/skills

## 1. Goal

Turn testing from manual decision-making into an agent-executable workflow.

## 2. Core Skills

1. `core/tipg-test-autopilot.md`
2. `core/tipg-pg-oracle-check.md`
3. `core/tipg-failure-triage.md`
4. `core/tipg-pr-readiness-gate.md`

## 3. Recommended Invocation Order

1. `tipg-test-autopilot`: run lanes, collect coverage, generate backlog.
2. `tipg-pg-oracle-check`: validate SQL expectations against PostgreSQL (when matched).
3. `tipg-failure-triage`: classify failures and gaps by root layer.
4. `tipg-pr-readiness-gate`: output final merge decision.

## 4. Relationship with Maps and Rules

1. Lane/rule source: `auto_testing/coverage_map.yaml`
2. Area/path maps: `auto_testing/area_map.yaml`, `auto_testing/path_map.yaml`
3. Gap-to-action mapping: `auto_testing/gap_action_map.yaml`
4. Agent task backlog: `artifacts/agent_backlog.json`
5. Machine workflow protocol: `auto_testing/workflow.yaml`

## 5. Minimal Run Example

```bash
bash scripts/test_orchestrator.sh HEAD~1 HEAD pr
cat artifacts/test_report.json
cat artifacts/agent_backlog.json
```
