#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("benchmark_m1_baseline.py")
SPEC = importlib.util.spec_from_file_location("benchmark_m1_baseline", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"failed to load module spec for {MODULE_PATH}")
benchmark_m1_baseline = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark_m1_baseline
SPEC.loader.exec_module(benchmark_m1_baseline)


def make_raw(engine_label: str, engine_version: str, elapsed_ms: float) -> dict:
    aggregate_summary = {
        "count": 1,
        "elapsed_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
        "p50_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
        "p95_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
        "first_row_latency_ms": {"value": elapsed_ms / 2.0, "unit": "ms", "source": "client"},
        "elapsed_stdev_ms": {"value": 0.0, "unit": "ms", "source": "client"},
        "elapsed_ci95_low_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
        "elapsed_ci95_high_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
        "elapsed_outlier_count": {"value": 0.0, "unit": "count", "source": "tukey_iqr_1_5x"},
    }
    return {
        "schema_version": benchmark_m1_baseline.SCHEMA_VERSION,
        "meta": {
            "engine_label": engine_label,
            "engine_version": engine_version,
            "git_sha": "abc123",
            "timestamp_utc": "2026-04-12T00:00:00+00:00",
            "hostname": "host",
            "platform": "platform",
            "harness_version": "test",
            "dsn_redacted": "postgresql://user:***@127.0.0.1:5432/test",
            "cpu_model": "cpu",
            "cpu_count_logical": 8,
            "python": "3.9",
        },
        "dataset": {
            "dataset_id": "m1_baseline_S_20260412",
            "scale": "S",
            "seed": 20260412,
            "tables": [],
            "validation_checks": [],
        },
        "run_config": {
            "scenario_set": "m2",
            "warmup": 1,
            "measured": 1,
            "repeats": 1,
            "protocol_mode": None,
            "server_settings": {},
        },
        "scenarios": [
            {
                "scenario_id": "Q01",
                "milestone_focus": "M2",
                "variant_kind": "base",
                "parameter_pack_id": "hot_page",
                "sql": "SELECT 1",
                "params": [1, 2],
                "execution_mode": "extended_prepared",
                "submode": None,
                "worker_count": None,
                "memory_limit_mb": None,
                "explain": ["Seq Scan"],
                "applied_settings": {},
                "repeats": [
                    {
                        "repeat": 1,
                        "summary": {
                            "count": 1,
                            "elapsed_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
                            "p50_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
                            "p95_ms": {"value": elapsed_ms, "unit": "ms", "source": "client"},
                            "first_row_latency_ms": {"value": elapsed_ms / 2.0, "unit": "ms", "source": "client"},
                            "peak_rss_bytes": {"value": None, "unit": "bytes", "source": None},
                            "cpu_time_ms": {"value": None, "unit": "ms", "source": None},
                        },
                        "counters": {
                            "scanned_rows": {"value": None, "unit": "rows", "source": None},
                            "decoded_rows": {"value": None, "unit": "rows", "source": None},
                            "fetched_base_rows": {"value": None, "unit": "rows", "source": None},
                            "rows_scanned_before_stop": {"value": None, "unit": "rows", "source": None},
                            "payload_bytes": {"value": None, "unit": "bytes", "source": None},
                            "kv_requests": {"value": None, "unit": "count", "source": None},
                            "sort_bytes": {"value": None, "unit": "bytes", "source": None},
                            "aggregate_memory_bytes": {"value": None, "unit": "bytes", "source": None},
                            "spill_bytes": {"value": None, "unit": "bytes", "source": None},
                            "spill_passes": {"value": None, "unit": "count", "source": None},
                            "plan_cache_hit_ratio": {"value": None, "unit": "ratio", "source": None},
                            "planning_cpu_ms": {"value": None, "unit": "ms", "source": None},
                            "cleanup_ok": None,
                        },
                        "measurements": {
                            "elapsed_ms": [elapsed_ms],
                            "first_row_latency_ms": [elapsed_ms / 2.0],
                        },
                    }
                ],
                "aggregate": {
                    "repeat_count": 1,
                    "outlier_rule": "tukey_iqr_1_5x",
                    "summary": aggregate_summary,
                },
                "representative_repeat": 1,
            }
        ],
    }


class BenchmarkM1BaselineTests(unittest.TestCase):
    def test_parse_prom_metric_spec_with_labels(self) -> None:
        spec = benchmark_m1_baseline.parse_prom_metric_spec(
            'kv_requests=db9_server_kv_requests_total{kind="cop",tenant="t1"}'
        )
        self.assertEqual(spec.alias, "kv_requests")
        self.assertEqual(spec.metric_name, "db9_server_kv_requests_total")
        self.assertEqual(spec.labels, (("kind", "cop"), ("tenant", "t1")))

    def test_prometheus_snapshot_lookup_with_labels(self) -> None:
        snapshot = benchmark_m1_baseline.parse_prometheus_text(
            """
            # HELP db9_server_kv_requests_total test
            # TYPE db9_server_kv_requests_total counter
            db9_server_kv_requests_total{kind="cop",tenant="t1"} 10
            db9_server_kv_requests_total{kind="cop",tenant="t2"} 20
            db9_server_kv_requests_total{kind="txn",tenant="t1"} 30
            """
        )
        spec = benchmark_m1_baseline.parse_prom_metric_spec(
            'kv_requests=db9_server_kv_requests_total{kind="cop"}'
        )
        value = benchmark_m1_baseline.get_metric_snapshot_value(snapshot, spec)
        self.assertEqual(value, 30.0)

    def test_m2_scenario_set_contains_expected_ids(self) -> None:
        scenarios = benchmark_m1_baseline.get_scenarios_for_set("m2")
        names = [(item.scenario_id, item.variant_kind, item.submode) for item in scenarios]
        self.assertIn(("Q01", "base", None), names)
        self.assertIn(("Q01F", "function_companion", None), names)
        self.assertIn(("Q02", "base", "consume_all"), names)
        self.assertIn(("Q02", "base", "consume_3_then_close"), names)
        self.assertIn(("Q02F", "function_companion", "consume_all"), names)
        self.assertIn(("Q02F", "function_companion", "consume_3_then_close"), names)

    def test_compare_smoke(self) -> None:
        before = make_raw("db9_before", "db9 before", 10.0)
        after = make_raw("db9_after", "db9 after", 7.0)
        postgres = make_raw("postgres_18_3", "PostgreSQL 18.3", 6.0)

        with tempfile.TemporaryDirectory() as tmpdir:
            root = Path(tmpdir)
            before_path = root / "before.json"
            after_path = root / "after.json"
            postgres_path = root / "postgres.json"
            output_path = root / "compare.json"
            before_path.write_text(json.dumps(before), encoding="utf-8")
            after_path.write_text(json.dumps(after), encoding="utf-8")
            postgres_path.write_text(json.dumps(postgres), encoding="utf-8")

            rc = benchmark_m1_baseline.main(
                [
                    "compare",
                    "--before",
                    str(before_path),
                    "--after",
                    str(after_path),
                    "--postgres",
                    str(postgres_path),
                    "--output",
                    str(output_path),
                ]
            )
            self.assertEqual(rc, 0)

            compare_payload = json.loads(output_path.read_text(encoding="utf-8"))
            self.assertEqual(compare_payload["schema_version"], "m1-benchmark-compare/v1")
            self.assertEqual(len(compare_payload["comparisons"]), 1)

            metrics = compare_payload["comparisons"][0]["metrics"]
            self.assertEqual(metrics["elapsed_ms"]["after"], 7.0)
            self.assertEqual(metrics["elapsed_ms"]["postgres_18_3"], 6.0)
            self.assertEqual(metrics["elapsed_ms"]["unit"], "ms")
            self.assertIn("cleanup_ok", metrics)

    def test_compare_includes_extra_metrics(self) -> None:
        before = make_raw("db9_before", "db9 before", 10.0)
        after = make_raw("db9_after", "db9 after", 7.0)
        postgres = make_raw("postgres_18_3", "PostgreSQL 18.3", 6.0)
        before["scenarios"][0]["repeats"][0]["extra_metrics"] = {
            "custom_counter": {"value": 10.0, "unit": None, "source": "prometheus:test_total"}
        }
        after["scenarios"][0]["repeats"][0]["extra_metrics"] = {
            "custom_counter": {"value": 14.0, "unit": None, "source": "prometheus:test_total"}
        }
        postgres["scenarios"][0]["repeats"][0]["extra_metrics"] = {
            "custom_counter": {"value": 9.0, "unit": None, "source": "prometheus:test_total"}
        }

        comparisons = benchmark_m1_baseline.compare_scenarios(before, after, postgres)
        metrics = comparisons[0]["metrics"]
        self.assertIn("custom_counter", metrics)
        self.assertEqual(metrics["custom_counter"]["before"], 10.0)
        self.assertEqual(metrics["custom_counter"]["after"], 14.0)
        self.assertEqual(metrics["custom_counter"]["postgres_18_3"], 9.0)

    def test_compare_prefers_aggregate_summary_over_representative_repeat(self) -> None:
        before = make_raw("db9_before", "db9 before", 10.0)
        after = make_raw("db9_after", "db9 after", 7.0)
        postgres = make_raw("postgres_18_3", "PostgreSQL 18.3", 6.0)

        before["scenarios"][0]["aggregate"]["summary"]["elapsed_ms"]["value"] = 100.0
        after["scenarios"][0]["aggregate"]["summary"]["elapsed_ms"]["value"] = 70.0
        postgres["scenarios"][0]["aggregate"]["summary"]["elapsed_ms"]["value"] = 60.0

        comparisons = benchmark_m1_baseline.compare_scenarios(before, after, postgres)
        metrics = comparisons[0]["metrics"]
        self.assertEqual(metrics["elapsed_ms"]["before"], 100.0)
        self.assertEqual(metrics["elapsed_ms"]["after"], 70.0)
        self.assertEqual(metrics["elapsed_ms"]["postgres_18_3"], 60.0)
        self.assertEqual(comparisons[0]["summary_source"], "aggregate")


if __name__ == "__main__":
    unittest.main()
