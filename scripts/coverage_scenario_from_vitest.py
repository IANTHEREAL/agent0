#!/usr/bin/env python3
import argparse
import json
import glob
from pathlib import Path

ORMS = ["typeorm", "prisma", "sequelize", "knex", "drizzle", "kysely", "sqlalchemy", "dify_sqlalchemy", "gorm"]
CAPABILITIES = ["ddl_lifecycle", "crud_basic", "transaction", "prepared_statement", "json_and_array", "join_and_subquery", "vector"]


def detect_capabilities(test_name: str):
    t = test_name.lower()
    caps = set()
    if any(k in t for k in ["schema", "ddl", "create", "drop", "migrate"]):
        caps.add("ddl_lifecycle")
    if any(k in t for k in ["crud", "insert", "update", "delete", "select"]):
        caps.add("crud_basic")
    if any(k in t for k in ["transaction", "commit", "rollback", "savepoint"]):
        caps.add("transaction")
    if any(k in t for k in ["prepared", "bind", "parameter"]):
        caps.add("prepared_statement")
    if any(k in t for k in ["json", "array"]):
        caps.add("json_and_array")
    if any(k in t for k in ["join", "subquery", "cte", "window", "query"]):
        caps.add("join_and_subquery")
    if "vector" in t:
        caps.add("vector")
    return caps


def orm_from_path(path: str):
    p = path.lower()
    for orm in ["typeorm", "prisma", "sequelize", "knex", "drizzle", "kysely"]:
        if f"/{orm}/" in p or p.startswith(f"{orm}/"):
            return orm
    if "sqlalchemy" in p and "dify" in p:
        return "dify_sqlalchemy"
    if "sqlalchemy" in p:
        return "sqlalchemy"
    if "gorm" in p:
        return "gorm"
    return None


def add_e2e_inferred_caps(covered, evidence):
    # Supplement scenario coverage with e2e suites that are outside vitest.
    # These suites are first-class compatibility evidence for sqlalchemy/gorm/dify.
    e2e_patterns = [
        ("sqlalchemy", "e2e/sqlalchemy_smoke/**/*.py"),
        ("dify_sqlalchemy", "e2e/dify_sqlalchemy_compat/**/*.py"),
        ("gorm", "e2e/gorm_smoke/**/*.go"),
    ]

    for orm, pattern in e2e_patterns:
        for path in glob.glob(pattern, recursive=True):
            p = Path(path)
            if not p.is_file():
                continue
            text = p.read_text(encoding="utf-8", errors="ignore")
            caps = detect_capabilities(p.name + " " + text)
            for cap in caps:
                key = (orm, cap)
                covered.add(key)
                evidence.setdefault(key, []).append(path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="input_json", default="orm-tests/test-results.json")
    ap.add_argument("--out", default="artifacts/coverage/scenario_coverage.json")
    args = ap.parse_args()

    covered = set()
    evidence = {}

    input_path = Path(args.input_json)
    status = "ok"
    if input_path.exists():
        data = json.loads(input_path.read_text(encoding="utf-8"))
        for suite in data.get("testResults", []):
            orm = orm_from_path(str(suite.get("name", "")))
            if not orm:
                continue
            for case in suite.get("assertionResults", []):
                if case.get("status") != "passed":
                    continue
                name = " > ".join(case.get("ancestorTitles", []) + [case.get("title", "")])
                caps = detect_capabilities(name)
                for cap in caps:
                    key = (orm, cap)
                    covered.add(key)
                    evidence.setdefault(key, []).append(name)
    else:
        # Fallback: static inference from ORM test files when execution result is unavailable.
        status = "static_inferred"
        for path in glob.glob("orm-tests/*/*.test.ts"):
            orm = orm_from_path(path)
            if not orm:
                continue
            name = Path(path).name
            txt = Path(path).read_text(encoding="utf-8", errors="ignore")
            caps = detect_capabilities(name + " " + txt)
            for cap in caps:
                key = (orm, cap)
                covered.add(key)
                evidence.setdefault(key, []).append(path)

    # e2e suites as scenario evidence (outside vitest).
    add_e2e_inferred_caps(covered, evidence)

    universe = {(o, c) for o in ORMS for c in CAPABILITIES}
    uncovered = sorted(list(universe - covered))
    ratio = (len(covered) / len(universe)) if universe else 0.0

    out = {
        "status": status,
        "orms": ORMS,
        "capabilities": CAPABILITIES,
        "covered_cells": [{"orm": o, "capability": c} for (o, c) in sorted(covered)],
        "uncovered_cells": [{"orm": o, "capability": c} for (o, c) in uncovered],
        "covered": len(covered),
        "total": len(universe),
        "ratio": round(ratio, 4),
        "evidence": [
            {"orm": o, "capability": c, "sources": sorted(set(srcs))}
            for (o, c), srcs in sorted(evidence.items())
        ],
    }

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(out, ensure_ascii=False, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
