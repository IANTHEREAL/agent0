#!/usr/bin/env python3
"""
Corpus-agnostic differential test entry point.

    run_corpus.py --corpus <name> --dsn <db9-dsn> [mode] [scope]

Modes:
    --check-baseline   (default) run db9 vs the frozen baseline; exit 1 on regression
    --regen-baseline   rebuild baseline.json from a full PG-vs-db9 diff (needs oracle)
    --accept           promote now-passing known gaps to expect=pass; rewrite baseline

Scope:
    --smoke            run the curated fast subset (manifest: schedule_smoke)
    --full             run the whole schedule (default)

Each corpus lives in auto_testing/corpora/<name>/ and is described by manifest.yaml.
The engine reads the manifest and dispatches to runners/<type>.py — adding a new
corpus is a new folder + manifest, no engine change.
"""
import argparse
import json
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CORPORA = HERE.parent
sys.path.insert(0, str(HERE / "runners"))
sys.path.insert(0, str(HERE / "lib"))
import baseline as bl                      # noqa: E402


def read_manifest(path):
    """Tiny flat 'key: value' YAML reader (no pyyaml dependency)."""
    m = {}
    for ln in Path(path).read_text().splitlines():
        ln = ln.split("#")[0].rstrip()
        if not ln or ln.startswith(" ") or ":" not in ln:
            continue
        k, v = ln.split(":", 1)
        m[k.strip()] = v.strip().strip('"').strip("'")
    return m


def read_list(path):
    out = []
    for ln in Path(path).read_text().splitlines():
        ln = ln.split("#")[0].strip()
        if ln:
            out.append(ln)
    return out


def with_db(dsn, dbname):
    """Return dsn with its database overridden to dbname."""
    import psycopg
    d = psycopg.conninfo.conninfo_to_dict(dsn)
    d["dbname"] = dbname
    return psycopg.conninfo.make_conninfo(**d)


def reset_db(dsn):
    """Drop + recreate the dsn's database (via the server's 'postgres' db) so the
    run starts clean. Without this, a re-run hits 'already exists' on every CREATE
    and cascades into false regressions."""
    import psycopg
    d = psycopg.conninfo.conninfo_to_dict(dsn)
    target = d.get("dbname", "")
    if target in ("", "postgres", "template0", "template1"):
        sys.exit(f"--reset-db refuses to drop '{target}'; pass a dedicated --db-name")
    admin = dict(d); admin["dbname"] = "postgres"
    c = psycopg.connect(psycopg.conninfo.make_conninfo(**admin), autocommit=True, connect_timeout=15)
    try:
        with c.cursor() as cur:
            try:    # best-effort: kick lingering connections (ignored if unsupported)
                cur.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity "
                            "WHERE datname=%s AND pid<>pg_backend_pid()", (target,))
            except Exception:
                pass
            cur.execute(f'DROP DATABASE IF EXISTS "{target}"')
            cur.execute(f'CREATE DATABASE "{target}"')
    finally:
        c.close()
    print(f"  reset db: {target} (clean)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--dsn", default="postgres://admin:admin@127.0.0.1:5433/postgres",
                    help="db9 DSN under test")
    ap.add_argument("--oracle-dsn", default="host=127.0.0.1 port=55432 dbname=regression user=postgres password=postgres")
    ap.add_argument("--check-baseline", action="store_true")
    ap.add_argument("--regen-baseline", action="store_true")
    ap.add_argument("--accept", action="store_true")
    ap.add_argument("--smoke", action="store_true")
    ap.add_argument("--full", action="store_true")
    ap.add_argument("--reset-db", action="store_true",
                    help="drop+recreate the target db before running (REQUIRED for a "
                         "clean, deterministic gate — a dirty db causes 'already exists' cascades)")
    ap.add_argument("--db-name", default=None,
                    help="override the database in --dsn (use a dedicated gate db, not 'postgres')")
    args = ap.parse_args()

    cdir = CORPORA / args.corpus
    man = read_manifest(cdir / "manifest.yaml")
    rtype = man.get("type")
    sql_dir = cdir / man["source_sql"]
    src_glob = "*.test" if rtype == "sqllogictest" else "*.sql"
    # corpus source is fetched on demand (not vendored) — materialise if missing
    if not sql_dir.exists() or not any(sql_dir.glob(src_glob)):
        fetch = cdir / "source" / "fetch.sh"
        if fetch.exists():
            import subprocess
            print(f"== corpus not present — fetching ({man.get('corpus_tag')}) ==")
            subprocess.run(["bash", str(fetch)], check=True)
    sched = cdir / (man["schedule_smoke"] if args.smoke else man["schedule_full"])
    files = read_list(sched)
    baseline_path = cdir / man["baseline"]
    scope = "smoke" if args.smoke else "full"

    db9_dsn = with_db(args.dsn, args.db_name) if args.db_name else args.dsn
    oracle_dsn = args.oracle_dsn
    if args.reset_db:
        reset_db(db9_dsn)
        if args.regen_baseline:
            reset_db(oracle_dsn)

    def prog(n, fname, total):
        print(f"  [{n}/{len(files)}] {fname:<24} ledger={total}", flush=True)

    # --- per-type runner adapter (run + build/compare/accept + scan extraction) ---
    if rtype == "regress_diff":
        import regress_diff as R
        data_dir, setup = cdir / man["source_data"], cdir / man["setup"]
        do_run = lambda od: R.run(db9_dsn, od, sql_dir, data_dir, setup, files, prog)
        build, compare, accept, scan_of = bl.build, bl.compare, bl.accept, \
            (lambda L: {k: {"db9_ok": v["db9_ok"], "sqlstate": v.get("sqlstate"), "msg": v.get("msg")}
                        for k, v in L.items()})
    elif rtype == "sqllogictest":
        import sqllogictest as R
        do_run = lambda od: R.run(db9_dsn, od, sql_dir, files, prog)
        build, compare, accept, scan_of = R.build_baseline, R.compare, R.accept, R.scan_from_ledger
    else:
        sys.exit(f"unknown corpus type: {rtype}")

    links = json.load(open(cdir / man["issue_links"])) if (cdir / man.get("issue_links", "x")).exists() else {}

    if args.regen_baseline:
        print(f"== regen baseline ({args.corpus}, {scope}, {len(files)} files) — full PG-vs-db9 diff ==")
        ledger = do_run(oracle_dsn)
        meta = {"corpus": args.corpus, "corpus_tag": man.get("corpus_tag", ""),
                "oracle": man.get("oracle", ""), "scope": scope, "type": rtype,
                "db9_version": R.Side(db9_dsn).version(),
                "generated_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        base = build(ledger, links, meta)
        bl.save(base, baseline_path)
        import collections
        c = collections.Counter(v["expect"] for v in base["statements"].values())
        print(f"\nbaseline -> {baseline_path}\n  gated: {len(base['statements'])}  "
              + "  ".join(f"{k}={n}" for k, n in c.most_common()))
        return

    # check / accept both run a db9-only scan
    if not baseline_path.exists():
        sys.exit(f"no baseline at {baseline_path} — run --regen-baseline first")
    print(f"== {'accept' if args.accept else 'check'} ({args.corpus}, {scope}, {len(files)} files) vs baseline ==")
    ledger = do_run(None)
    scan = scan_of(ledger)
    base = bl.load(baseline_path)
    cmp = compare(base, scan)

    if args.accept:
        n = accept(base, scan)
        base["meta"]["accepted_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        bl.save(base, baseline_path)
        print(f"promoted {n} now-passing statement(s) -> {baseline_path}")
        return

    reg, imp, miss = cmp["regressions"], cmp["improvements"], cmp["missing"]
    print(f"\n  scanned: {len(scan)}   regressions: {len(reg)}   improvements: {len(imp)}   missing(skipped): {len(miss)}")
    if imp:
        byi = {}
        for i in imp:
            byi.setdefault(i.get("issue"), 0); byi[i["issue"]] += 1
        print("\n  IMPROVEMENTS (known gaps now passing — run --accept to lock in):")
        for issue, n in sorted(byi.items(), key=lambda kv: -kv[1]):
            tag = f"#{issue}" if issue else "(unlinked)"
            print(f"    {n:>5} statements  {tag}")
    if reg:
        print("\n  ❌ REGRESSIONS (were passing, now fail):")
        for r in reg[:40]:
            print(f"    {r['key']:<34} {r['sqlstate']}: {r['msg']}")
        if len(reg) > 40:
            print(f"    … and {len(reg)-40} more")
        print(f"\nFAIL: {len(reg)} regression(s).")
        sys.exit(1)
    print("\nPASS: no regressions.")


if __name__ == "__main__":
    main()
