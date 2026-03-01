#!/usr/bin/env python3
import argparse
import glob
import json
from pathlib import Path

import yaml


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


def detect_capability(text: str):
    t = text.lower()
    caps = set()
    if any(k in t for k in ["schema", "ddl", "create", "drop", "migrate", "alter", "index"]):
        caps.add("ddl_lifecycle")
    if any(k in t for k in ["crud", "insert", "update", "delete", "select", "upsert", "returning"]):
        caps.add("crud_basic")
    if any(k in t for k in ["transaction", "commit", "rollback", "savepoint", "isolation", "nested"]):
        caps.add("transaction")
    if any(k in t for k in ["prepared", "bind", "parameter", "metadata"]):
        caps.add("prepared_statement")
    if any(k in t for k in ["json", "array"]):
        caps.add("json_and_array")
    if any(k in t for k in ["join", "subquery", "cte", "window", "having"]):
        caps.add("join_and_subquery")
    if "vector" in t:
        caps.add("vector")
    return caps


def detect_operations(text: str):
    t = text.lower()
    ops = set()
    # ddl_lifecycle
    if "create table" in t or "create_table" in t:
        ops.add(("ddl_lifecycle", "create_table"))
    if "alter table" in t or "alter_table" in t:
        ops.add(("ddl_lifecycle", "alter_table"))
    if "drop table" in t or "drop_table" in t:
        ops.add(("ddl_lifecycle", "drop_table"))
    if "create index" in t or "create_index" in t:
        ops.add(("ddl_lifecycle", "create_index"))
    if "drop index" in t or "drop_index" in t:
        ops.add(("ddl_lifecycle", "drop_index"))
    if "create schema" in t or "create_schema" in t:
        ops.add(("ddl_lifecycle", "create_schema"))
    if "drop schema" in t or "drop_schema" in t:
        ops.add(("ddl_lifecycle", "drop_schema"))
    if "migrate" in t or "migration" in t:
        ops.add(("ddl_lifecycle", "migrate"))

    # crud_basic
    if "insert" in t:
        ops.add(("crud_basic", "insert"))
    if "select" in t:
        ops.add(("crud_basic", "select"))
    if "update" in t:
        ops.add(("crud_basic", "update"))
    if "delete" in t:
        ops.add(("crud_basic", "delete"))
    if "upsert" in t or "on conflict" in t:
        ops.add(("crud_basic", "upsert"))
    if "returning" in t:
        ops.add(("crud_basic", "returning"))
    if "batch" in t or "bulk" in t:
        ops.add(("crud_basic", "batch_write"))

    # transaction
    if "begin" in t and "commit" in t:
        ops.add(("transaction", "begin_commit"))
    if "rollback" in t:
        ops.add(("transaction", "rollback"))
    if "savepoint" in t:
        ops.add(("transaction", "savepoint"))
    if "nested transaction" in t or "nested" in t:
        ops.add(("transaction", "nested_tx"))
    if "isolation" in t:
        ops.add(("transaction", "isolation_level"))

    # prepared_statement
    if "positional" in t or "$1" in t:
        ops.add(("prepared_statement", "positional_bind"))
    if "named prepared" in t or "named statement" in t:
        ops.add(("prepared_statement", "named_bind"))
    if "re-exec" in t or "reexecute" in t or "re-execute" in t or "repeated" in t:
        ops.add(("prepared_statement", "repeated_execute"))
    if "metadata" in t or "describe" in t:
        ops.add(("prepared_statement", "prepared_metadata"))

    # json_and_array
    if "json" in t and any(k in t for k in ["insert", "create"]):
        ops.add(("json_and_array", "json_insert"))
    if "json" in t and any(k in t for k in ["query", "select", "path", "extract"]):
        ops.add(("json_and_array", "json_query"))
    if "json" in t and "update" in t:
        ops.add(("json_and_array", "json_update"))
    if "array" in t and any(k in t for k in ["insert", "create"]):
        ops.add(("json_and_array", "array_insert"))
    if "array" in t and any(k in t for k in ["query", "select", "contains"]):
        ops.add(("json_and_array", "array_query"))

    # join_and_subquery
    if "inner join" in t:
        ops.add(("join_and_subquery", "inner_join"))
    if "left join" in t:
        ops.add(("join_and_subquery", "left_join"))
    if "group by" in t or "having" in t:
        ops.add(("join_and_subquery", "group_having"))
    if "subquery" in t or "exists (" in t or " in (" in t:
        ops.add(("join_and_subquery", "subquery"))
    if "cte" in t or "with recursive" in t or "with " in t:
        ops.add(("join_and_subquery", "cte"))
    if "over (" in t or "window" in t or "row_number" in t:
        ops.add(("join_and_subquery", "window"))

    # vector
    if "vector" in t and "column" in t:
        ops.add(("vector", "vector_column"))
    if "vector" in t and "insert" in t:
        ops.add(("vector", "vector_insert"))
    if any(k in t for k in ["cosine", "l2", "distance", "<->"]):
        ops.add(("vector", "vector_distance"))
    if "hnsw" in t or ("vector" in t and "index" in t):
        ops.add(("vector", "vector_index"))
    if "vector" in t and any(k in t for k in ["where", "filter"]):
        ops.add(("vector", "vector_filter"))
    return ops


def read_text(path: Path):
    return path.read_text(encoding="utf-8", errors="ignore")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="input_json", default="orm-tests/test-results.json")
    ap.add_argument("--map", dest="map_yaml", default="auto_testing/scenario_operation_map.yaml")
    ap.add_argument("--out", default="artifacts/coverage/scenario_operation_coverage.json")
    args = ap.parse_args()

    cfg = yaml.safe_load(Path(args.map_yaml).read_text(encoding="utf-8"))
    orms = cfg["orms"]
    cap_ops = cfg["capability_operations"]
    universe = {(o, cap, op) for o in orms for cap, ops in cap_ops.items() for op in ops}

    covered = set()
    evidence = {}
    status = "ok"
    input_path = Path(args.input_json)
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
                found = detect_operations(name)
                if not found:
                    # Fallback to coarse capability-hit if operation words absent.
                    for cap in detect_capability(name):
                        for op in cap_ops.get(cap, []):
                            cell = (orm, cap, op)
                            covered.add(cell)
                            evidence.setdefault(cell, []).append(name)
                else:
                    for cap, op in found:
                        cell = (orm, cap, op)
                        if cell in universe:
                            covered.add(cell)
                            evidence.setdefault(cell, []).append(name)
    else:
        status = "static_inferred"
        for path in glob.glob("orm-tests/*/*.test.ts"):
            orm = orm_from_path(path)
            if not orm:
                continue
            p = Path(path)
            text = f"{p.name}\n{read_text(p)}"
            found = detect_operations(text)
            if not found:
                for cap in detect_capability(text):
                    for op in cap_ops.get(cap, []):
                        cell = (orm, cap, op)
                        covered.add(cell)
                        evidence.setdefault(cell, []).append(path)
            else:
                for cap, op in found:
                    cell = (orm, cap, op)
                    if cell in universe:
                        covered.add(cell)
                        evidence.setdefault(cell, []).append(path)

    # Add e2e evidence for non-vitest suites.
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
            text = f"{p.name}\n{read_text(p)}"
            found = detect_operations(text)
            if not found:
                for cap in detect_capability(text):
                    for op in cap_ops.get(cap, []):
                        cell = (orm, cap, op)
                        covered.add(cell)
                        evidence.setdefault(cell, []).append(path)
            else:
                for cap, op in found:
                    cell = (orm, cap, op)
                    if cell in universe:
                        covered.add(cell)
                        evidence.setdefault(cell, []).append(path)

    uncovered = sorted(list(universe - covered))
    ratio = (len(covered) / len(universe)) if universe else 0.0

    # Rollups
    per_orm = []
    for orm in orms:
        total = sum(len(ops) for ops in cap_ops.values())
        cov = sum((orm, cap, op) in covered for cap, ops in cap_ops.items() for op in ops)
        per_orm.append({"orm": orm, "covered": cov, "total": total, "ratio": round((cov / total) if total else 0.0, 4)})

    per_orm_cap = []
    for orm in orms:
        for cap, ops in cap_ops.items():
            total = len(ops)
            cov = sum((orm, cap, op) in covered for op in ops)
            per_orm_cap.append({
                "orm": orm,
                "capability": cap,
                "covered": cov,
                "total": total,
                "ratio": round((cov / total) if total else 0.0, 4),
            })

    out = {
        "status": status,
        "orms": orms,
        "capability_operations": cap_ops,
        "covered_cells": [{"orm": o, "capability": c, "operation": op} for (o, c, op) in sorted(covered)],
        "uncovered_cells": [{"orm": o, "capability": c, "operation": op} for (o, c, op) in uncovered],
        "covered": len(covered),
        "total": len(universe),
        "ratio": round(ratio, 4),
        "per_orm": per_orm,
        "per_orm_capability": per_orm_cap,
        "evidence": [
            {"orm": o, "capability": c, "operation": op, "sources": sorted(set(srcs))}
            for (o, c, op), srcs in sorted(evidence.items())
        ],
    }

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(out, ensure_ascii=False, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
