#!/usr/bin/env python3
"""Parse pytest-json-report output from the asyncpg suite -> regenerate asyncpg-bank.md.

Runs **asyncpg's own test suite** (Python's async PG driver) against db9 — the
connect-path surface (extended protocol, BINARY type/OID codecs, COPY, prepared
statements, introspection) from a third independent implementation (after psycopg
and pgx). asyncpg is binary-protocol-heavy, so it stresses binary codecs harder
than psycopg.

NOTE: asyncpg's graceful Connection.close() hangs against db9 (issue #2721 — db9
doesn't close the socket on Terminate), so the lane's conftest.py monkeypatches
close()->terminate() to let teardowns complete. That is a documented db9 gap, not
a masking of query semantics.

Usage: classify_failures.py <json-report-dir> <output_md>
  <json-report-dir> holds one <file>.json per asyncpg test file (pytest-json-report).
"""
import glob
import json
import os
import re
import sys
from collections import Counter, defaultdict

# (substring in error, (root-cause label, fault layer, issue)) — first match wins.
KNOWN = [
    # --- catalog type-introspection (#2722): breaks composite/domain/custom codecs ---
    ("rngmultitypid", ("pg_catalog type-introspection incomplete (pg_range.rngmultitypid missing)", "catalog", "#2722")),
    ("cannot use custom codec", ("pg_catalog type-introspection incomplete (composite/domain/custom codec setup fails)", "catalog", "#2722")),
    # --- cursor control (#2723) ---
    ("close { cursor", ("CLOSE/cursor-control unsupported (CLOSE ALL)", "parser/cursor", "#2723")),
    ("declare {", ("CLOSE/cursor-control unsupported (DECLARE [BINARY] CURSOR)", "parser/cursor", "#2723")),
    ("found: move", ("CLOSE/cursor-control unsupported (MOVE)", "parser/cursor", "#2723")),
    # --- COPY (#2724) ---
    ("unsupported copy to stdout", ("COPY (query) TO STDOUT unsupported", "wire/copy", "#2724")),
    ("copy format binary", ("COPY FORMAT binary unsupported", "wire/copy", "#2724")),
    # --- CREATE DOMAIN (#2725) ---
    ("create domain not supported", ("CREATE DOMAIN unsupported", "ddl/types", "#2725")),
    # --- already-filed cross-driver gaps ---
    ("could not determine data type of parameter",
     ("Extended-protocol parameter type inference ($N indeterminate)", "types/protocol", "#2713")),
    ("statement timeout", ("statement_timeout firing (db9 default 60s)", "session/timeout", "#2714")),
    ("transaction_read_only", ("transaction_read_only GUC unrecognized", "session/guc", "#2715")),
    ("generate_series is a set-returning", ("generate_series() unsupported in this context", "functions/srf", "#2716")),
    ("int4range", ("range-type constructors unsupported", "functions/types", "#2717")),
    ("int8range", ("range-type constructors unsupported", "functions/types", "#2717")),
    ("found: range", ("range-type DDL not parsed", "parser/types", "#2717")),
    ('type "inet', ("inet/inet[] type missing", "types", "#2703")),
    # --- minor / sibling classes (noted, not own issue) ---
    ('configuration parameter "jit"', ("unrecognized GUC (jit) — missing-GUC class", "session/guc", "#2715")),
    ('type "tid"', ("tid type missing — missing-type class", "types", "#2703")),
    # --- test-isolation artifact (NOT a db9 bug) ---
    ("already exists", ("test-isolation artifact (asyncpg reuses fixed names; close->terminate workaround skips cleanup)", "test-harness", "—")),
    # --- #2721 artifacts (close/Terminate hang) ---
    ("connection was closed in the middle", ("connection-close artifact (downstream of #2721 close hang)", "wire/protocol", "#2721")),
    ("timeout >", ("op HANGS (timeout) — likely close/Terminate (#2721) or op-level hang", "wire/protocol", "#2721")),
    # --- cascades ---
    ("current transaction is aborted", ("cascade (downstream of a prior error)", "cascade", "—")),
    ("does not exist", ("relation/object missing (often cascade of failed setup)", "cascade", "—")),
    ("not supported", ("other feature not supported (see verbatim case)", "executor/feature", "—")),
]


def classify(msg):
    m = msg.lower()
    for sub, info in KNOWN:
        if sub in m:
            return info
    if "assertionerror" in m or re.search(r"assert|!=|== ", msg):
        return ("result/behavior mismatch (assertion)", "behavior", "—")
    return ("untriaged", "—", "—")


def real_err(longrepr):
    """Pull the meaningful error line out of a pytest longrepr string."""
    if not longrepr:
        return "(no longrepr)"
    lines = [l.rstrip() for l in str(longrepr).splitlines() if l.strip()]
    # asyncpg exceptions are the most precise signal
    for l in reversed(lines):
        if re.search(r"asyncpg\.exceptions\.\w+:", l):
            return l.strip()
    for l in reversed(lines):
        if re.search(r"^E\s+\w*(Error|Exception):", l) or re.search(r"ERROR:|SQLSTATE", l):
            return l.strip().lstrip("E ").strip()
    for l in reversed(lines):
        if l.startswith("E ") and len(l) > 3:
            return l[2:].strip()
    return lines[-1].strip() if lines else "(empty)"


def load(json_dir):
    npass = nfail = nerr = nskip = 0
    fails = []  # (file, test, err)
    perfile = {}
    for jf in sorted(glob.glob(os.path.join(json_dir, "*.json"))):
        try:
            d = json.load(open(jf))
        except Exception:
            continue
        fname = os.path.basename(jf)[:-5]
        s = d.get("summary", {})
        perfile[fname] = (s.get("passed", 0), s.get("failed", 0) + s.get("error", 0))
        npass += s.get("passed", 0)
        nskip += s.get("skipped", 0)
        for t in d.get("tests", []):
            out = t.get("outcome")
            if out in ("failed", "error"):
                node = t.get("nodeid", "").split("::", 1)[-1]
                lr = ""
                for ph in ("call", "setup", "teardown"):
                    p = t.get(ph) or {}
                    if p.get("outcome") in ("failed", "error") and p.get("longrepr"):
                        lr = p["longrepr"]
                        break
                fails.append((fname, node, real_err(lr)))
                if out == "error":
                    nerr += 1
                else:
                    nfail += 1
    return npass, nfail, nerr, nskip, fails, perfile


def main():
    json_dir, out_md = sys.argv[1], sys.argv[2]
    npass, nfail, nerr, nskip, fails, perfile = load(json_dir)
    total = npass + nfail + nerr + nskip

    groups = defaultdict(list)
    for f, t, err in fails:
        groups[classify(err)].append((f, t, err))

    o = []
    W = o.append
    W("# asyncpg Driver Bank — db9 PostgreSQL-Compatibility Validation\n")
    W("> Auto-generated by `classify_failures.py` from pytest-json-report. Runs **asyncpg's")
    W("> own test suite** (Python async PG driver) against db9 — connect-path surface")
    W("> (extended protocol, **binary** type/OID codecs, COPY, prepared stmts, introspection)")
    W("> from a third independent impl after psycopg + pgx. asyncpg is binary-heavy.\n")
    W("> **Caveat:** asyncpg's graceful `Connection.close()` HANGS on db9 (**#2721** — db9 doesn't")
    W("> close the socket on `Terminate`). The lane's `conftest.py` makes close() abrupt so")
    W("> teardowns complete and the suite produces numbers. #2721 is itself a logged gap.\n")
    W(f"**Tests:** {total} · **Pass:** {npass} ({round(100*npass/total) if total else 0}%) · "
      f"**Fail:** {nfail} · **Error:** {nerr} · **Skip:** {nskip}\n")
    W("## Pass/Fail per file\n")
    W("| Test file | Pass | Fail+Err |")
    W("|---|---|---|")
    for f in sorted(perfile):
        W(f"| {f} | {perfile[f][0]} | {perfile[f][1]} |")
    W("")
    W("## Root causes (by blast radius)\n")
    W("| Root cause | Fault layer | Issue | Cases |")
    W("|---|---|---|---|")
    for (name, layer, issue), items in sorted(groups.items(), key=lambda x: -len(x[1])):
        W(f"| {name} | {layer} | {issue} | {len(items)} |")
    W("")
    W("## Every failing case (verbatim)\n")
    W("| Test file | Test | db9 error (verbatim) |")
    W("|---|---|---|")
    for f, t, err in sorted(fails):
        e = err.replace("|", chr(92) + "|")[:200]
        W(f"| {f} | `{t}` | {e} |")
    W("")
    W("## Reproduce\n```bash")
    W("bash auto_testing/corpora/drivers/asyncpg/run.sh   # full (pytest json vs db9 on :5455)")
    W("```")
    open(out_md, "w").write("\n".join(o))
    print(f"wrote {out_md}: {total} tests, {npass} pass, {nfail+nerr} fail/err, "
          f"{len(groups)} root-cause groups")
    # echo the root-cause table to stdout for quick triage
    print("\nROOT CAUSES:")
    for (name, layer, issue), items in sorted(groups.items(), key=lambda x: -len(x[1])):
        print(f"  {len(items):3d}  [{issue}] {name}  ({layer})")


if __name__ == "__main__":
    main()
