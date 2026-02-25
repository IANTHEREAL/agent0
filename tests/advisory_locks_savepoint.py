#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.8"
# dependencies = []
# ///
"""
Two-session advisory lock savepoint tests for db9-server.

Proves that ROLLBACK TO SAVEPOINT releases xact advisory locks at the
rollback boundary (not just at COMMIT) by observing lock state from a
second session.  Keys 6000–6003 to avoid collisions with single-session
tests (5000-range).

Follows the pattern of tests/34_concurrent_transactions.py.
"""

import argparse
import os
import queue
import subprocess
import sys
import threading
import time
from urllib.parse import urlparse
from dataclasses import dataclass
from typing import List


@dataclass
class TestResult:
    name: str
    passed: bool
    message: str
    duration: float


class AdvisoryLockSavepointTests:
    def __init__(self, host: str, port: int, user: str, password: str, database: str):
        self.host = host
        self.port = port
        self.user = user
        self.password = password
        self.database = database
        self.results: List[TestResult] = []

    def run_sql(self, sql: str) -> str:
        """Run a single SQL statement and return the result."""
        env = os.environ.copy()
        env["PGPASSWORD"] = self.password
        try:
            result = subprocess.run(
                ["psql", "-h", self.host, "-p", str(self.port),
                 "-U", self.user, "-d", self.database, "-v", "ON_ERROR_STOP=1", "-t", "-A", "-c", sql],
                capture_output=True, text=True, env=env, timeout=30
            )
        except subprocess.TimeoutExpired as e:
            raise RuntimeError(f"psql timed out after 30s: {sql[:100]}") from e
        if result.returncode != 0:
            raise RuntimeError(
                f"psql failed (exit {result.returncode}): {result.stderr.strip()}")
        return result.stdout.strip()

    def wait_for_advisory_release(self, key: int):
        """Block until another session releases a session advisory lock."""
        self.run_sql(
            f"SELECT pg_advisory_lock({key}); SELECT pg_advisory_unlock({key});")

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

    def run_interactive_session(self, commands_and_barriers):
        """Run psql interactively, sending commands separated by barriers.

        commands_and_barriers is a list of (sql_string, event_to_set, event_to_wait).
        - sql_string: SQL to send
        - event_to_set: threading.Event to set after server confirms execution (or None)
        - event_to_wait: threading.Event to wait on before sending (or None)

        Uses sentinel queries to confirm each command has been processed by the
        server, replacing sleep-based timing with deterministic synchronization.
        """
        env = os.environ.copy()
        env["PGPASSWORD"] = self.password
        proc = subprocess.Popen(
            ["psql", "-h", self.host, "-p", str(self.port),
             "-U", self.user, "-d", self.database, "-v", "ON_ERROR_STOP=1", "-t", "-A"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, env=env
        )

        collected = []
        stdout_q = queue.Queue()
        stderr_lines = []

        def drain_stdout():
            for line in proc.stdout:
                collected.append(line)
                stdout_q.put(line)
            stdout_q.put(None)

        def drain_stderr():
            for line in proc.stderr:
                stderr_lines.append(line)

        reader = threading.Thread(target=drain_stdout, daemon=True)
        err_reader = threading.Thread(target=drain_stderr, daemon=True)
        reader.start()
        err_reader.start()

        try:
            for i, (sql, evt_set, evt_wait) in enumerate(commands_and_barriers):
                if evt_wait is not None:
                    if not evt_wait.wait(timeout=10):
                        raise RuntimeError(
                            f"Timed out waiting for event before: {sql}")
                sentinel = f"__sentinel_{i}__"
                proc.stdin.write(sql + "\n")
                proc.stdin.write(f"SELECT '{sentinel}';\n")
                proc.stdin.flush()
                # Block on stdout queue until sentinel appears.
                deadline = time.time() + 10
                while True:
                    remaining = deadline - time.time()
                    if remaining <= 0:
                        raise RuntimeError(
                            f"Timed out waiting for server to process: {sql}")
                    try:
                        line = stdout_q.get(timeout=remaining)
                    except queue.Empty as e:
                        raise RuntimeError(
                            f"Timed out waiting for server to process: {sql}") from e
                    if line is None:
                        stderr_text = "".join(stderr_lines).strip()
                        raise RuntimeError(
                            "interactive psql exited before command completed "
                            f"(exit {proc.returncode}): {stderr_text}")
                    if sentinel in line:
                        break
                if evt_set is not None:
                    evt_set.set()
            proc.stdin.close()
            proc.wait(timeout=30)
            stderr_text = "".join(stderr_lines).strip()
            if proc.returncode != 0:
                raise RuntimeError(
                    f"interactive psql failed (exit {proc.returncode}): {stderr_text}")
            if "ERROR:" in stderr_text:
                raise RuntimeError(
                    f"interactive psql reported error on stderr: {stderr_text}")
        except Exception:
            proc.kill()
            proc.wait(timeout=5)
            raise
        finally:
            reader.join(timeout=5)
            err_reader.join(timeout=5)

        return "".join(collected).strip()

    # ------------------------------------------------------------------
    # 2S-1: Basic xact lock released on ROLLBACK TO SAVEPOINT
    #   Session A acquires xact lock 6000 inside a savepoint, then rolls
    #   back.  Session B observes that the lock is freed before A commits.
    # ------------------------------------------------------------------
    def test_2s1_basic_rollback_releases(self):
        key = 6000
        sync_key = 6100
        barrier_a_ready = threading.Event()
        barrier_b_observed_held = threading.Event()
        barrier_b_probed = threading.Event()
        b_before = [None]
        b_after = [None]

        def session_a_thread():
            self.run_interactive_session([
                (f"BEGIN;", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key});", None, None),
                (f"SELECT pg_advisory_lock({sync_key});", barrier_a_ready, None),
                # Wait for B to observe held state before rollback.
                (f"ROLLBACK TO SAVEPOINT s1;", None, barrier_b_observed_held),
                (f"SELECT pg_advisory_unlock({sync_key});", None, None),
                # Wait for B to probe post-rollback state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            # Wait for A to acquire xact lock + sync lock.
            assert barrier_a_ready.wait(timeout=10), "Timed out waiting for A to lock"
            # Observe: lock is held by A
            b_before[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key});")
            barrier_b_observed_held.set()
            # Block until A releases the sync lock after rollback.
            self.wait_for_advisory_release(sync_key)
            # Observe: lock should now be free
            b_after[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key});")
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
        sync_key = 6101
        barrier_a_ready = threading.Event()
        barrier_b_checked = threading.Event()
        barrier_b_probed = threading.Event()
        b_key_before_after_rollback = [None]
        b_key_after_after_rollback = [None]

        def session_a_thread():
            self.run_interactive_session([
                (f"BEGIN;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_before});", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_after});", None, None),
                (f"SELECT pg_advisory_lock({sync_key});", barrier_a_ready, None),
                (f"ROLLBACK TO SAVEPOINT s1;", None, barrier_b_checked),
                (f"SELECT pg_advisory_unlock({sync_key});", None, None),
                # Wait for B to probe post-rollback state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_ready.wait(timeout=10)
            barrier_b_checked.set()
            self.wait_for_advisory_release(sync_key)
            # key_before should still be held by A (acquired before savepoint)
            b_key_before_after_rollback[0] = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key_before});")
            # key_after should be released (acquired after savepoint, rolled back)
            b_key_after_after_rollback[0] = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key_after});")
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
        key_s2 = 6004  # Using 6004 instead of conflicting with other cases
        sync_key = 6102
        barrier_a_ready = threading.Event()
        barrier_b_checked = threading.Event()
        barrier_b_probed = threading.Event()
        b_s1_after = [None]
        b_s2_after = [None]

        def session_a_thread():
            self.run_interactive_session([
                (f"BEGIN;", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_s1});", None, None),
                (f"SAVEPOINT s2;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_s2});", None, None),
                (f"SELECT pg_advisory_lock({sync_key});", barrier_a_ready, None),
                (f"ROLLBACK TO SAVEPOINT s1;", None, barrier_b_checked),
                (f"SELECT pg_advisory_unlock({sync_key});", None, None),
                # Wait for B to probe post-rollback state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_ready.wait(timeout=10)
            barrier_b_checked.set()
            self.wait_for_advisory_release(sync_key)
            b_s1_after[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key_s1});")
            b_s2_after[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key_s2});")
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
                (f"BEGIN;", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key});", None, None),
                (f"RELEASE SAVEPOINT s1;", barrier_a_released_sp, None),
                # Wait for B to probe post-release state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_released_sp.wait(timeout=10)
            # Lock should still be held (RELEASE merges to parent, doesn't free)
            b_after_release[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key});")
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
                        help="PostgreSQL DSN, e.g. postgres://user:pass@host:port/db")
    parser.add_argument("--host",
                        default=os.environ.get("PG_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int,
                        default=int(os.environ.get("PG_PORT", "15433")))
    parser.add_argument("--user",
                        default=os.environ.get("PG_USER", "tenant_a.admin"))
    parser.add_argument("--password",
                        default=os.environ.get("PG_PASSWORD", "secret"))
    parser.add_argument("--database",
                        default=os.environ.get("PG_DATABASE", "postgres"))
    args = parser.parse_args()

    host = args.host
    port = args.port
    user = args.user
    password = args.password
    database = args.database

    if args.dsn:
        parsed = urlparse(args.dsn)
        if parsed.scheme not in ("postgres", "postgresql"):
            raise ValueError(f"Unsupported DSN scheme: {parsed.scheme}")
        host = parsed.hostname or host
        port = parsed.port or port
        if parsed.username:
            user = parsed.username
        if parsed.password is not None:
            password = parsed.password
        if parsed.path and parsed.path != "/":
            database = parsed.path.lstrip("/")

    tests = AdvisoryLockSavepointTests(
        host, port, user, password, database)
    success = tests.run_all()
    sys.exit(0 if success else 1)


if __name__ == "__main__":
    main()
