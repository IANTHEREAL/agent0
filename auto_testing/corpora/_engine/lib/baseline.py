"""
Baseline ratchet: freeze the expected per-statement verdict, then gate future
runs against it.

baseline.json schema:
{
  "meta": { corpus, corpus_tag, db9_version, oracle, generated_utc },
  "statements": {
     "<file>#<idx>": { "expect": "pass" }                                  # db9 must keep passing
     "<file>#<idx>": { "expect": "gap", "fid": "...", "issue": 2647,        # known gap, allowed to fail
                       "sqlstate": "XX000" }
  }
}

Only statements PostgreSQL accepts are gated. A statement PG also rejects is not
in the baseline (we don't gate db9 on invalid SQL).
"""
import json


def build(ledger, issue_links, meta):
    """ledger: {key: {sql, pg_ok, db9_ok, sqlstate, msg, fid}} from a full diff."""
    stmts = {}
    for key, r in ledger.items():
        if not r.get("pg_ok"):
            continue                      # only gate statements PG accepts
        if r.get("db9_ok"):
            stmts[key] = {"expect": "pass"}
        else:
            fid = r.get("fid") or "unclassified"
            stmts[key] = {"expect": "gap", "fid": fid,
                          "issue": issue_links.get(fid), "sqlstate": r.get("sqlstate")}
    return {"meta": meta, "statements": stmts}


def compare(baseline, scan):
    """scan: {key: {db9_ok, sqlstate, msg}} from a db9-only run.
    Scope-limited: only statements actually scanned are evaluated, so a --smoke
    run never flags the un-run statements. Returns regressions / improvements."""
    bs = baseline["statements"]
    reg, imp = [], []
    for key, s in scan.items():
        b = bs.get(key)
        if b is None:
            continue                      # not in baseline (out of scope / new) — ignore
        if b["expect"] == "pass" and not s["db9_ok"]:
            reg.append({"key": key, "sqlstate": s.get("sqlstate"), "msg": s.get("msg")})
        elif b["expect"] == "gap" and s["db9_ok"]:
            imp.append({"key": key, "fid": b.get("fid"), "issue": b.get("issue")})
    return {"regressions": reg, "improvements": imp, "missing": []}


def accept(baseline, scan):
    """Promote every now-passing known gap to expect=pass. Returns (#promoted)."""
    n = 0
    for key, b in baseline["statements"].items():
        s = scan.get(key)
        if b["expect"] == "gap" and s and s["db9_ok"]:
            baseline["statements"][key] = {"expect": "pass"}
            n += 1
    return n


def load(path):
    return json.load(open(path))


def save(baseline, path):
    """One statement per line: valid JSON, compact, and gives clean diffs when a
    fix flips gap->pass (so `--accept` changes are reviewable)."""
    items = sorted(baseline["statements"].items())
    with open(path, "w") as f:
        f.write("{\n")
        f.write(' "meta": ' + json.dumps(baseline["meta"], sort_keys=True) + ",\n")
        f.write(' "statements": {\n')
        for i, (k, v) in enumerate(items):
            tail = "," if i < len(items) - 1 else ""
            f.write("  " + json.dumps(k) + ": " + json.dumps(v, sort_keys=True) + tail + "\n")
        f.write(" }\n}\n")
