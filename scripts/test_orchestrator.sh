#!/usr/bin/env bash
set -euo pipefail

BASE_SHA="${1:-}"
HEAD_SHA="${2:-HEAD}"
MODE="${3:-pr}"
DSN="${4:-${PG_DSN:-postgres://admin:admin@127.0.0.1:5433/postgres}}"
DRY_RUN="${DRY_RUN:-0}"

if [[ -z "$BASE_SHA" ]]; then
  BASE_SHA="$(git rev-parse HEAD~1 2>/dev/null || git rev-parse HEAD)"
fi

export BASE_SHA HEAD_SHA MODE DSN DRY_RUN

python3 - <<'PY'
import fnmatch
import json
import os
import subprocess
import sys
import time
from pathlib import Path
import yaml

base_sha = os.environ["BASE_SHA"]
head_sha = os.environ["HEAD_SHA"]
mode = os.environ["MODE"]
dsn = os.environ["DSN"]
dry_run = os.environ.get("DRY_RUN", "0") == "1"

art = Path("artifacts")
logs_dir = art / "logs"
cov_dir = art / "coverage"
logs_dir.mkdir(parents=True, exist_ok=True)
cov_dir.mkdir(parents=True, exist_ok=True)

cfg_path = Path("auto_testing/coverage_map.yaml")
if not cfg_path.exists():
    raise SystemExit(f"missing config: {cfg_path}")
cfg = yaml.safe_load(cfg_path.read_text(encoding="utf-8")) or {}

meta = cfg.get("meta", {})
fail_if_unmapped_change = bool(meta.get("fail_if_unmapped_change", False))

mode_defaults = cfg.get("mode_defaults", {})
rules = cfg.get("rules", [])
lanes_raw = cfg.get("lanes", {})
order = list(lanes_raw.keys())
lanes = {}
for lane, lane_cfg in lanes_raw.items():
    lanes[lane] = {
        "run": lane_cfg.get("run", []),
        "timeout": int(lane_cfg.get("timeout_sec", 1800)),
        "retry": int(lane_cfg.get("retry", 0)),
        "fail_fast": bool(lane_cfg.get("fail_fast", False)),
    }

cov_dim = cfg.get("coverage_dimensions", {})
line_targets = (cov_dim.get("line", {}) or {}).get("targets_by_mode", {})
stmt_targets = (cov_dim.get("pg_protocol_statement", {}) or {}).get("required_min_ratio_by_mode", {})
scn_targets = (cov_dim.get("scenario", {}) or {}).get("required_min_ratio_by_mode", {})

def run(cmd, timeout, log_file):
    env = os.environ.copy()
    env["DSN"] = dsn
    shell_cmd = cmd.replace("${DSN}", dsn)
    start = time.time()
    with log_file.open("w", encoding="utf-8") as f:
        p = subprocess.Popen(shell_cmd, shell=True, stdout=f, stderr=subprocess.STDOUT, env=env)
        try:
            code = p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            p.kill()
            code = 124
    return code, round(time.time() - start, 2), shell_cmd

# changed files
try:
    diff = subprocess.check_output(["git", "diff", "--name-only", f"{base_sha}..{head_sha}"], text=True)
    changed_files = [x.strip() for x in diff.splitlines() if x.strip()]
except Exception:
    changed_files = []

default_mode_lanes = mode_defaults.get("pr", [])
selected = list(mode_defaults.get(mode, default_mode_lanes))
matched_files = set()
for r in rules:
    hit = False
    for f in changed_files:
        if any(fnmatch.fnmatch(f, pat) for pat in r["when_changed"]):
            hit = True
            matched_files.add(f)
            break
    if hit:
        selected.extend(r["include"])
        for ex in r.get("exclude", []):
            selected = [x for x in selected if x != ex]

# dedupe + order
selected = [l for l in order if l in set(selected)]
unmapped_files = [f for f in changed_files if f not in matched_files]

results = []
merge_allowed = True
rerun = []

for lane in selected:
    cfg = lanes[lane]
    attempts = cfg["retry"] + 1
    ok = False
    last_code = 1
    duration = 0.0
    command_used = ""
    if dry_run:
        log_file = logs_dir / f"{lane}.log"
        command_used = cfg["run"][0].replace("${DSN}", dsn)
        log_file.write_text(f"DRY_RUN=1 skipped execution\ncommand={command_used}\n", encoding="utf-8")
        ok = True
        last_code = 0
        duration = 0.0
    else:
        for i in range(attempts):
            log_file = logs_dir / f"{lane}.log"
            code, duration, command_used = run(cfg["run"][0], cfg["timeout"], log_file)
            last_code = code
            if code == 0:
                ok = True
                break
    status = "passed" if ok else "failed"
    results.append({
        "lane": lane,
        "status": status,
        "duration_sec": duration,
        "exit_code": last_code,
        "log": str(logs_dir / f"{lane}.log"),
        "command": command_used,
    })
    rerun.append(command_used)

    if not ok:
        merge_allowed = False
        if cfg["fail_fast"]:
            break

# collectors
subprocess.call(["bash", "scripts/coverage_line.sh", str(cov_dir / "line_coverage.json")])
subprocess.call(["python3", "scripts/coverage_statement_protocol.py", "--out", str(cov_dir / "statement_protocol_coverage.json")])
subprocess.call(["python3", "scripts/coverage_scenario_from_vitest.py", "--out", str(cov_dir / "scenario_coverage.json")])
subprocess.call(["python3", "scripts/coverage_area_path.py",
                 "--line-cov", str(cov_dir / "line_coverage.json"),
                 "--stmt-cov", str(cov_dir / "statement_protocol_coverage.json"),
                 "--scn-cov", str(cov_dir / "scenario_coverage.json"),
                 "--out-area", str(cov_dir / "area_coverage.json"),
                 "--out-path", str(cov_dir / "critical_path_coverage.json")])

# read coverage ratios
line_ratio = None
line_data = {}
line_path = cov_dir / "line_coverage.json"
if line_path.exists():
    try:
        line_data = json.loads(line_path.read_text(encoding="utf-8"))
        line_ratio = line_data.get("global_ratio")
        if line_ratio is None and isinstance(line_data.get("data"), list):
            covered = 0
            total = 0
            for d in line_data.get("data", []):
                for f in d.get("files", []):
                    s = f.get("summary", {}).get("lines", {})
                    covered += int(s.get("covered", 0))
                    total += int(s.get("count", 0))
            if total > 0:
                line_ratio = round(covered / total, 4)
    except Exception:
        line_data = {}

stmt_data = json.loads((cov_dir / "statement_protocol_coverage.json").read_text(encoding="utf-8"))
scn_data = json.loads((cov_dir / "scenario_coverage.json").read_text(encoding="utf-8"))

mode_line_cfg = line_targets.get(mode, {})
line_target = mode_line_cfg.get("global_min", mode_line_cfg.get("changed_module_min", 0.60))
statement_target = stmt_targets.get(mode, 0.65)
scenario_target = scn_targets.get(mode, 0.55)
statement_ratio = stmt_data.get("ratio", 0.0)
scenario_ratio = scn_data.get("ratio", 0.0)

line_gaps = []
if isinstance(line_ratio, (int, float)) and line_ratio < line_target:
    line_gaps.append({"module": "global", "current": round(line_ratio, 4), "target": line_target})

statement_gaps = stmt_data.get("uncovered_cells", [])[:50]
scenario_gaps = scn_data.get("uncovered_cells", [])[:50]

if statement_ratio < statement_target or scenario_ratio < scenario_target or line_gaps:
    merge_allowed = False
if fail_if_unmapped_change and unmapped_files:
    merge_allowed = False

gap_list = {
    "line_gaps": line_gaps,
    "statement_gaps": statement_gaps,
    "scenario_gaps": scenario_gaps,
}
(cov_dir / "gap_list.json").write_text(json.dumps(gap_list, ensure_ascii=False, indent=2), encoding="utf-8")

# refresh agent backlog now that gap_list.json is finalized
subprocess.call(["python3", "scripts/generate_agent_backlog.py",
                 "--gap-list", str(cov_dir / "gap_list.json"),
                 "--critical-path", str(cov_dir / "critical_path_coverage.json"),
                 "--area-coverage", str(cov_dir / "area_coverage.json"),
                 "--out", str(art / "agent_backlog.json")])

backlog_task_count = 0
backlog_path = art / "agent_backlog.json"
if backlog_path.exists():
    try:
        backlog_data = json.loads(backlog_path.read_text(encoding="utf-8"))
        backlog_task_count = len(backlog_data.get("tasks", []))
    except Exception:
        backlog_task_count = 0

report = {
    "base_sha": base_sha,
    "head_sha": head_sha,
    "mode": mode,
    "dry_run": dry_run,
    "changed_files": changed_files,
    "selected_lanes": selected,
    "merge_allowed": merge_allowed,
    "results": results,
    "coverage_summary": {
        "line_ratio": line_ratio,
        "line_target": line_target,
        "statement_ratio": statement_ratio,
        "statement_target": statement_target,
        "scenario_ratio": scenario_ratio,
        "scenario_target": scenario_target,
    },
    "config_source": str(cfg_path),
    "unmapped_changed_files": unmapped_files,
    "gaps": gap_list,
    "agent_backlog": {
        "path": str(backlog_path),
        "task_count": backlog_task_count,
    },
    "rerun_commands": rerun,
}

(art / "test_report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")

md = []
md.append("# Test Report")
md.append("")
md.append(f"- mode: `{mode}`")
md.append(f"- base_sha: `{base_sha}`")
md.append(f"- head_sha: `{head_sha}`")
md.append(f"- merge_allowed: `{merge_allowed}`")
md.append("")
md.append("## Selected Lanes")
for lane in selected:
    md.append(f"- `{lane}`")
md.append("")
md.append("## Lane Results")
for r in results:
    md.append(f"- `{r['lane']}`: {r['status']} ({r['duration_sec']}s), log=`{r['log']}`")
md.append("")
md.append("## Coverage Summary")
md.append(f"- line_ratio: `{line_ratio}`")
md.append(f"- line_target: `{line_target}`")
md.append(f"- statement_ratio: `{statement_ratio}` (target `{statement_target}`)")
md.append(f"- scenario_ratio: `{scenario_ratio}` (target `{scenario_target}`)")
md.append(f"- agent_backlog_tasks: `{backlog_task_count}`")
md.append("")
md.append("## Config/Rule Summary")
md.append(f"- config_source: `{cfg_path}`")
md.append(f"- unmapped_changed_files: `{len(unmapped_files)}`")
for f in unmapped_files[:20]:
    md.append(f"- unmapped: `{f}`")
md.append("")
md.append("## Gap Summary")
md.append(f"- line_gaps: `{len(line_gaps)}`")
md.append(f"- statement_gaps: `{len(statement_gaps)}`")
md.append(f"- scenario_gaps: `{len(scenario_gaps)}`")
md.append("")
md.append("## Rerun Commands")
for c in rerun:
    md.append(f"- `{c}`")
(art / "test_report.md").write_text("\n".join(md) + "\n", encoding="utf-8")

print("wrote artifacts/test_report.json")
print("wrote artifacts/test_report.md")
print("wrote artifacts/coverage/gap_list.json")
PY
