#!/usr/bin/env python3

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("tpcc_correctness_check.py")
SPEC = importlib.util.spec_from_file_location("tpcc_correctness_check", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"failed to load module spec for {MODULE_PATH}")
tpcc_correctness_check = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = tpcc_correctness_check
SPEC.loader.exec_module(tpcc_correctness_check)


class TpccCorrectnessCheckTests(unittest.TestCase):
    def test_configured_checks_include_all_four_tpcc_consistency_conditions(self) -> None:
        checks = tpcc_correctness_check.configured_checks()
        ids = {check.check_id for check in checks}
        self.assertIn("warehouse_ytd_matches_district_sum", ids)
        self.assertIn("orders_match_d_next_o_id", ids)
        self.assertIn("sum_o_ol_cnt_matches_order_line_rows", ids)
        self.assertIn("order_line_count_matches_o_ol_cnt", ids)

    def test_validate_phase_prerequisite_accepts_matching_after_prepare_artifact(self) -> None:
        args = argparse.Namespace(
            phase="after_run",
            prior_phase_json=None,
            label="db9",
        )
        conn = tpcc_correctness_check.ConnectionConfig(
            host="127.0.0.1",
            port=5432,
            user="admin",
            password="",
            database="tpcc_db9",
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            artifact = Path(tmpdir) / "after_prepare.json"
            artifact.write_text(
                json.dumps(
                    {
                        "phase": "after_prepare",
                        "label": "db9",
                        "all_passed": True,
                        "connection": {"database": "tpcc_db9"},
                    }
                ),
                encoding="utf-8",
            )
            args.prior_phase_json = str(artifact)
            phase_guard = tpcc_correctness_check.validate_phase_prerequisite(args, conn)
            self.assertEqual(phase_guard["required_prior_phase"], "after_prepare")
            self.assertEqual(phase_guard["validated_database"], "tpcc_db9")

    def test_validate_phase_prerequisite_rejects_failed_after_prepare_artifact(self) -> None:
        args = argparse.Namespace(
            phase="after_run",
            label="db9",
            prior_phase_json=None,
        )
        conn = tpcc_correctness_check.ConnectionConfig(
            host="127.0.0.1",
            port=5432,
            user="admin",
            password="",
            database="tpcc_db9",
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            artifact = Path(tmpdir) / "after_prepare.json"
            artifact.write_text(
                json.dumps(
                    {
                        "phase": "after_prepare",
                        "label": "db9",
                        "all_passed": False,
                        "connection": {"database": "tpcc_db9"},
                    }
                ),
                encoding="utf-8",
            )
            args.prior_phase_json = str(artifact)
            with self.assertRaisesRegex(RuntimeError, "did not pass"):
                tpcc_correctness_check.validate_phase_prerequisite(args, conn)

    def test_validate_phase_prerequisite_rejects_database_mismatch(self) -> None:
        args = argparse.Namespace(
            phase="after_run",
            label="db9",
            prior_phase_json=None,
        )
        conn = tpcc_correctness_check.ConnectionConfig(
            host="127.0.0.1",
            port=5432,
            user="admin",
            password="",
            database="tpcc_db9",
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            artifact = Path(tmpdir) / "after_prepare.json"
            artifact.write_text(
                json.dumps(
                    {
                        "phase": "after_prepare",
                        "label": "db9",
                        "all_passed": True,
                        "connection": {"database": "tpcc_other"},
                    }
                ),
                encoding="utf-8",
            )
            args.prior_phase_json = str(artifact)
            with self.assertRaisesRegex(RuntimeError, "does not match current database"):
                tpcc_correctness_check.validate_phase_prerequisite(args, conn)


if __name__ == "__main__":
    unittest.main()
