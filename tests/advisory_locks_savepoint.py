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
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from typing import List


@dataclass
class TestResult:
    name: str
    passed: bool
    message: str
    duration: float


class AdvisoryLockSavepointTests:
    def __init__(self, host: str, port: int, user: str, password: str):
        self.host = host
        self.port = port
        self.user = user
        self.password = password
        self.results: List[TestResult] = []

    def run_sql(self, sql: str) -> str:
        """Run a single SQL statement and return the result."""
        env = os.environ.copy()
        env["PGPASSWORD"] = self.password
        result = subprocess.run(
            ["psql", "-h", self.host, "-p", str(self.port),
             "-U", self.user, "-d", "postgres", "-t", "-A", "-c", sql],
            capture_output=True, text=True, env=env, timeout=30
        )
        return result.stdout.strip()

    def run_sql_script(self, sql: str) -> str:
        """Run a multi-statement SQL script and return the output."""
        env = os.environ.copy()
        env["PGPASSWORD"] = self.password
        result = subprocess.run(
            ["psql", "-h", self.host, "-p", str(self.port),
             "-U", self.user, "-d", "postgres", "-t", "-A"],
            input=sql, capture_output=True, text=True, env=env, timeout=30
        )
        return result.stdout.strip()

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
        b_before = [None]
        b_after = [None]

        def session_a():
            self.run_sql_script(f"""\
BEGIN;
SAVEPOINT s1;
SELECT pg_advisory_xact_lock({key});
""")
            barrier_a_locked.set()
            # Wait for B to observe the held lock
            barrier_a_rolled_back.wait(timeout=10)
            # Actually we roll back first, then let B re-check
            # Re-sequence: A locks -> B observes held -> A rollback -> B observes free
            # So we need B to observe *before* rollback. Let's fix the flow.

        # Corrected flow: use explicit step-by-step coordination
        def session_a_corrected():
            env = os.environ.copy()
            env["PGPASSWORD"] = self.password
            # Step 1: BEGIN + SAVEPOINT + acquire lock
            self.run_sql_script(f"""\
BEGIN;
SAVEPOINT s1;
SELECT pg_advisory_xact_lock({key});
""")
            barrier_a_locked.set()
            # Wait for session B to observe the held lock
            assert barrier_a_rolled_back.wait(timeout=10), "Timed out waiting for B to observe held lock"
            # Step 2: ROLLBACK TO SAVEPOINT in a new psql (same session won't work)
            # psql runs each invocation in a separate connection, so we need
            # a single long-running script. Use a different approach.

        # Since psql creates a new connection each invocation, we need to run
        # the entire session A flow in a single script. We use a helper that
        # writes commands to psql's stdin incrementally.
        def run_interactive_session(commands_and_barriers):
            """Run psql interactively, sending commands separated by barriers.

            commands_and_barriers is a list of (sql_string, event_to_set, event_to_wait).
            - sql_string: SQL to send
            - event_to_set: threading.Event to set after sending (or None)
            - event_to_wait: threading.Event to wait on before sending (or None)

            Returns all stdout.
            """
            env = os.environ.copy()
            env["PGPASSWORD"] = self.password
            proc = subprocess.Popen(
                ["psql", "-h", self.host, "-p", str(self.port),
                 "-U", self.user, "-d", "postgres", "-t", "-A"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=env
            )
            for sql, evt_set, evt_wait in commands_and_barriers:
                if evt_wait is not None:
                    assert evt_wait.wait(timeout=10), f"Timed out waiting for event before: {sql}"
                proc.stdin.write(sql + "\n")
                proc.stdin.flush()
                # Give the server a moment to process
                time.sleep(0.3)
                if evt_set is not None:
                    evt_set.set()
            stdout, _ = proc.communicate(timeout=30)
            return stdout.strip()

        barrier_b_observed_held = threading.Event()
        barrier_b_probed = threading.Event()

        def session_a_thread():
            run_interactive_session([
                (f"BEGIN;", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key});", barrier_a_locked, None),
                # Wait for B to observe the held lock
                (f"ROLLBACK TO SAVEPOINT s1;", barrier_a_rolled_back, barrier_b_observed_held),
                # Wait for B to probe post-rollback state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            # Wait for A to acquire the lock
            assert barrier_a_locked.wait(timeout=10), "Timed out waiting for A to lock"
            time.sleep(0.2)
            # Observe: lock is held by A
            b_before[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key});")
            barrier_b_observed_held.set()
            # Wait for A to ROLLBACK TO SAVEPOINT
            assert barrier_a_rolled_back.wait(timeout=10), "Timed out waiting for A to rollback"
            time.sleep(0.2)
            # Observe: lock should now be free
            b_after[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key});")
            barrier_b_probed.set()
            # Clean up B's lock
            self.run_sql("ROLLBACK;")

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
        b_key_before_after_rollback = [None]
        b_key_after_after_rollback = [None]

        def run_interactive_session(commands_and_barriers):
            env = os.environ.copy()
            env["PGPASSWORD"] = self.password
            proc = subprocess.Popen(
                ["psql", "-h", self.host, "-p", str(self.port),
                 "-U", self.user, "-d", "postgres", "-t", "-A"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=env
            )
            for sql, evt_set, evt_wait in commands_and_barriers:
                if evt_wait is not None:
                    assert evt_wait.wait(timeout=10), f"Timed out waiting for event"
                proc.stdin.write(sql + "\n")
                proc.stdin.flush()
                time.sleep(0.3)
                if evt_set is not None:
                    evt_set.set()
            stdout, _ = proc.communicate(timeout=30)
            return stdout.strip()

        barrier_b_probed = threading.Event()

        def session_a_thread():
            run_interactive_session([
                (f"BEGIN;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_before});", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_after});", barrier_a_locked, None),
                (f"ROLLBACK TO SAVEPOINT s1;", barrier_a_rolled_back, barrier_b_checked),
                # Wait for B to probe post-rollback state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_locked.wait(timeout=10)
            barrier_b_checked.set()
            assert barrier_a_rolled_back.wait(timeout=10)
            time.sleep(0.2)
            # key_before should still be held by A (acquired before savepoint)
            b_key_before_after_rollback[0] = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key_before});")
            # key_after should be released (acquired after savepoint, rolled back)
            b_key_after_after_rollback[0] = self.run_sql(
                f"SELECT pg_try_advisory_xact_lock({key_after});")
            barrier_b_probed.set()
            self.run_sql("ROLLBACK;")

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
        barrier_a_locked = threading.Event()
        barrier_a_rolled_back = threading.Event()
        barrier_b_checked = threading.Event()
        b_s1_after = [None]
        b_s2_after = [None]

        def run_interactive_session(commands_and_barriers):
            env = os.environ.copy()
            env["PGPASSWORD"] = self.password
            proc = subprocess.Popen(
                ["psql", "-h", self.host, "-p", str(self.port),
                 "-U", self.user, "-d", "postgres", "-t", "-A"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=env
            )
            for sql, evt_set, evt_wait in commands_and_barriers:
                if evt_wait is not None:
                    assert evt_wait.wait(timeout=10), f"Timed out waiting for event"
                proc.stdin.write(sql + "\n")
                proc.stdin.flush()
                time.sleep(0.3)
                if evt_set is not None:
                    evt_set.set()
            stdout, _ = proc.communicate(timeout=30)
            return stdout.strip()

        barrier_b_probed = threading.Event()

        def session_a_thread():
            run_interactive_session([
                (f"BEGIN;", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_s1});", None, None),
                (f"SAVEPOINT s2;", None, None),
                (f"SELECT pg_advisory_xact_lock({key_s2});", barrier_a_locked, None),
                (f"ROLLBACK TO SAVEPOINT s1;", barrier_a_rolled_back, barrier_b_checked),
                # Wait for B to probe post-rollback state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_locked.wait(timeout=10)
            barrier_b_checked.set()
            assert barrier_a_rolled_back.wait(timeout=10)
            time.sleep(0.2)
            b_s1_after[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key_s1});")
            b_s2_after[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key_s2});")
            barrier_b_probed.set()
            self.run_sql("ROLLBACK;")

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
        b_after_release = [None]

        def run_interactive_session(commands_and_barriers):
            env = os.environ.copy()
            env["PGPASSWORD"] = self.password
            proc = subprocess.Popen(
                ["psql", "-h", self.host, "-p", str(self.port),
                 "-U", self.user, "-d", "postgres", "-t", "-A"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=env
            )
            for sql, evt_set, evt_wait in commands_and_barriers:
                if evt_wait is not None:
                    assert evt_wait.wait(timeout=10), f"Timed out waiting for event"
                proc.stdin.write(sql + "\n")
                proc.stdin.flush()
                time.sleep(0.3)
                if evt_set is not None:
                    evt_set.set()
            stdout, _ = proc.communicate(timeout=30)
            return stdout.strip()

        barrier_b_probed = threading.Event()

        def session_a_thread():
            run_interactive_session([
                (f"BEGIN;", None, None),
                (f"SAVEPOINT s1;", None, None),
                (f"SELECT pg_advisory_xact_lock({key});", None, None),
                (f"RELEASE SAVEPOINT s1;", barrier_a_released_sp, None),
                # Wait for B to probe post-release state before committing
                (f"COMMIT;", None, barrier_b_probed),
            ])

        def session_b_thread():
            assert barrier_a_released_sp.wait(timeout=10)
            time.sleep(0.2)
            # Lock should still be held (RELEASE merges to parent, doesn't free)
            b_after_release[0] = self.run_sql(f"SELECT pg_try_advisory_xact_lock({key});")
            barrier_b_probed.set()
            self.run_sql("ROLLBACK;")

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
            time.sleep(0.2)

        print("\n" + "=" * 60)
        passed = sum(1 for r in self.results if r.passed)
        failed = len(self.results) - passed
        print(f"Results: {passed} passed, {failed} failed")
        print("=" * 60 + "\n")

        return failed == 0


def main():
    parser = argparse.ArgumentParser(
        description="Two-session advisory lock savepoint tests")
    parser.add_argument("--host",
                        default=os.environ.get("PG_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int,
                        default=int(os.environ.get("PG_PORT", "15433")))
    parser.add_argument("--user",
                        default=os.environ.get("PG_USER", "tenant_a.admin"))
    parser.add_argument("--password",
                        default=os.environ.get("PG_PASSWORD", "secret"))
    args = parser.parse_args()

    tests = AdvisoryLockSavepointTests(
        args.host, args.port, args.user, args.password)
    success = tests.run_all()
    sys.exit(0 if success else 1)


if __name__ == "__main__":
    main()
