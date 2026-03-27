#!/usr/bin/env python3
"""
Lock contention stress test for #2156.

Reproduces the scenario: concurrent poll traffic where explicit transactions
(BEGIN → SELECT → UPDATE → COMMIT) contend with plain autocommit SELECTs
on the same rows.

Measures error rates for:
  1. Pure autocommit SELECT (should be ~0% after fix)
  2. UPDATE inside explicit txn (expected >0% under SI — write conflicts)

Usage:
  python3 tests/stress_lock_contention_2156.py --dsn "postgres://admin:admin@localhost:5433/test"
"""

import argparse
import threading
import time
import psycopg2


def parse_args():
    p = argparse.ArgumentParser(description="Lock contention stress test (#2156)")
    p.add_argument("--dsn", required=True)
    p.add_argument("--rows", type=int, default=3, help="Number of contended rows")
    p.add_argument("--pollers", type=int, default=20, help="Concurrent poll threads")
    p.add_argument("--readers", type=int, default=10, help="Concurrent pure-read threads")
    p.add_argument("--duration", type=int, default=30, help="Test duration in seconds")
    return p.parse_args()


def setup(dsn, n_rows):
    conn = psycopg2.connect(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("DROP TABLE IF EXISTS lc_stress")
    cur.execute("""
        CREATE TABLE lc_stress (
            id INT PRIMARY KEY,
            last_seen TIMESTAMPTZ DEFAULT now(),
            status TEXT DEFAULT 'active',
            counter INT DEFAULT 0
        )
    """)
    for i in range(1, n_rows + 1):
        cur.execute("INSERT INTO lc_stress (id) VALUES (%s)", (i,))
    conn.close()


def cleanup(dsn):
    conn = psycopg2.connect(dsn)
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("DROP TABLE IF EXISTS lc_stress")
    conn.close()


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.select_ok = 0
        self.select_err = 0
        self.select_errors = {}
        self.update_ok = 0
        self.update_err = 0
        self.update_errors = {}

    def record_select(self, ok, err_code=None):
        with self.lock:
            if ok:
                self.select_ok += 1
            else:
                self.select_err += 1
                self.select_errors[err_code] = self.select_errors.get(err_code, 0) + 1

    def record_update(self, ok, err_code=None):
        with self.lock:
            if ok:
                self.update_ok += 1
            else:
                self.update_err += 1
                self.update_errors[err_code] = self.update_errors.get(err_code, 0) + 1


def poll_worker(dsn, n_rows, stats, stop_event):
    """Simulates poll pattern: BEGIN → SELECT → UPDATE → COMMIT"""
    import random
    conn = psycopg2.connect(dsn)
    while not stop_event.is_set():
        row_id = random.randint(1, n_rows)
        try:
            conn.autocommit = False
            cur = conn.cursor()
            cur.execute("SELECT * FROM lc_stress WHERE id = %s", (row_id,))
            cur.fetchone()
            cur.execute(
                "UPDATE lc_stress SET last_seen = now(), counter = counter + 1 WHERE id = %s",
                (row_id,),
            )
            conn.commit()
            stats.record_update(True)
        except Exception as e:
            try:
                conn.rollback()
            except Exception:
                # Connection may be broken, reconnect
                try:
                    conn.close()
                except Exception:
                    pass
                conn = psycopg2.connect(dsn)
            code = getattr(e, "pgcode", str(type(e).__name__))
            stats.record_update(False, code)
    conn.close()


def reader_worker(dsn, n_rows, stats, stop_event):
    """Pure autocommit SELECT — should never fail under normal contention."""
    import random
    conn = psycopg2.connect(dsn)
    conn.autocommit = True
    while not stop_event.is_set():
        row_id = random.randint(1, n_rows)
        try:
            cur = conn.cursor()
            cur.execute("SELECT * FROM lc_stress WHERE id = %s", (row_id,))
            cur.fetchone()
            stats.record_select(True)
        except Exception as e:
            code = getattr(e, "pgcode", str(type(e).__name__))
            stats.record_select(False, code)
            # Reconnect on error
            try:
                conn.close()
            except Exception:
                pass
            conn = psycopg2.connect(dsn)
            conn.autocommit = True
    conn.close()


def main():
    args = parse_args()
    print(f"=== Lock Contention Stress Test (#2156) ===")
    print(f"DSN: {args.dsn}")
    print(f"Rows: {args.rows}, Pollers: {args.pollers}, Readers: {args.readers}")
    print(f"Duration: {args.duration}s\n")

    setup(args.dsn, args.rows)
    stats = Stats()
    stop = threading.Event()

    threads = []
    for _ in range(args.pollers):
        t = threading.Thread(target=poll_worker, args=(args.dsn, args.rows, stats, stop))
        t.start()
        threads.append(t)
    for _ in range(args.readers):
        t = threading.Thread(target=reader_worker, args=(args.dsn, args.rows, stats, stop))
        t.start()
        threads.append(t)

    time.sleep(args.duration)
    stop.set()
    for t in threads:
        t.join(timeout=10)

    cleanup(args.dsn)

    print("=" * 60)
    total_select = stats.select_ok + stats.select_err
    total_update = stats.update_ok + stats.update_err
    select_rate = (stats.select_err / total_select * 100) if total_select > 0 else 0
    update_rate = (stats.update_err / total_update * 100) if total_update > 0 else 0

    print(f"Pure SELECT:  {stats.select_ok} OK / {stats.select_err} ERR "
          f"({select_rate:.1f}% error rate)")
    if stats.select_errors:
        for code, count in sorted(stats.select_errors.items(), key=lambda x: -x[1]):
            print(f"  {code}: {count}")

    print(f"UPDATE (txn): {stats.update_ok} OK / {stats.update_err} ERR "
          f"({update_rate:.1f}% error rate)")
    if stats.update_errors:
        for code, count in sorted(stats.update_errors.items(), key=lambda x: -x[1]):
            print(f"  {code}: {count}")

    print(f"\nTotal operations: {total_select + total_update}")

    # Gate: pure SELECT error rate should be < 1%
    if select_rate > 1.0:
        print(f"\n[FAIL] SELECT error rate {select_rate:.1f}% exceeds 1% threshold")
        exit(1)
    else:
        print(f"\n[PASS] SELECT error rate {select_rate:.1f}% is within threshold")


if __name__ == "__main__":
    main()
