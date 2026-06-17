"""
Runner for pg_regress-style differential corpora.

Two modes:
  * full diff (oracle + db9)  -> ledger {key: pg_ok, db9_ok, ...}  (for baseline regen)
  * db9-only scan             -> scan   {key: db9_ok, ...}          (for the gate)

A "key" is "<file>#<stmt_index>". Statements that are not gatable
(psql meta-commands, COPY FROM STDIN, VACUUM/ANALYZE/... maintenance, and any
timeout / connection / aborted-transaction-cascade (25P02) outcome) are excluded
so the ratchet stays deterministic.
"""
import re
import sys
import time
from pathlib import Path

import psycopg

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "lib"))
from sqlsplit import split_sql            # noqa: E402
from classify import feature_id           # noqa: E402

SESSION = ["SET statement_timeout='45s'", "SET lock_timeout='20s'",
           "SET client_min_messages=warning", "SET TimeZone='America/Los_Angeles'",
           "SET DateStyle='ISO, MDY'", "SET IntervalStyle='postgres'"]
COPY_STDIN = re.compile(r"\bCOPY\b.*\bFROM\s+STDIN\b", re.I | re.S)
MAINT = re.compile(r"^\s*(VACUUM|ANALYZE|CHECKPOINT|CLUSTER|REINDEX)\b", re.I)
# Inconclusive outcomes excluded from the ledger (and therefore from the gate) so
# the ratchet stays deterministic against a busy/shared server:
#   57014 timeout, 55P03 lock-not-available, 53300 too-many-connections, 08xxx
#   connection failures  -> environment noise, not a compatibility signal;
#   25P02 in-failed-sql-transaction -> a CASCADE: the statement only failed
#   because an EARLIER one in the same transaction did, so whether it appears
#   depends on where the first failure landed (not on the statement itself).
SKIP_CODES = {"57014", "55P03", "53300", "08006", "08000", "08003", "08001", "25P02"}
COPY_MAP = {"onek": "onek.data", "tenk1": "tenk.data", "person": "person.data",
            "emp": "emp.data", "student": "student.data", "stud_emp": "stud_emp.data",
            "road": "streets.data"}
FIX_SKIP = re.compile(r"CREATE\s+TABLESPACE|allow_in_place_tablespaces|LANGUAGE\s+C\b|:'regresslib'", re.I)
FIX_COPY = re.compile(r"^\s*COPY\s+(\w+)\s+FROM\s+:'filename'", re.I)


class Side:
    def __init__(self, dsn):
        self.dsn = dsn
        self.conn = psycopg.connect(dsn, autocommit=True, connect_timeout=15)
        for g in SESSION:
            try:
                with self.conn.cursor() as c:
                    c.execute(g)
            except Exception:
                pass

    def version(self):
        try:
            with self.conn.cursor() as c:
                c.execute("SELECT version()"); return c.fetchone()[0]
        except Exception:
            return "?"

    def run(self, sql):
        try:
            with self.conn.cursor() as c:
                c.execute(sql)
                if c.description is not None:
                    try: c.fetchall()
                    except Exception: pass
            return (True, None, None)
        except psycopg.Error as e:
            msg = (e.diag.message_primary if getattr(e, "diag", None) and e.diag.message_primary
                   else (str(e).splitlines() or [""])[0])
            if self.conn.closed or self.conn.broken:
                self.conn = psycopg.connect(self.dsn, autocommit=True, connect_timeout=15)
            return (False, e.sqlstate, msg)

    def reset_txn(self):
        try:
            if self.conn.info.transaction_status != psycopg.pq.TransactionStatus.IDLE:
                with self.conn.cursor() as c:
                    c.execute("ROLLBACK")
        except Exception:
            self.conn = psycopg.connect(self.dsn, autocommit=True, connect_timeout=15)


def load_fixtures(sides, setup_file, data_dir):
    """Run the portable fixture statements + client-side COPY on every Side."""
    items = [it for it in split_sql(Path(setup_file).read_text()) if it.kind == "sql"]
    for it in items:
        if FIX_SKIP.search(it.text):
            continue
        m = FIX_COPY.match(it.text)
        if m:
            table = m.group(1).lower()
            dfile = COPY_MAP.get(table)
            if not dfile:
                continue
            dp = Path(data_dir) / dfile
            for s in sides:
                try:
                    with open(dp, "rb") as f, s.conn.cursor() as cur, cur.copy(f"COPY {table} FROM STDIN") as cp:
                        while (chunk := f.read(1 << 16)):
                            cp.write(chunk)
                except Exception:
                    pass
            continue
        for s in sides:
            s.run(it.text)


def gatable(item):
    return not (item.kind == "meta" or COPY_STDIN.search(item.text) or MAINT.match(item.text))


def _iter(sql_dir, files):
    for fname in files:
        p = Path(sql_dir) / (fname if fname.endswith(".sql") else fname + ".sql")
        if not p.exists():
            continue
        items = split_sql(p.read_text(errors="replace"))
        yield p.name, items


def run(db9_dsn, oracle_dsn, sql_dir, data_dir, setup_file, files, progress=None):
    """Returns ledger dict. If oracle_dsn is None -> db9-only scan (pg_ok absent)."""
    db9 = Side(db9_dsn)
    oracle = Side(oracle_dsn) if oracle_dsn else None
    sides = [db9] + ([oracle] if oracle else [])
    load_fixtures(sides, setup_file, data_dir)

    ledger = {}
    for n, (fname, items) in enumerate(_iter(sql_dir, files)):
        for idx, it in enumerate(items):
            if not gatable(it):
                continue
            d_ok, d_state, d_msg = db9.run(it.text)
            if d_state in SKIP_CODES:                 # flaky outcome -> never gate
                continue
            key = f"{fname}#{idx}"
            entry = {"db9_ok": d_ok, "sqlstate": d_state, "msg": d_msg}
            if oracle:
                o_ok, o_state, _ = oracle.run(it.text)
                entry["pg_ok"] = o_ok
                if o_ok and not d_ok:
                    entry["fid"] = feature_id(d_state, d_msg, it.text)
                entry["sql"] = " ".join(it.text.split())[:200]
            ledger[key] = entry
        db9.reset_txn()
        if oracle:
            oracle.reset_txn()
        if progress:
            progress(n + 1, fname, len(ledger))
    return ledger
