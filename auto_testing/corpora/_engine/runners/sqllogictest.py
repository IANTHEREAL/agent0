"""
Runner for sqllogictest-style corpora, with OUTPUT-equivalence checking.

PostgreSQL 17.10 is the oracle: every record is run on PG and db9; for `query`
records we compare the *result* (canonical sqllogictest hash), not just success.

Verdicts per gated record (PG must accept it to be gated):
  statement:  pass (db9 ok)            | gap (db9 errors)
  query:      match (db9 output == PG) | diff (db9 ok, output != PG) | gap (db9 errors)

The baseline stores PG's output hash per query, so the gate (check) runs db9-only
and compares db9's hash to the stored PG hash — no oracle needed at gate time.
Inconclusive outcomes (timeout / conn / 25P02 cascade) are excluded, as in the
regress runner, to keep the ratchet deterministic.
"""
import sys
from pathlib import Path

import psycopg

sys.path.insert(0, str(Path(__file__).resolve().parent))         # runners/
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from regress_diff import Side, SKIP_CODES                         # noqa: E402  (reuse conn mgmt)
from sltparse import parse                                        # noqa: E402
from outcmp import result_hash                                    # noqa: E402
from classify import feature_id                                   # noqa: E402


def _exec(side, sql, want_rows):
    """(ok, sqlstate, msg, rows). rows only when want_rows and it succeeded."""
    try:
        with side.conn.cursor() as cur:
            cur.execute(sql)
            rows = cur.fetchall() if (want_rows and cur.description is not None) else []
        return (True, None, None, rows)
    except psycopg.Error as e:
        msg = (e.diag.message_primary if getattr(e, "diag", None) and e.diag.message_primary
               else (str(e).splitlines() or [""])[0])
        if side.conn.closed or side.conn.broken:
            side.conn = psycopg.connect(side.dsn, autocommit=True, connect_timeout=15)
        return (False, e.sqlstate, msg, None)


def _clean(side):
    """Drop all public tables/views so each .test file starts isolated. Each
    sqllogictest file assumes a fresh database (it CREATEs its own tables); running
    several in one db without this makes a later file's CREATE collide with the
    previous file's objects and the db9/PG state diverges (false output diffs).
    db9 forbids DROP SCHEMA public, so we drop objects individually."""
    try:
        with side.conn.cursor() as c:
            c.execute("SELECT tablename FROM pg_tables WHERE schemaname='public'")
            for (t,) in c.fetchall():
                c.execute(f'DROP TABLE IF EXISTS "{t}" CASCADE')
            try:
                c.execute("SELECT viewname FROM pg_views WHERE schemaname='public'")
                for (v,) in c.fetchall():
                    c.execute(f'DROP VIEW IF EXISTS "{v}" CASCADE')
            except Exception:
                pass
    except Exception:
        side.reset_txn()


def run(db9_dsn, oracle_dsn, sql_dir, files, prog=None):
    db9 = Side(db9_dsn)
    oracle = Side(oracle_dsn) if oracle_dsn else None
    ledger = {}
    for n, fname in enumerate(files):
        p = Path(sql_dir) / (fname if fname.endswith(".test") else fname + ".test")
        if not p.exists():
            continue
        _clean(db9)                 # isolate each file (fresh-db semantics)
        if oracle:
            _clean(oracle)
        recs = parse(p.read_text(errors="replace"))
        for idx, r in enumerate(recs):
            sql = r["sql"]
            if not sql:
                continue
            is_query = r["kind"] == "query"
            d_ok, d_state, d_msg, d_rows = _exec(db9, sql, is_query)
            if d_state in SKIP_CODES:
                continue
            key = f"{p.name}#{idx}"
            e = {"kind": r["kind"], "db9_ok": d_ok, "sqlstate": d_state, "msg": d_msg}
            # Compare row SETS (rowsort), not the .test file's declared order: that
            # order was SQLite-tuned, and for queries whose ORDER BY leaves ties the
            # row order is implementation-defined — comparing it db9-vs-PG yields
            # false divergences. Set-equality tests the value-correctness question.
            # (ORDER-correctness for total-order queries is a separate future check.)
            if is_query and d_ok:
                e["db9_hash"] = result_hash(d_rows, r["types"], "rowsort")
            if oracle:
                o_ok, o_state, _, o_rows = _exec(oracle, sql, is_query)
                e["pg_ok"] = o_ok
                if is_query and o_ok:
                    e["pg_hash"] = result_hash(o_rows, r["types"], "rowsort")
                if o_ok and not d_ok:
                    e["fid"] = feature_id(d_state, d_msg, sql)
            ledger[key] = e
        db9.reset_txn()
        if oracle:
            oracle.reset_txn()
        if prog:
            prog(n + 1, p.name, len(ledger))
    return ledger


# ---- baseline build / compare / accept (output-aware) ----------------------
def build_baseline(ledger, links, meta):
    stmts = {}
    for k, e in ledger.items():
        if not e.get("pg_ok"):
            continue                          # only gate what PG accepts
        if e["kind"] == "statement":
            if e["db9_ok"]:
                stmts[k] = {"kind": "stmt", "expect": "pass"}
            else:
                fid = e.get("fid") or "unclassified"
                stmts[k] = {"kind": "stmt", "expect": "gap", "fid": fid,
                            "issue": links.get(fid), "sqlstate": e.get("sqlstate")}
        else:  # query
            ph = e.get("pg_hash", "")
            if not e["db9_ok"]:
                fid = e.get("fid") or "unclassified"
                stmts[k] = {"kind": "query", "expect": "gap", "pg_hash": ph,
                            "fid": fid, "issue": links.get(fid), "sqlstate": e.get("sqlstate")}
            elif e.get("db9_hash") == ph:
                stmts[k] = {"kind": "query", "expect": "match", "pg_hash": ph}
            else:
                stmts[k] = {"kind": "query", "expect": "diff", "pg_hash": ph}
    return {"meta": meta, "statements": stmts}


def scan_from_ledger(ledger):
    return {k: {"kind": v["kind"], "db9_ok": v["db9_ok"], "db9_hash": v.get("db9_hash"),
                "sqlstate": v.get("sqlstate"), "msg": v.get("msg")}
            for k, v in ledger.items()}


def compare(baseline, scan):
    bs = baseline["statements"]
    reg, imp = [], []
    for k, s in scan.items():
        b = bs.get(k)
        if b is None:
            continue
        if b["kind"] == "stmt":
            if b["expect"] == "pass" and not s["db9_ok"]:
                reg.append({"key": k, "sqlstate": s.get("sqlstate"), "msg": s.get("msg")})
            elif b["expect"] == "gap" and s["db9_ok"]:
                imp.append({"key": k, "issue": b.get("issue")})
        else:  # query
            matches = s["db9_ok"] and s.get("db9_hash") == b.get("pg_hash")
            if b["expect"] == "match":
                if not matches:
                    why = "errored" if not s["db9_ok"] else "output differs from PG"
                    reg.append({"key": k, "sqlstate": s.get("sqlstate"),
                                "msg": s.get("msg") or why})
            else:  # diff / gap
                if matches:
                    imp.append({"key": k, "issue": b.get("issue")})
    return {"regressions": reg, "improvements": imp, "missing": []}


def accept(baseline, scan):
    n = 0
    for k, b in baseline["statements"].items():
        s = scan.get(k)
        if not s:
            continue
        if b["kind"] == "stmt" and b["expect"] == "gap" and s["db9_ok"]:
            baseline["statements"][k] = {"kind": "stmt", "expect": "pass"}
            n += 1
        elif b["kind"] == "query" and b["expect"] in ("diff", "gap") \
                and s["db9_ok"] and s.get("db9_hash") == b.get("pg_hash"):
            baseline["statements"][k] = {"kind": "query", "expect": "match", "pg_hash": b["pg_hash"]}
            n += 1
    return n
