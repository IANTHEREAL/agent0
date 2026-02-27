#!/usr/bin/env python3
import argparse
import glob
import json
import re
from pathlib import Path

STATEMENTS = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE_TABLE",
    "ALTER_TABLE",
    "DROP_TABLE",
    "CREATE_INDEX",
    "COPY",
    "BEGIN_COMMIT_ROLLBACK",
]
PHASES = ["simple_query", "parse_bind_execute", "describe", "sync", "error_path"]

SQL_TO_STATEMENT = [
    (re.compile(r"\bSELECT\b", re.I), "SELECT"),
    (re.compile(r"\bINSERT\b", re.I), "INSERT"),
    (re.compile(r"\bUPDATE\b", re.I), "UPDATE"),
    (re.compile(r"\bDELETE\b", re.I), "DELETE"),
    (re.compile(r"\bCREATE\s+TABLE\b", re.I), "CREATE_TABLE"),
    (re.compile(r"\bALTER\s+TABLE\b", re.I), "ALTER_TABLE"),
    (re.compile(r"\bDROP\s+TABLE\b", re.I), "DROP_TABLE"),
    (re.compile(r"\bCREATE\s+INDEX\b", re.I), "CREATE_INDEX"),
    (re.compile(r"\bCOPY\b", re.I), "COPY"),
    (re.compile(r"\b(BEGIN|COMMIT|ROLLBACK)\b", re.I), "BEGIN_COMMIT_ROLLBACK"),
]


def detect_statements(sql: str):
    out = set()
    for pat, stmt in SQL_TO_STATEMENT:
        if pat.search(sql):
            out.add(stmt)
    return out


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="ignore")
    except Exception:
        return ""


def collect_simple_query_cells(covered, evidence):
    for path in glob.glob("tests/*.sql"):
        p = Path(path)
        sql = read_text(p)
        stmts = detect_statements(sql)
        for s in stmts:
            covered.add((s, "simple_query"))
            evidence.setdefault((s, "simple_query"), []).append(str(p))


def collect_extended_protocol_cells(covered, evidence):
    p = Path("scripts/extended_protocol_smoke.py")
    txt = read_text(p)
    sql_literals = re.findall(r'"([^"]+)"', txt, re.I)
    for q in sql_literals:
        for s in detect_statements(q):
            for phase in ("parse_bind_execute", "describe", "sync"):
                covered.add((s, phase))
                evidence.setdefault((s, phase), []).append("scripts/extended_protocol_smoke.py")


def collect_error_path_cells(covered, evidence):
    for path in glob.glob("tests/*.errors"):
        stem = Path(path).stem
        sql_path = Path("tests") / f"{stem}.sql"
        txt = read_text(sql_path)
        for s in detect_statements(txt):
            covered.add((s, "error_path"))
            evidence.setdefault((s, "error_path"), []).append(str(sql_path))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="artifacts/coverage/statement_protocol_coverage.json")
    args = ap.parse_args()

    covered = set()
    evidence = {}
    collect_simple_query_cells(covered, evidence)
    collect_extended_protocol_cells(covered, evidence)
    collect_error_path_cells(covered, evidence)

    universe = {(s, p) for s in STATEMENTS for p in PHASES}
    uncovered = sorted(list(universe - covered))
    ratio = (len(covered) / len(universe)) if universe else 0.0

    out = {
        "status": "ok",
        "statements": STATEMENTS,
        "protocol_phases": PHASES,
        "covered_cells": [{"statement": s, "phase": p} for s, p in sorted(covered)],
        "uncovered_cells": [{"statement": s, "phase": p} for s, p in uncovered],
        "covered": len(covered),
        "total": len(universe),
        "ratio": round(ratio, 4),
        "evidence": [
            {"statement": s, "phase": p, "sources": sorted(set(srcs))}
            for (s, p), srcs in sorted(evidence.items())
        ],
    }

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(out, ensure_ascii=False, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
