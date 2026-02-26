#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.8"
# dependencies = []
# ///
"""
Two-session advisory lock savepoint tests for db9-server.

Proves that ROLLBACK TO SAVEPOINT releases xact advisory locks at the
rollback boundary (not just at COMMIT) by observing lock state from a
second session.  Keys 6000–6005 to avoid collisions with single-session
tests (5000-range).

Follows the pattern of tests/34_concurrent_transactions.py.
"""

import argparse
import os
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from typing import List, Optional, Tuple
from urllib.parse import urlparse


@dataclass
class TestResult:
    name: str
    passed: bool
    message: str
    duration: float


class AdvisoryLockSavepointTests:
    def __init__(self, host: str, port: int, user: str, password: str,
                 database: str = "postgres"):
        self.host = host
        self.port = port
        self.user = user
        self.password = password
        self.database = database
        self.results: List[TestResult] = []

    def _psql_env(self) -> dict:
        env = os.environ.copy()
        env["PGPASSWORD"] = self.password
        return env

    def _psql_base_args(self) -> List[str]:
        return [
            "psql", "-h", self.host, "-p", str(self.port),
            "-U", self.user, "-d", self.database,
            "-v", "ON_ERROR_STOP=1",
        ]

    def run_sql(self, sql: str) -> str:
        """Run a single SQL statement and return the result.

        Raises RuntimeError if psql exits with non-zero status.
        """
        result = subprocess.run(
            self._psql_base_args() + ["-t", "-A", "-c", sql],
            capture_output=True, text=True, env=self._psql_env(), timeout=30
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"psql failed (exit {result.returncode}): {result.stderr.strip()}"
            )
        return result.stdout.strip()

    def run_interactive_session(
        self,
        commands_and_barriers: List[Tuple[str, Optional[threading.Event],
                                          Optional[threading.Event]]],
    ) -> str:
        """Run psql interactively with server-confirmed synchronization.

        Each entry is (sql_string, event_to_set_after_confirmed,
        event_to_wait_before_sending).

        After each SQL command, a \\echo barrier marker is sent and we
        read stdout until the server echoes it back, confirming the
        command has been fully processed.
        """
        proc = subprocess.Popen(
            self._psql_base_args() + ["-t", "-A"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, env=self._psql_env()
        )

        stdout_lines: List[str] = []
        barrier_events: dict = {}

        def stdout_reader():
            # Use readline() instead of iterator to avoid Python's internal
            # read-ahead buffering which can delay line delivery from pipes.
            while True:
                line = proc.stdout.readline()
                if not line:
                    break
                line = line.rstrip("\n")
                stdout_lines.append(line)
                if line in barrier_events:
                    barrier_events[line].set()

        reader = threading.Thread(target=stdout_reader, daemon=True)
        reader.start()

        try:
            for i, (sql, evt_set, evt_wait) in enumerate(commands_and_barriers):
                if evt_wait is not None:
                    assert evt_wait.wait(timeout=10), \
                        f"Timed out waiting for event before: {sql}"
                barrier_id = f"__barrier_{i}__"
                barrier_events[barrier_id] = threading.Event()
                proc.stdin.write(sql + "\n")
                proc.stdin.write(f"\\echo {barrier_id}\n")
                proc.stdin.flush()
                assert barrier_events[barrier_id].wait(timeout=10), \
                    f"Timed out waiting for server to process: {sql}"
                if evt_set is not None:
                    evt_set.set()
            proc.stdin.close()
        except Exception:
            proc.kill()
            raise

        proc.wait(timeout=30)
        reader.join(timeout=5)

        stderr = proc.stderr.read()
        if proc.returncode != 0:
            raise RuntimeError(
                f"psql interactive session failed "
                f"(exit {proc.returncode}): {stderr.strip()}"
            )

        return "\n".join(
            line for line in stdout_lines
            if not line.startswith("__barrier_")
        )

    def poll_try_lock(self, key: int, expected: str,
                      timeout: float = 5.0) -> str:
        """Poll pg_try_advisory_xact_lock until it returns *expected*.

        Each probe runs in its own autocommit connection so the xact lock
        is released immediately when the connection closes — probes are
        non-destructive.  Returns the last observed value.
        """
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            last = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key});")
            if last == expected:
                return last
            time.sleep(0.1)
        return last

    def run_test(self, name: str, test_func) -> TestResult:
        start = time.time()
        try:
            test_func()
            duration = time.time() - start
            result = TestResult(name, True, "PASSED", duration)
        except AssertionError as e:
            duration = time.time() - start
            result = TestResult(name, False, f"FAILED: {e}", duration)
        except Exception as e:
            duration = time.time() - start
            result = TestResult(name, False, f"ERROR: {e}", duration)

        self.results.append(result)
        status = "PASS" if result.passed else "FAIL"
        print(f"  [{status}] {name} ({result.duration:.3f}s)")
        if not result.passed:
            print(f"    {result.message}")
        return result

    # ------------------------------------------------------------------
    # 2S-1: Basic xact lock released on ROLLBACK TO SAVEPOINT
    #   Session A acquires xact lock 6000 inside a savepoint, then rolls
    #   back.  Session B observes that the lock is freed before A commits.
    # ------------------------------------------------------------------
    def test_2s1_basic_rollback_releases(self):
        key = 6000
        barrier_a_locked = threading.Event()
        barrier_a_rolled_back = threading.Event()
        barrier_b_observed_held = threading.Event()
        barrier_b_probed = threading.Event()
        b_before = [None]
        b_after = [None]

        def session_a_thread():
            self.run_interactive_session([
                ("BEGIN;", None, None),
                ("SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key});",
                 barrier_a_locked, None),
                ("ROLLBACK TO SAVEPOINT s1;",
                 barrier_a_rolled_back, barrier_b_observed_held),
                ("COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_locked.wait(timeout=10), \
                "Timed out waiting for A to lock"
            # Single probe: barrier guarantees A holds the lock already
            b_before[0] = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key});")
            barrier_b_observed_held.set()
            assert barrier_a_rolled_back.wait(timeout=10), \
                "Timed out waiting for A to rollback"
            # Poll: lock release may not be instantly visible
            b_after[0] = self.poll_try_lock(key, "t")
            barrier_b_probed.set()

        t_a = threading.Thread(target=session_a_thread)
        t_b = threading.Thread(target=session_b_thread)
        t_a.start()
        t_b.start()
        t_a.join(timeout=20)
        t_b.join(timeout=20)

        assert b_before[0] == "f", \
            f"2S-1: B should NOT acquire lock while A holds it, got: {b_before[0]}"
        assert b_after[0] == "t", \
            f"2S-1: B should acquire lock after A's ROLLBACK TO SAVEPOINT, got: {b_after[0]}"

    # ------------------------------------------------------------------
    # 2S-2: Lock acquired BEFORE savepoint NOT released by rollback
    #   Session A acquires xact lock 6001 before the savepoint, acquires
    #   6002 after.  ROLLBACK TO releases 6002 but not 6001.
    # ------------------------------------------------------------------
    def test_2s2_lock_before_savepoint_survives(self):
        key_before = 6001
        key_after = 6002
        barrier_a_locked = threading.Event()
        barrier_a_rolled_back = threading.Event()
        barrier_b_checked = threading.Event()
        barrier_b_probed = threading.Event()
        b_key_before_after_rollback = [None]
        b_key_after_after_rollback = [None]

        def session_a_thread():
            self.run_interactive_session([
                ("BEGIN;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_before});", None, None),
                ("SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_after});",
                 barrier_a_locked, None),
                ("ROLLBACK TO SAVEPOINT s1;",
                 barrier_a_rolled_back, barrier_b_checked),
                ("COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_locked.wait(timeout=10)
            barrier_b_checked.set()
            assert barrier_a_rolled_back.wait(timeout=10)
            # key_before: single probe — barrier guarantees A still holds it
            b_key_before_after_rollback[0] = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key_before});")
            # key_after: poll — release may not be instantly visible
            b_key_after_after_rollback[0] = self.poll_try_lock(
                key_after, "t")
            barrier_b_probed.set()

        t_a = threading.Thread(target=session_a_thread)
        t_b = threading.Thread(target=session_b_thread)
        t_a.start()
        t_b.start()
        t_a.join(timeout=20)
        t_b.join(timeout=20)

        assert b_key_before_after_rollback[0] == "f", \
            f"2S-2: Lock before savepoint should survive rollback, B got: {b_key_before_after_rollback[0]}"
        assert b_key_after_after_rollback[0] == "t", \
            f"2S-2: Lock after savepoint should be released by rollback, B got: {b_key_after_after_rollback[0]}"

    # ------------------------------------------------------------------
    # 2S-3: Nested savepoints — rollback to outer releases both frames
    #   Session A acquires locks in s1 (6003) and s2 (6004), then rolls
    #   back to s1.  Session B observes both are released.
    # ------------------------------------------------------------------
    def test_2s3_nested_savepoints(self):
        key_s1 = 6003
        key_s2 = 6004
        barrier_a_locked = threading.Event()
        barrier_a_rolled_back = threading.Event()
        barrier_b_checked = threading.Event()
        barrier_b_probed = threading.Event()
        b_s1_after = [None]
        b_s2_after = [None]

        def session_a_thread():
            self.run_interactive_session([
                ("BEGIN;", None, None),
                ("SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_s1});", None, None),
                ("SAVEPOINT s2;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_s2});",
                 barrier_a_locked, None),
                ("ROLLBACK TO SAVEPOINT s1;",
                 barrier_a_rolled_back, barrier_b_checked),
                ("COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_locked.wait(timeout=10)
            barrier_b_checked.set()
            assert barrier_a_rolled_back.wait(timeout=10)
            # Poll: lock releases may not be instantly visible
            b_s1_after[0] = self.poll_try_lock(key_s1, "t")
            b_s2_after[0] = self.poll_try_lock(key_s2, "t")
            barrier_b_probed.set()

        t_a = threading.Thread(target=session_a_thread)
        t_b = threading.Thread(target=session_b_thread)
        t_a.start()
        t_b.start()
        t_a.join(timeout=20)
        t_b.join(timeout=20)

        assert b_s1_after[0] == "t", \
            f"2S-3: Lock in s1 frame should be released by rollback to s1, B got: {b_s1_after[0]}"
        assert b_s2_after[0] == "t", \
            f"2S-3: Lock in s2 frame should be released by rollback to s1, B got: {b_s2_after[0]}"

    # ------------------------------------------------------------------
    # 2S-4: RELEASE SAVEPOINT does NOT release locks
    #   Session A acquires xact lock 6005 inside a savepoint, then
    #   RELEASEs the savepoint. Session B observes lock is still held.
    # ------------------------------------------------------------------
    def test_2s4_release_preserves_locks(self):
        key = 6005
        barrier_a_released_sp = threading.Event()
        barrier_b_probed = threading.Event()
        b_after_release = [None]

        def session_a_thread():
            self.run_interactive_session([
                ("BEGIN;", None, None),
                ("SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key});", None, None),
                ("RELEASE SAVEPOINT s1;", barrier_a_released_sp, None),
                ("COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_released_sp.wait(timeout=10)
            # Single probe: barrier guarantees A still holds the lock
            # (RELEASE SAVEPOINT merges to parent, does NOT free locks)
            b_after_release[0] = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key});")
            barrier_b_probed.set()

        t_a = threading.Thread(target=session_a_thread)
        t_b = threading.Thread(target=session_b_thread)
        t_a.start()
        t_b.start()
        t_a.join(timeout=20)
        t_b.join(timeout=20)

        assert b_after_release[0] == "f", \
            f"2S-4: RELEASE SAVEPOINT should NOT release lock, B got: {b_after_release[0]}"

    def run_all(self):
        print("\n" + "=" * 60)
        print("Advisory Lock Savepoint Tests (Two-Session)")
        print("=" * 60 + "\n")

        tests = [
            ("2S-1: Basic ROLLBACK TO SAVEPOINT releases xact lock",
             self.test_2s1_basic_rollback_releases),
            ("2S-2: Lock before savepoint survives rollback",
             self.test_2s2_lock_before_savepoint_survives),
            ("2S-3: Nested savepoints — rollback to outer releases both",
             self.test_2s3_nested_savepoints),
            ("2S-4: RELEASE SAVEPOINT preserves locks",
             self.test_2s4_release_preserves_locks),
        ]

        for name, func in tests:
            self.run_test(name, func)

        print("\n" + "=" * 60)
        passed = sum(1 for r in self.results if r.passed)
        failed = len(self.results) - passed
        print(f"Results: {passed} passed, {failed} failed")
        print("=" * 60 + "\n")

        return failed == 0


def main():
    parser = argparse.ArgumentParser(
        description="Two-session advisory lock savepoint tests")
    parser.add_argument("--dsn",
                        default=os.environ.get("PG_DSN"),
                        help="PostgreSQL connection URI "
                             "(e.g. postgres://user:pass@host:port/db)")
    parser.add_argument("--host",
                        default=os.environ.get("PG_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int,
                        default=int(os.environ.get("PG_PORT", "15433")))
    parser.add_argument("--user",
                        default=os.environ.get("PG_USER", "tenant_a.admin"))
    parser.add_argument("--password",
                        default=os.environ.get("PG_PASSWORD", "secret"))
    args = parser.parse_args()

    if args.dsn:
        parsed = urlparse(args.dsn)
        host = parsed.hostname or args.host
        port = parsed.port or args.port
        user = parsed.username or args.user
        password = parsed.password or args.password
        database = (parsed.path or "").lstrip("/") or "postgres"
    else:
        host = args.host
        port = args.port
        user = args.user
        password = args.password
        database = "postgres"

    tests = AdvisoryLockSavepointTests(host, port, user, password, database)
    success = tests.run_all()
    sys.exit(0 if success else 1)


if __name__ == "__main__":
    main()
