#!/usr/bin/env python3
import argparse
import json
from pathlib import Path
from typing import Dict, List, Tuple

import yaml


def load_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def load_yaml(path: Path):
    return yaml.safe_load(path.read_text(encoding="utf-8"))


def lanes_to_commands(coverage_map: dict) -> Dict[str, str]:
    out = {}
    lanes = coverage_map.get("lanes", {})
    for name, cfg in lanes.items():
        runs = cfg.get("run", [])
        out[name] = runs[0] if runs else ""
    return out


def action_template(gap_type: str, gap_action_map: dict) -> dict:
    actions = gap_action_map.get("actions", {})
    if gap_type == "line":
        return actions.get("line_gap", {})
    if gap_type == "statement":
        return actions.get("statement_gap", {})
    return actions.get("scenario_gap", {})


def make_line_task(idx: int, gap: dict, cfg: dict, lane_cmd: Dict[str, str]) -> dict:
    rerun_lanes = cfg.get("rerun_lanes", [])
    return {
        "task_id": f"LINE-{idx:03d}",
        "priority": "P1",
        "task_type": "line_gap",
        "source_gap": {
            "module": gap.get("module"),
            "current": gap.get("current"),
            "target": gap.get("target"),
        },
        "owner_skill": "tipg-test-autopilot",
        "suggest_tests": cfg.get("suggest_tests", []),
        "rerun_lanes": rerun_lanes,
        "run_commands": [lane_cmd.get(l, "") for l in rerun_lanes if lane_cmd.get(l, "")],
        "done_when": [
            f"line coverage for {gap.get('module')} >= {gap.get('target')}",
            "all rerun lanes passed",
        ],
    }


def make_statement_task(idx: int, group_key: Tuple[str, str], cfg: dict, lane_cmd: Dict[str, str]) -> dict:
    stmt, phase = group_key
    rerun_lanes = cfg.get("rerun_lanes", [])
    p0 = phase in {"parse_bind_execute", "describe", "sync"}
    return {
        "task_id": f"STMT-{idx:03d}",
        "priority": "P0" if p0 else "P1",
        "task_type": "statement_gap",
        "source_gap": {
            "statement": stmt,
            "phase": phase,
        },
        "owner_skill": "tipg-test-autopilot",
        "suggest_tests": cfg.get("suggest_tests", []),
        "rerun_lanes": rerun_lanes,
        "run_commands": [lane_cmd.get(l, "") for l in rerun_lanes if lane_cmd.get(l, "")],
        "done_when": [
            f"statement/protocol cell {stmt}@{phase} is covered",
            "all rerun lanes passed",
        ],
    }


def make_scenario_task(idx: int, group_key: Tuple[str, str], cfg: dict, lane_cmd: Dict[str, str]) -> dict:
    orm, cap = group_key
    rerun_lanes = cfg.get("rerun_lanes", [])
    critical_cap = cap in {"crud_basic", "transaction", "prepared_statement"}
    return {
        "task_id": f"SCN-{idx:03d}",
        "priority": "P1" if critical_cap else "P2",
        "task_type": "scenario_gap",
        "source_gap": {
            "orm": orm,
            "capability": cap,
        },
        "owner_skill": "tipg-test-autopilot",
        "suggest_tests": cfg.get("suggest_tests", []),
        "rerun_lanes": rerun_lanes,
        "run_commands": [lane_cmd.get(l, "") for l in rerun_lanes if lane_cmd.get(l, "")],
        "done_when": [
            f"scenario cell {orm}@{cap} is covered",
            "all rerun lanes passed",
        ],
    }


def make_path_task(idx: int, path_gap: dict, lane_cmd: Dict[str, str]) -> dict:
    missing_stmt = [f"{x[0]}@{x[1]}" for x in path_gap.get("missing_statement_cells", [])]
    missing_scn = [f"{x[0]}@{x[1]}" for x in path_gap.get("missing_scenario_any", [])]
    missing_area = path_gap.get("missing_area_any", [])

    rerun_lanes = ["protocol_smoke", "sql_corpus", "orm_all"]
    return {
        "task_id": f"PATH-{idx:03d}",
        "priority": "P0",
        "task_type": "critical_path_gap",
        "source_gap": {
            "path": path_gap.get("path"),
            "desc": path_gap.get("desc", ""),
            "missing_statement_cells": missing_stmt,
            "missing_scenario_any": missing_scn,
            "missing_area_any": missing_area,
        },
        "owner_skill": "tipg-test-autopilot",
        "suggest_tests": [
            "Prioritize required statement/protocol cells for this critical path.",
            "If scenario cells are missing, add orm-tests for the required capabilities.",
            "If area coverage is missing, add minimal SQL integration coverage for that area.",
        ],
        "rerun_lanes": rerun_lanes,
        "run_commands": [lane_cmd.get(l, "") for l in rerun_lanes if lane_cmd.get(l, "")],
        "done_when": [
            f"critical path {path_gap.get('path')} marked covered",
            "all rerun lanes passed",
        ],
    }


def build_backlog(gap_list: dict, path_cov: dict, area_cov: dict, gap_action_map: dict, coverage_map: dict) -> dict:
    lane_cmd = lanes_to_commands(coverage_map)
    tasks: List[dict] = []

    line_cfg = action_template("line", gap_action_map)
    for i, g in enumerate(gap_list.get("line_gaps", []), start=1):
        tasks.append(make_line_task(i, g, line_cfg, lane_cmd))

    stmt_cfg = action_template("statement", gap_action_map)
    stmt_gaps = sorted(
        {(g.get("statement"), g.get("phase")) for g in gap_list.get("statement_gaps", []) if g.get("statement") and g.get("phase")}
    )
    for i, key in enumerate(stmt_gaps, start=1):
        tasks.append(make_statement_task(i, key, stmt_cfg, lane_cmd))

    scn_cfg = action_template("scenario", gap_action_map)
    raw_scn_gaps = gap_list.get("scenario_operation_gaps", gap_list.get("scenario_gaps", []))
    scn_gaps = sorted({(g.get("orm"), g.get("capability")) for g in raw_scn_gaps if g.get("orm") and g.get("capability")})
    for i, key in enumerate(scn_gaps, start=1):
        tasks.append(make_scenario_task(i, key, scn_cfg, lane_cmd))

    missing_paths = [p for p in path_cov.get("paths", []) if not p.get("covered", False)]
    for i, p in enumerate(missing_paths, start=1):
        tasks.append(make_path_task(i, p, lane_cmd))

    # Priority order for deterministic execution.
    prio_rank = {"P0": 0, "P1": 1, "P2": 2}
    tasks.sort(key=lambda t: (prio_rank.get(t.get("priority", "P2"), 3), t["task_id"]))

    summary = {
        "line_gap_count": len(gap_list.get("line_gaps", [])),
        "statement_gap_count": len(gap_list.get("statement_gaps", [])),
        "scenario_gap_count": len(raw_scn_gaps),
        "missing_critical_paths": len(missing_paths),
        "area_global_ratio": area_cov.get("global_ratio"),
        "critical_path_ratio": path_cov.get("ratio"),
    }

    return {
        "status": "ok",
        "schema_version": 1,
        "source": {
            "gap_list": "artifacts/coverage/gap_list.json",
            "critical_path": "artifacts/coverage/critical_path_coverage.json",
            "area_coverage": "artifacts/coverage/area_coverage.json",
            "gap_action_map": "auto_testing/gap_action_map.yaml",
            "coverage_map": "auto_testing/coverage_map.yaml",
        },
        "summary": summary,
        "tasks": tasks,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gap-list", default="artifacts/coverage/gap_list.json")
    ap.add_argument("--critical-path", default="artifacts/coverage/critical_path_coverage.json")
    ap.add_argument("--area-coverage", default="artifacts/coverage/area_coverage.json")
    ap.add_argument("--gap-action-map", default="auto_testing/gap_action_map.yaml")
    ap.add_argument("--coverage-map", default="auto_testing/coverage_map.yaml")
    ap.add_argument("--out", default="artifacts/agent_backlog.json")
    args = ap.parse_args()

    gap_list = load_json(Path(args.gap_list))
    critical_path = load_json(Path(args.critical_path))
    area_coverage = load_json(Path(args.area_coverage))
    gap_action_map = load_yaml(Path(args.gap_action_map))
    coverage_map = load_yaml(Path(args.coverage_map))

    backlog = build_backlog(gap_list, critical_path, area_coverage, gap_action_map, coverage_map)

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(backlog, ensure_ascii=False, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
