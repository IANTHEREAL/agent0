#!/usr/bin/env python3
"""
M1 benchmark baseline harness for db9-server.

This script follows the contracts in:
  - docs/design/benchmark_m1_baseline.md
  - docs/design/benchmark_m1_generator_and_harness.md
  - docs/design/benchmark_m1_result_schema.json

Modes:
1) Generate deterministic benchmark data:
   python3 scripts/benchmark_m1_baseline.py gen \
     --dsn postgres://admin:admin@127.0.0.1:5433/postgres \
     --scale S \
     --seed 20260412

2) Run one engine variant:
   python3 scripts/benchmark_m1_baseline.py run \
     --dsn postgres://admin:admin@127.0.0.1:5433/postgres \
     --engine-label db9_after \
     --scale S \
     --scenario-set m2_m4 \
     --output /tmp/db9_after_S.json

3) Compare three raw outputs:
   python3 scripts/benchmark_m1_baseline.py compare \
     --before /tmp/db9_before_S.json \
     --after /tmp/db9_after_S.json \
     --postgres /tmp/postgres_18_3_S.json \
     --output /tmp/m1_compare.json
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import os
import platform
import re
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.request import urlopen
from urllib.parse import urlsplit, urlunsplit


SCHEMA_VERSION = "m1-benchmark-result/v1"
COMPARE_SCHEMA_VERSION = "m1-benchmark-compare/v1"

DEFAULT_DSN = "postgres://admin:admin@127.0.0.1:5433/postgres"
DEFAULT_SEED = 20260412
DEFAULT_BATCH_SIZE = 2_000
DEFAULT_WARMUP = 3
DEFAULT_MEASURED = 7
DEFAULT_REPEATS = 1

BASE_TS = dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc)
COUNTRIES = [
    "US",
    "CN",
    "JP",
    "DE",
    "GB",
    "FR",
    "IN",
    "BR",
    "CA",
    "AU",
]
CHANNELS = ["push", "email", "ads", "organic", "partner"]
NON_PUSH_CHANNELS = [c for c in CHANNELS if c != "push"]
DEVICE_TYPES = ["ios", "android", "web", "tablet"]
PLAN_CODES = ["free", "pro", "team", "enterprise"]
REGIONS = ["apac", "emea", "latam", "na"]
ORDER_STATUSES = [1, 2, 3, 4, 5]
EVENT_TYPES = [1, 2, 3, 4, 5, 6, 7, 8]

PROM_LINE_RE = re.compile(
    r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{([^}]*)\})?\s+"
    r"([-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][-+]?\d+)?)"
    r"(?:\s+\d+)?$"
)
PROM_LABEL_RE = re.compile(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:\\\\.|[^"])*)"')


@dataclass(frozen=True)
class ScaleConfig:
    label: str
    users: int
    orders: int
    events: int


@dataclass(frozen=True)
class ScenarioDef:
    scenario_id: str
    name: str
    milestone_focus: str
    variant_kind: str
    parameter_pack_id: str
    sql: str
    param_keys: tuple[str, ...]
    scenario_sets: tuple[str, ...]
    execution_mode: str = "extended_prepared"
    submode: str | None = None
    consume_rows: int | None = None
    worker_count: int | None = None
    memory_limit_mb: int | None = None
    include_explain: bool = True


@dataclass(frozen=True)
class PromMetricSpec:
    alias: str
    metric_name: str
    labels: tuple[tuple[str, str], ...] = ()


SCALES: dict[str, ScaleConfig] = {
    "S": ScaleConfig(label="S", users=100_000, orders=200_000, events=1_000_000),
    "M": ScaleConfig(label="M", users=250_000, orders=800_000, events=5_000_000),
    "L": ScaleConfig(label="L", users=500_000, orders=2_000_000, events=10_000_000),
}


SCENARIOS: list[ScenarioDef] = [
    ScenarioDef(
        scenario_id="Q01",
        name="orders_page_full_row",
        milestone_focus="M2",
        variant_kind="base",
        parameter_pack_id="hot_page",
        sql=(
            "SELECT * "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "status"),
        scenario_sets=("all", "m2", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q01F",
        name="orders_page_full_row_fn",
        milestone_focus="M2",
        variant_kind="function_companion",
        parameter_pack_id="hot_page_fn",
        sql=(
            "SELECT * "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "  AND lower(channel) = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "status", "channel"),
        scenario_sets=("all", "m2", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q02",
        name="events_stream_first_page",
        milestone_focus="M2",
        variant_kind="base",
        parameter_pack_id="events_hot",
        sql=(
            "SELECT * "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "ORDER BY created_at DESC, event_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id",),
        scenario_sets=("all", "m2", "m2_m4"),
        submode="consume_all",
    ),
    ScenarioDef(
        scenario_id="Q02",
        name="events_stream_first_page_early_close",
        milestone_focus="M2",
        variant_kind="base",
        parameter_pack_id="events_hot",
        sql=(
            "SELECT * "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "ORDER BY created_at DESC, event_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id",),
        scenario_sets=("all", "m2", "m2_m4"),
        submode="consume_3_then_close",
        consume_rows=3,
    ),
    ScenarioDef(
        scenario_id="Q02F",
        name="events_stream_first_page_fn",
        milestone_focus="M2",
        variant_kind="function_companion",
        parameter_pack_id="events_hot_fn",
        sql=(
            "SELECT * "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND abs(priority_bucket) = %s "
            "ORDER BY created_at DESC, event_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "priority_abs"),
        scenario_sets=("all", "m2", "m2_m4"),
        submode="consume_all",
    ),
    ScenarioDef(
        scenario_id="Q02F",
        name="events_stream_first_page_fn_early_close",
        milestone_focus="M2",
        variant_kind="function_companion",
        parameter_pack_id="events_hot_fn",
        sql=(
            "SELECT * "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND abs(priority_bucket) = %s "
            "ORDER BY created_at DESC, event_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "priority_abs"),
        scenario_sets=("all", "m2", "m2_m4"),
        submode="consume_3_then_close",
        consume_rows=3,
    ),
    ScenarioDef(
        scenario_id="Q03",
        name="orders_page_narrow_projection",
        milestone_focus="M3",
        variant_kind="base",
        parameter_pack_id="hot_page",
        sql=(
            "SELECT order_id, created_at, amount_cents "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "status"),
        scenario_sets=("all", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q03F",
        name="orders_page_narrow_projection_fn",
        milestone_focus="M3",
        variant_kind="function_companion",
        parameter_pack_id="hot_page_fn",
        sql=(
            "SELECT order_id, created_at, amount_cents "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "  AND lower(channel) = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "status", "channel"),
        scenario_sets=("all", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q04",
        name="orders_ordered_topn",
        milestone_focus="M4",
        variant_kind="base",
        parameter_pack_id="hot_page",
        sql=(
            "SELECT order_id, created_at, amount_cents, score "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "status"),
        scenario_sets=("all", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q04F",
        name="orders_ordered_topn_fn",
        milestone_focus="M4",
        variant_kind="function_companion",
        parameter_pack_id="hot_page_fn",
        sql=(
            "SELECT order_id, created_at, amount_cents, score "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "  AND lower(channel) = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10"
        ),
        param_keys=("tenant_id", "status", "channel"),
        scenario_sets=("all", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q05",
        name="orders_ordered_offset",
        milestone_focus="M4",
        variant_kind="base",
        parameter_pack_id="hot_offset",
        sql=(
            "SELECT order_id, created_at, amount_cents, score "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10 OFFSET %s"
        ),
        param_keys=("tenant_id", "status", "offset"),
        scenario_sets=("all", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q05F",
        name="orders_ordered_offset_fn",
        milestone_focus="M4",
        variant_kind="function_companion",
        parameter_pack_id="hot_offset_fn",
        sql=(
            "SELECT order_id, created_at, amount_cents, score "
            "FROM bench_orders "
            "WHERE tenant_id = %s "
            "  AND status = %s "
            "  AND lower(channel) = %s "
            "ORDER BY created_at DESC, order_id DESC "
            "LIMIT 10 OFFSET %s"
        ),
        param_keys=("tenant_id", "status", "channel", "offset"),
        scenario_sets=("all", "m2_m4"),
    ),
    ScenarioDef(
        scenario_id="Q06",
        name="events_ordered_group_by",
        milestone_focus="M5",
        variant_kind="base",
        parameter_pack_id="agg_hot",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id",),
        scenario_sets=("all", "m5"),
    ),
    ScenarioDef(
        scenario_id="Q06F",
        name="events_ordered_group_by_fn",
        milestone_focus="M5",
        variant_kind="function_companion",
        parameter_pack_id="agg_hot_fn",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND lower(channel) = %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "channel"),
        scenario_sets=("all", "m5"),
    ),
    ScenarioDef(
        scenario_id="Q07",
        name="users_prepared_lookup_hot",
        milestone_focus="M6",
        variant_kind="base",
        parameter_pack_id="orm_hot",
        sql=(
            "SELECT user_id, status, region, plan_code "
            "FROM bench_users "
            "WHERE tenant_id = %s "
            "  AND user_id = %s"
        ),
        param_keys=("tenant_id", "user_id"),
        scenario_sets=("all", "m6"),
        submode="hot_only",
        include_explain=False,
    ),
    ScenarioDef(
        scenario_id="Q07",
        name="users_prepared_lookup_mixed",
        milestone_focus="M6",
        variant_kind="base",
        parameter_pack_id="orm_mixed",
        sql=(
            "SELECT user_id, status, region, plan_code "
            "FROM bench_users "
            "WHERE tenant_id = %s "
            "  AND user_id = %s"
        ),
        param_keys=("tenant_id", "user_id"),
        scenario_sets=("all", "m6"),
        submode="mixed_tenants",
        include_explain=False,
    ),
    ScenarioDef(
        scenario_id="Q07F",
        name="users_prepared_lookup_hot_fn",
        milestone_focus="M6",
        variant_kind="function_companion",
        parameter_pack_id="orm_hot",
        sql=(
            "SELECT user_id, status, region, plan_code "
            "FROM bench_users "
            "WHERE tenant_id = %s "
            "  AND lower(email) = %s"
        ),
        param_keys=("tenant_id", "email"),
        scenario_sets=("all", "m6"),
        submode="hot_only",
        include_explain=False,
    ),
    ScenarioDef(
        scenario_id="Q07F",
        name="users_prepared_lookup_mixed_fn",
        milestone_focus="M6",
        variant_kind="function_companion",
        parameter_pack_id="orm_mixed",
        sql=(
            "SELECT user_id, status, region, plan_code "
            "FROM bench_users "
            "WHERE tenant_id = %s "
            "  AND lower(email) = %s"
        ),
        param_keys=("tenant_id", "email"),
        scenario_sets=("all", "m6"),
        submode="mixed_tenants",
        include_explain=False,
    ),
    ScenarioDef(
        scenario_id="Q08",
        name="events_parallel_analytics_w1",
        milestone_focus="M7",
        variant_kind="base",
        parameter_pack_id="events_range_hot",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end"),
        scenario_sets=("all", "m7"),
        worker_count=1,
    ),
    ScenarioDef(
        scenario_id="Q08",
        name="events_parallel_analytics_w2",
        milestone_focus="M7",
        variant_kind="base",
        parameter_pack_id="events_range_hot",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end"),
        scenario_sets=("all", "m7"),
        worker_count=2,
    ),
    ScenarioDef(
        scenario_id="Q08",
        name="events_parallel_analytics_w4",
        milestone_focus="M7",
        variant_kind="base",
        parameter_pack_id="events_range_hot",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end"),
        scenario_sets=("all", "m7"),
        worker_count=4,
    ),
    ScenarioDef(
        scenario_id="Q08",
        name="events_parallel_analytics_w8",
        milestone_focus="M7",
        variant_kind="base",
        parameter_pack_id="events_range_hot",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end"),
        scenario_sets=("all", "m7"),
        worker_count=8,
    ),
    ScenarioDef(
        scenario_id="Q08F",
        name="events_parallel_analytics_fn_w1",
        milestone_focus="M7",
        variant_kind="function_companion",
        parameter_pack_id="events_range_hot_fn",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "  AND lower(channel) = %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end", "channel"),
        scenario_sets=("all", "m7"),
        worker_count=1,
    ),
    ScenarioDef(
        scenario_id="Q08F",
        name="events_parallel_analytics_fn_w2",
        milestone_focus="M7",
        variant_kind="function_companion",
        parameter_pack_id="events_range_hot_fn",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "  AND lower(channel) = %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end", "channel"),
        scenario_sets=("all", "m7"),
        worker_count=2,
    ),
    ScenarioDef(
        scenario_id="Q08F",
        name="events_parallel_analytics_fn_w4",
        milestone_focus="M7",
        variant_kind="function_companion",
        parameter_pack_id="events_range_hot_fn",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "  AND lower(channel) = %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end", "channel"),
        scenario_sets=("all", "m7"),
        worker_count=4,
    ),
    ScenarioDef(
        scenario_id="Q08F",
        name="events_parallel_analytics_fn_w8",
        milestone_focus="M7",
        variant_kind="function_companion",
        parameter_pack_id="events_range_hot_fn",
        sql=(
            "SELECT created_day, country, device_type, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND created_day BETWEEN %s AND %s "
            "  AND lower(channel) = %s "
            "GROUP BY created_day, country, device_type "
            "ORDER BY created_day, country, device_type"
        ),
        param_keys=("tenant_id", "day_start", "day_end", "channel"),
        scenario_sets=("all", "m7"),
        worker_count=8,
    ),
    ScenarioDef(
        scenario_id="Q09",
        name="events_spill_sort_64mb",
        milestone_focus="M8",
        variant_kind="base",
        parameter_pack_id="events_hot",
        sql=(
            "SELECT event_id, created_at, score "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "ORDER BY score DESC, event_id DESC "
            "LIMIT 50000"
        ),
        param_keys=("tenant_id",),
        scenario_sets=("all", "m8"),
        memory_limit_mb=64,
    ),
    ScenarioDef(
        scenario_id="Q09",
        name="events_spill_sort_128mb",
        milestone_focus="M8",
        variant_kind="base",
        parameter_pack_id="events_hot",
        sql=(
            "SELECT event_id, created_at, score "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "ORDER BY score DESC, event_id DESC "
            "LIMIT 50000"
        ),
        param_keys=("tenant_id",),
        scenario_sets=("all", "m8"),
        memory_limit_mb=128,
    ),
    ScenarioDef(
        scenario_id="Q09F",
        name="events_spill_sort_fn_64mb",
        milestone_focus="M8",
        variant_kind="function_companion",
        parameter_pack_id="events_hot_fn",
        sql=(
            "SELECT event_id, created_at, score "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND abs(priority_bucket) = %s "
            "ORDER BY score DESC, event_id DESC "
            "LIMIT 50000"
        ),
        param_keys=("tenant_id", "priority_abs"),
        scenario_sets=("all", "m8"),
        memory_limit_mb=64,
    ),
    ScenarioDef(
        scenario_id="Q09F",
        name="events_spill_sort_fn_128mb",
        milestone_focus="M8",
        variant_kind="function_companion",
        parameter_pack_id="events_hot_fn",
        sql=(
            "SELECT event_id, created_at, score "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND abs(priority_bucket) = %s "
            "ORDER BY score DESC, event_id DESC "
            "LIMIT 50000"
        ),
        param_keys=("tenant_id", "priority_abs"),
        scenario_sets=("all", "m8"),
        memory_limit_mb=128,
    ),
    ScenarioDef(
        scenario_id="Q10",
        name="events_spill_aggregate_64mb",
        milestone_focus="M8",
        variant_kind="base",
        parameter_pack_id="agg_hot",
        sql=(
            "SELECT user_id, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "GROUP BY user_id "
            "ORDER BY user_id"
        ),
        param_keys=("tenant_id",),
        scenario_sets=("all", "m8"),
        memory_limit_mb=64,
    ),
    ScenarioDef(
        scenario_id="Q10",
        name="events_spill_aggregate_128mb",
        milestone_focus="M8",
        variant_kind="base",
        parameter_pack_id="agg_hot",
        sql=(
            "SELECT user_id, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "GROUP BY user_id "
            "ORDER BY user_id"
        ),
        param_keys=("tenant_id",),
        scenario_sets=("all", "m8"),
        memory_limit_mb=128,
    ),
    ScenarioDef(
        scenario_id="Q10F",
        name="events_spill_aggregate_fn_64mb",
        milestone_focus="M8",
        variant_kind="function_companion",
        parameter_pack_id="agg_hot_fn",
        sql=(
            "SELECT user_id, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND lower(channel) = %s "
            "GROUP BY user_id "
            "ORDER BY user_id"
        ),
        param_keys=("tenant_id", "channel"),
        scenario_sets=("all", "m8"),
        memory_limit_mb=64,
    ),
    ScenarioDef(
        scenario_id="Q10F",
        name="events_spill_aggregate_fn_128mb",
        milestone_focus="M8",
        variant_kind="function_companion",
        parameter_pack_id="agg_hot_fn",
        sql=(
            "SELECT user_id, COUNT(*), SUM(amount_cents), AVG(score) "
            "FROM bench_events "
            "WHERE tenant_id = %s "
            "  AND lower(channel) = %s "
            "GROUP BY user_id "
            "ORDER BY user_id"
        ),
        param_keys=("tenant_id", "channel"),
        scenario_sets=("all", "m8"),
        memory_limit_mb=128,
    ),
]


def percentile(values: list[float], q: float) -> float:
    if not values:
        raise ValueError("percentile requires non-empty values")
    if len(values) == 1:
        return float(values[0])
    ordered = sorted(values)
    pos = (len(ordered) - 1) * q
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return float(ordered[lo])
    frac = pos - lo
    return float(ordered[lo] + (ordered[hi] - ordered[lo]) * frac)


def choose_representative_repeat(repeat_summaries: list[dict[str, Any]]) -> int:
    p50s = [float(item["p50_ms"]["value"]) for item in repeat_summaries]
    target = statistics.median(p50s)
    best_idx = 0
    best_dist = abs(p50s[0] - target)
    for idx in range(1, len(p50s)):
        dist = abs(p50s[idx] - target)
        if dist < best_dist:
            best_idx = idx
            best_dist = dist
    return best_idx


def splitmix64(value: int) -> int:
    z = (value + 0x9E3779B97F4A7C15) & 0xFFFFFFFFFFFFFFFF
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & 0xFFFFFFFFFFFFFFFF
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & 0xFFFFFFFFFFFFFFFF
    return z ^ (z >> 31)


def choose_coprime_step(n: int, seed: int) -> int:
    candidate = int(splitmix64(seed ^ n) % max(n - 1, 1)) + 1
    while math.gcd(candidate, n) != 1:
        candidate += 1
        if candidate >= n:
            candidate = 1
    return candidate


def permute_slot(row_index_zero_based: int, cardinality: int, seed: int) -> int:
    if cardinality <= 1:
        return 0
    step = choose_coprime_step(cardinality, seed)
    offset = int(splitmix64(seed ^ 0xA5A5A5A5A5A5A5A5) % cardinality)
    return (row_index_zero_based * step + offset) % cardinality


def metric(value: float | int | None, unit: str | None, source: str | None = None) -> dict[str, Any]:
    numeric_value: float | None
    if value is None:
        numeric_value = None
    else:
        numeric_value = float(value)
    return {
        "value": numeric_value,
        "unit": unit,
        "source": source,
    }


def get_git_sha() -> str:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return "unknown"


def get_cpu_model() -> str:
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        try:
            for line in cpuinfo.read_text(encoding="utf-8", errors="ignore").splitlines():
                if line.lower().startswith("model name"):
                    parts = line.split(":", 1)
                    if len(parts) == 2:
                        return parts[1].strip()
        except Exception:
            pass
    return platform.processor() or "unknown"


def redact_dsn(dsn: str) -> str:
    parts = urlsplit(dsn)
    netloc = parts.hostname or ""
    if parts.port is not None:
        netloc = f"{netloc}:{parts.port}"
    if parts.username:
        netloc = f"{parts.username}:***@{netloc}"
    return urlunsplit((parts.scheme, netloc, parts.path, parts.query, parts.fragment))


def parse_prom_metric_spec(raw: str) -> PromMetricSpec:
    if "=" not in raw:
        raise ValueError(f"invalid --capture-prom-metric spec: {raw}")
    alias, selector = raw.split("=", 1)
    alias = alias.strip()
    selector = selector.strip()
    if not alias or not selector:
        raise ValueError(f"invalid --capture-prom-metric spec: {raw}")

    if "{" not in selector:
        return PromMetricSpec(alias=alias, metric_name=selector, labels=())

    if not selector.endswith("}"):
        raise ValueError(f"invalid Prometheus selector: {selector}")
    metric_name, label_blob = selector.split("{", 1)
    metric_name = metric_name.strip()
    label_blob = label_blob[:-1]
    labels: list[tuple[str, str]] = []
    if label_blob.strip():
        matches = list(PROM_LABEL_RE.finditer(label_blob))
        if not matches:
            raise ValueError(f"invalid Prometheus selector labels: {selector}")
        for match in matches:
            value = bytes(match.group(2), "utf-8").decode("unicode_escape")
            labels.append((match.group(1), value))
    return PromMetricSpec(alias=alias, metric_name=metric_name, labels=tuple(sorted(labels)))


def parse_prometheus_text(payload: str) -> dict[tuple[str, tuple[tuple[str, str], ...]], float]:
    metrics: dict[tuple[str, tuple[tuple[str, str], ...]], float] = {}
    for line in payload.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        match = PROM_LINE_RE.match(line)
        if match is None:
            continue
        metric_name = match.group(1)
        label_blob = match.group(2)
        value = float(match.group(3))
        labels: list[tuple[str, str]] = []
        if label_blob:
            for label_match in PROM_LABEL_RE.finditer(label_blob):
                label_value = bytes(label_match.group(2), "utf-8").decode("unicode_escape")
                labels.append((label_match.group(1), label_value))
        metrics[(metric_name, tuple(sorted(labels)))] = value
    return metrics


def scrape_prometheus_metrics(url: str, timeout_seconds: float = 5.0) -> dict[tuple[str, tuple[tuple[str, str], ...]], float]:
    with urlopen(url, timeout=timeout_seconds) as response:
        body = response.read().decode("utf-8", errors="replace")
    return parse_prometheus_text(body)


def get_metric_snapshot_value(
    snapshot: dict[tuple[str, tuple[tuple[str, str], ...]], float],
    spec: PromMetricSpec,
) -> float | None:
    required = dict(spec.labels)
    matched_values: list[float] = []
    for (metric_name, labels), value in snapshot.items():
        if metric_name != spec.metric_name:
            continue
        label_dict = dict(labels)
        if all(label_dict.get(key) == expected for key, expected in required.items()):
            matched_values.append(value)
    if not matched_values:
        return None
    return sum(matched_values)


def metadata(engine_label: str, engine_version: str, dsn: str) -> dict[str, Any]:
    return {
        "engine_label": engine_label,
        "engine_version": engine_version,
        "git_sha": get_git_sha(),
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "hostname": platform.node(),
        "platform": platform.platform(),
        "harness_version": "benchmark_m1_baseline.py/v1",
        "dsn_redacted": redact_dsn(dsn),
        "cpu_model": get_cpu_model(),
        "cpu_count_logical": os.cpu_count(),
        "python": sys.version.replace("\n", " "),
    }


def get_connection(dsn: str, autocommit: bool = False) -> Any:
    try:
        import psycopg
    except ImportError:
        print("ERROR: psycopg (v3) is required. Install with: pip install psycopg", file=sys.stderr)
        raise SystemExit(2)
    return psycopg.connect(dsn, autocommit=autocommit)


def get_engine_version(conn: Any) -> str:
    cur = conn.cursor()
    try:
        cur.execute("SELECT version()")
        row = cur.fetchone()
        if row and row[0]:
            return str(row[0])
    except Exception:
        pass
    finally:
        cur.close()
    return "unknown"


def maybe_print_progress(prefix: str, current: int, total: int) -> None:
    if total <= 0:
        return
    if current == total or current % 50_000 == 0:
        pct = (current / total) * 100.0
        print(f"{prefix}: {current}/{total} ({pct:.1f}%)")


def deterministic_tokens(prefix: str, row_id: int, seed: int, count: int) -> list[str]:
    tokens = []
    state = splitmix64(seed ^ row_id)
    for idx in range(count):
        state = splitmix64(state ^ idx)
        tokens.append(f"{prefix}{int(state % 997):03d}")
    return tokens


def make_title(prefix: str, row_id: int, seed: int) -> str:
    tokens = deterministic_tokens(prefix, row_id, seed, 8)
    return " ".join(tokens)


def make_note(prefix: str, row_id: int, seed: int) -> str:
    tokens = deterministic_tokens(prefix, row_id, seed ^ 0x1234, 36)
    return " ".join(tokens)


def make_json_payload(prefix: str, row_id: int, tenant_id: int, seed: int, width_hint: int) -> str:
    tokens = deterministic_tokens(prefix, row_id, seed ^ 0x5678, width_hint)
    payload = {
        "tenant": tenant_id,
        "kind": prefix,
        "row_id": row_id,
        "flags": {
            "hot": tenant_id == 1,
            "bucket": int(splitmix64(seed ^ row_id) % 32),
        },
        "labels": tokens[: min(8, len(tokens))],
        "summary": " ".join(tokens),
    }
    return json.dumps(payload, sort_keys=True, separators=(",", ":"))


def pick_country(value: int) -> str | None:
    if value % 11 == 0:
        return None
    return COUNTRIES[value % len(COUNTRIES)]


def tenant_from_slot(slot: int, tenant1_rows: int, mid_rows: int) -> int:
    if slot < tenant1_rows:
        return 1
    if slot < tenant1_rows + mid_rows:
        return 2 + ((slot - tenant1_rows) % 9)
    return 11 + ((slot - tenant1_rows - mid_rows) % 990)


def create_tables(cur: Any) -> None:
    cur.execute("DROP TABLE IF EXISTS bench_orders")
    cur.execute("DROP TABLE IF EXISTS bench_events")
    cur.execute("DROP TABLE IF EXISTS bench_users")

    cur.execute(
        """
        CREATE TABLE bench_orders (
            order_id BIGINT PRIMARY KEY,
            tenant_id INT NOT NULL,
            status SMALLINT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL,
            created_day DATE NOT NULL,
            user_id BIGINT NOT NULL,
            channel TEXT NOT NULL,
            country TEXT,
            priority_bucket INT NOT NULL,
            amount_cents BIGINT NOT NULL,
            score DOUBLE PRECISION NOT NULL,
            title TEXT NOT NULL,
            note TEXT NOT NULL,
            attrs JSONB NOT NULL
        )
        """
    )
    cur.execute(
        """
        CREATE TABLE bench_events (
            event_id BIGINT PRIMARY KEY,
            tenant_id INT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL,
            created_day DATE NOT NULL,
            user_id BIGINT NOT NULL,
            event_type SMALLINT NOT NULL,
            channel TEXT NOT NULL,
            country TEXT,
            device_type TEXT NOT NULL,
            priority_bucket INT NOT NULL,
            amount_cents BIGINT NOT NULL,
            score DOUBLE PRECISION NOT NULL,
            body TEXT NOT NULL,
            attrs JSONB NOT NULL
        )
        """
    )
    cur.execute(
        """
        CREATE TABLE bench_users (
            user_id BIGINT PRIMARY KEY,
            tenant_id INT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL,
            status SMALLINT NOT NULL,
            region TEXT NOT NULL,
            email TEXT NOT NULL,
            plan_code TEXT NOT NULL,
            profile JSONB NOT NULL
        )
        """
    )


def create_indexes(cur: Any) -> None:
    cur.execute(
        "CREATE INDEX bo_tenant_status_created_idx "
        "ON bench_orders (tenant_id, status, created_at DESC, order_id DESC)"
    )
    cur.execute(
        "CREATE INDEX bo_tenant_status_lower_channel_created_idx "
        "ON bench_orders (tenant_id, status, lower(channel), created_at DESC, order_id DESC)"
    )
    cur.execute(
        "CREATE INDEX bo_tenant_status_abs_priority_created_idx "
        "ON bench_orders (tenant_id, status, abs(priority_bucket), created_at DESC, order_id DESC)"
    )

    cur.execute(
        "CREATE INDEX be_tenant_created_idx "
        "ON bench_events (tenant_id, created_at DESC, event_id DESC)"
    )
    cur.execute(
        "CREATE INDEX be_tenant_lower_channel_created_idx "
        "ON bench_events (tenant_id, lower(channel), created_at DESC, event_id DESC)"
    )
    cur.execute(
        "CREATE INDEX be_tenant_created_day_country_device_idx "
        "ON bench_events (tenant_id, created_day, country, device_type, event_id)"
    )
    cur.execute(
        "CREATE INDEX be_tenant_lower_channel_created_day_country_device_idx "
        "ON bench_events (tenant_id, lower(channel), created_day, country, device_type, event_id)"
    )
    cur.execute(
        "CREATE INDEX be_tenant_abs_priority_created_idx "
        "ON bench_events (tenant_id, abs(priority_bucket), created_at DESC, event_id DESC)"
    )

    cur.execute("CREATE INDEX bu_tenant_user_idx ON bench_users (tenant_id, user_id)")
    cur.execute("CREATE INDEX bu_tenant_lower_email_idx ON bench_users (tenant_id, lower(email))")


def make_user_row(row_id: int, config: ScaleConfig, seed: int) -> tuple[Any, ...]:
    total = config.users
    slot = permute_slot(row_id - 1, total, seed ^ 0x11)
    tenant1_rows = total // 5
    mid_rows = (total * 35) // 100
    tenant_id = tenant_from_slot(slot, tenant1_rows, mid_rows)
    mix = splitmix64(seed ^ row_id ^ 0x1010)
    created_at = BASE_TS + dt.timedelta(seconds=row_id * 7)
    email_local = f"User{row_id:07d}.Tenant{tenant_id}"
    email = f"{email_local}@example.com"
    status = 1 + int(mix % 4)
    region = REGIONS[int((mix >> 8) % len(REGIONS))]
    plan_code = PLAN_CODES[int((mix >> 16) % len(PLAN_CODES))]
    profile = json.dumps(
        {
            "kind": "user",
            "tenant": tenant_id,
            "region": region,
            "plan": plan_code,
            "labels": deterministic_tokens("usr", row_id, seed, 6),
        },
        sort_keys=True,
        separators=(",", ":"),
    )
    return (
        row_id,
        tenant_id,
        created_at,
        status,
        region,
        email,
        plan_code,
        profile,
    )


def make_order_row(row_id: int, config: ScaleConfig, seed: int) -> tuple[Any, ...]:
    total = config.orders
    slot = permute_slot(row_id - 1, total, seed ^ 0x22)
    tenant1_rows = total // 4
    mid_rows = (total * 35) // 100
    tenant_id = tenant_from_slot(slot, tenant1_rows, mid_rows)
    mix = splitmix64(seed ^ row_id ^ 0x2020)

    if tenant_id == 1:
        local = slot
        if local < total // 20:
            status = 2
            if local < (total * 3) // 100:
                channel = "push"
            else:
                channel = NON_PUSH_CHANNELS[int((mix >> 8) % len(NON_PUSH_CHANNELS))]
        else:
            status = [1, 3, 4, 5][int((mix >> 16) % 4)]
            channel = CHANNELS[int((mix >> 24) % len(CHANNELS))]
    else:
        status = ORDER_STATUSES[int(mix % len(ORDER_STATUSES))]
        channel = CHANNELS[int((mix >> 8) % len(CHANNELS))]

    created_at = BASE_TS + dt.timedelta(seconds=row_id)
    created_day = created_at.date()
    user_id = ((tenant_id * 100_003 + row_id * 17) % config.users) + 1
    country = pick_country(int(mix >> 16))
    priority_bucket = int(mix % 21) - 10
    amount_cents = 100 + int((mix >> 20) % 500_000)
    score = float(((mix >> 32) % 100_000) / 100.0)
    title = make_title("ord", row_id, seed)
    note = make_note("note", row_id, seed)
    attrs = make_json_payload("order", row_id, tenant_id, seed, 24)
    return (
        row_id,
        tenant_id,
        status,
        created_at,
        created_day,
        user_id,
        channel,
        country,
        priority_bucket,
        amount_cents,
        score,
        title,
        note,
        attrs,
    )


def make_event_row(row_id: int, config: ScaleConfig, seed: int) -> tuple[Any, ...]:
    total = config.events
    slot = permute_slot(row_id - 1, total, seed ^ 0x33)
    tenant1_rows = total // 5
    mid_rows = (total * 35) // 100
    tenant_id = tenant_from_slot(slot, tenant1_rows, mid_rows)
    mix = splitmix64(seed ^ row_id ^ 0x3030)
    created_at = BASE_TS + dt.timedelta(seconds=row_id)
    created_day = created_at.date()
    user_id = ((tenant_id * 200_003 + row_id * 13) % config.users) + 1
    event_type = EVENT_TYPES[int(mix % len(EVENT_TYPES))]
    channel = CHANNELS[int((mix >> 8) % len(CHANNELS))]
    country = pick_country(int(mix >> 16))
    device_type = DEVICE_TYPES[int((mix >> 24) % len(DEVICE_TYPES))]
    priority_bucket = int((mix >> 32) % 21) - 10
    amount_cents = 10 + int((mix >> 40) % 50_000)
    score = float(((mix >> 12) % 100_000) / 100.0)
    body = make_note("evt", row_id, seed)[:191]
    attrs = make_json_payload("event", row_id, tenant_id, seed, 12)
    return (
        row_id,
        tenant_id,
        created_at,
        created_day,
        user_id,
        event_type,
        channel,
        country,
        device_type,
        priority_bucket,
        amount_cents,
        score,
        body,
        attrs,
    )


def insert_users(cur: Any, config: ScaleConfig, seed: int, batch_size: int) -> None:
    sql = (
        "INSERT INTO bench_users "
        "(user_id, tenant_id, created_at, status, region, email, plan_code, profile) "
        "VALUES (%s, %s, %s, %s, %s, %s, %s, %s::jsonb)"
    )
    batch: list[tuple[Any, ...]] = []
    total = config.users
    for row_id in range(1, total + 1):
        batch.append(make_user_row(row_id, config, seed))
        if len(batch) >= batch_size:
            cur.executemany(sql, batch)
            batch.clear()
            maybe_print_progress("bench_users", row_id, total)
    if batch:
        cur.executemany(sql, batch)
    maybe_print_progress("bench_users", total, total)


def insert_orders(cur: Any, config: ScaleConfig, seed: int, batch_size: int) -> None:
    sql = (
        "INSERT INTO bench_orders "
        "(order_id, tenant_id, status, created_at, created_day, user_id, channel, country, "
        " priority_bucket, amount_cents, score, title, note, attrs) "
        "VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb)"
    )
    batch: list[tuple[Any, ...]] = []
    total = config.orders
    for row_id in range(1, total + 1):
        batch.append(make_order_row(row_id, config, seed))
        if len(batch) >= batch_size:
            cur.executemany(sql, batch)
            batch.clear()
            maybe_print_progress("bench_orders", row_id, total)
    if batch:
        cur.executemany(sql, batch)
    maybe_print_progress("bench_orders", total, total)


def insert_events(cur: Any, config: ScaleConfig, seed: int, batch_size: int) -> None:
    sql = (
        "INSERT INTO bench_events "
        "(event_id, tenant_id, created_at, created_day, user_id, event_type, channel, country, "
        " device_type, priority_bucket, amount_cents, score, body, attrs) "
        "VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb)"
    )
    batch: list[tuple[Any, ...]] = []
    total = config.events
    for row_id in range(1, total + 1):
        batch.append(make_event_row(row_id, config, seed))
        if len(batch) >= batch_size:
            cur.executemany(sql, batch)
            batch.clear()
            maybe_print_progress("bench_events", row_id, total)
    if batch:
        cur.executemany(sql, batch)
    maybe_print_progress("bench_events", total, total)


def analyze_tables(cur: Any) -> None:
    cur.execute("ANALYZE bench_users")
    cur.execute("ANALYZE bench_orders")
    cur.execute("ANALYZE bench_events")


def fetch_scalar(cur: Any, sql: str, params: tuple[Any, ...] = ()) -> Any:
    cur.execute(sql, params)
    row = cur.fetchone()
    return row[0] if row else None


def sample_checksum(cur: Any, table_name: str, sql: str) -> str:
    cur.execute(sql)
    rows = cur.fetchall()
    payload = json.dumps(rows, default=str, sort_keys=True)
    return hashlib.md5(payload.encode("utf-8")).hexdigest()


def build_dataset_section(cur: Any, config: ScaleConfig, seed: int) -> dict[str, Any]:
    orders_count = int(fetch_scalar(cur, "SELECT COUNT(*) FROM bench_orders"))
    events_count = int(fetch_scalar(cur, "SELECT COUNT(*) FROM bench_events"))
    users_count = int(fetch_scalar(cur, "SELECT COUNT(*) FROM bench_users"))
    hot_count = int(
        fetch_scalar(
            cur,
            "SELECT COUNT(*) FROM bench_orders WHERE tenant_id = %s AND status = %s",
            (1, 2),
        )
    )
    hot_fn_count = int(
        fetch_scalar(
            cur,
            "SELECT COUNT(*) FROM bench_orders WHERE tenant_id = %s AND status = %s AND lower(channel) = %s",
            (1, 2, "push"),
        )
    )

    tables = [
        {
            "table_name": "bench_users",
            "row_count": users_count,
            "checksum": sample_checksum(
                cur,
                "bench_users",
                "SELECT user_id, tenant_id, email FROM bench_users ORDER BY user_id LIMIT 8",
            ),
        },
        {
            "table_name": "bench_orders",
            "row_count": orders_count,
            "checksum": sample_checksum(
                cur,
                "bench_orders",
                "SELECT order_id, tenant_id, status, channel FROM bench_orders ORDER BY order_id LIMIT 8",
            ),
        },
        {
            "table_name": "bench_events",
            "row_count": events_count,
            "checksum": sample_checksum(
                cur,
                "bench_events",
                "SELECT event_id, tenant_id, event_type, channel FROM bench_events ORDER BY event_id LIMIT 8",
            ),
        },
    ]

    validation_checks = [
        {
            "id": "bench_orders_count",
            "sql": "SELECT COUNT(*) FROM bench_orders",
            "value": orders_count,
        },
        {
            "id": "bench_events_count",
            "sql": "SELECT COUNT(*) FROM bench_events",
            "value": events_count,
        },
        {
            "id": "bench_users_count",
            "sql": "SELECT COUNT(*) FROM bench_users",
            "value": users_count,
        },
        {
            "id": "hot_orders_count",
            "sql": "SELECT COUNT(*) FROM bench_orders WHERE tenant_id = 1 AND status = 2",
            "value": hot_count,
        },
        {
            "id": "hot_orders_fn_count",
            "sql": "SELECT COUNT(*) FROM bench_orders WHERE tenant_id = 1 AND status = 2 AND lower(channel) = 'push'",
            "value": hot_fn_count,
        },
    ]

    return {
        "dataset_id": f"m1_baseline_{config.label}_{seed}",
        "scale": config.label,
        "seed": seed,
        "tables": tables,
        "validation_checks": validation_checks,
    }


def fixed_parameter_pack(pack_id: str) -> dict[str, Any] | None:
    fixed: dict[str, dict[str, Any]] = {
        "hot_page": {"tenant_id": 1, "status": 2},
        "hot_page_fn": {"tenant_id": 1, "status": 2, "channel": "push"},
        "hot_offset": {"tenant_id": 1, "status": 2, "offset": 10_000},
        "hot_offset_fn": {"tenant_id": 1, "status": 2, "channel": "push", "offset": 10_000},
        "events_hot": {"tenant_id": 1},
        "events_hot_fn": {"tenant_id": 1, "priority_abs": 7},
        "agg_hot": {"tenant_id": 1},
        "agg_hot_fn": {"tenant_id": 1, "channel": "push"},
        "events_range_hot": {
            "tenant_id": 1,
            "day_start": dt.date(2024, 2, 1),
            "day_end": dt.date(2024, 4, 30),
        },
        "events_range_hot_fn": {
            "tenant_id": 1,
            "day_start": dt.date(2024, 2, 1),
            "day_end": dt.date(2024, 4, 30),
            "channel": "push",
        },
    }
    return fixed.get(pack_id)


def resolve_orm_hot(cur: Any) -> dict[str, Any]:
    cur.execute(
        "SELECT tenant_id, user_id, lower(email) "
        "FROM bench_users "
        "WHERE tenant_id = 1 "
        "ORDER BY user_id "
        "LIMIT 1 OFFSET 123"
    )
    row = cur.fetchone()
    if row is None:
        raise RuntimeError("bench_users does not contain a hot tenant row for orm_hot")
    return {"tenant_id": int(row[0]), "user_id": int(row[1]), "email": str(row[2])}


def resolve_orm_mixed_sequence(cur: Any, needed: int) -> list[dict[str, Any]]:
    cur.execute(
        "SELECT tenant_id, user_id, lower(email) "
        "FROM bench_users "
        "WHERE tenant_id = 1 "
        "ORDER BY user_id "
        "LIMIT 16"
    )
    hot_rows = cur.fetchall()
    cur.execute(
        "SELECT tenant_id, user_id, lower(email) "
        "FROM bench_users "
        "WHERE tenant_id >= 11 "
        "ORDER BY tenant_id, user_id "
        "LIMIT 16"
    )
    cold_rows = cur.fetchall()
    if not hot_rows or not cold_rows:
        raise RuntimeError("bench_users does not contain enough rows for orm_mixed")

    merged: list[dict[str, Any]] = []
    for idx in range(max(len(hot_rows), len(cold_rows))):
        if idx < len(hot_rows):
            merged.append(
                {
                    "tenant_id": int(hot_rows[idx][0]),
                    "user_id": int(hot_rows[idx][1]),
                    "email": str(hot_rows[idx][2]),
                }
            )
        if idx < len(cold_rows):
            merged.append(
                {
                    "tenant_id": int(cold_rows[idx][0]),
                    "user_id": int(cold_rows[idx][1]),
                    "email": str(cold_rows[idx][2]),
                }
            )
    if not merged:
        raise RuntimeError("orm_mixed parameter sequence is empty")
    return [merged[idx % len(merged)] for idx in range(needed)]


def resolve_parameter_sequence(cur: Any, scenario: ScenarioDef, count: int) -> list[tuple[Any, ...]]:
    fixed = fixed_parameter_pack(scenario.parameter_pack_id)
    if fixed is not None:
        params = tuple(fixed[key] for key in scenario.param_keys)
        return [params for _ in range(count)]

    if scenario.parameter_pack_id == "orm_hot":
        resolved = resolve_orm_hot(cur)
        params = tuple(resolved[key] for key in scenario.param_keys)
        return [params for _ in range(count)]

    if scenario.parameter_pack_id == "orm_mixed":
        sequence = resolve_orm_mixed_sequence(cur, count)
        return [tuple(item[key] for key in scenario.param_keys) for item in sequence]

    raise ValueError(f"unknown parameter pack: {scenario.parameter_pack_id}")


def explain_query(cur: Any, sql: str, params: tuple[Any, ...]) -> list[str]:
    cur.execute(f"EXPLAIN {sql}", params)
    return [str(row[0]) for row in cur.fetchall()]


def summarize_elapsed_and_first_row(elapsed_ms: list[float], first_row_ms: list[float]) -> dict[str, Any]:
    return {
        "count": len(elapsed_ms),
        "elapsed_ms": metric(statistics.fmean(elapsed_ms), "ms", "client_timer"),
        "p50_ms": metric(percentile(elapsed_ms, 0.50), "ms", "client_timer"),
        "p95_ms": metric(percentile(elapsed_ms, 0.95), "ms", "client_timer"),
        "first_row_latency_ms": metric(statistics.fmean(first_row_ms), "ms", "client_timer"),
        "peak_rss_bytes": metric(None, "bytes", None),
        "cpu_time_ms": metric(None, "ms", None),
    }


def tukey_outlier_count(values: list[float]) -> int:
    if len(values) < 4:
        return 0
    q1 = percentile(values, 0.25)
    q3 = percentile(values, 0.75)
    iqr = q3 - q1
    if iqr <= 0:
        return 0
    lower = q1 - (1.5 * iqr)
    upper = q3 + (1.5 * iqr)
    return sum(1 for value in values if value < lower or value > upper)


def confidence_interval_95(values: list[float]) -> tuple[float, float, float]:
    mean = statistics.fmean(values)
    if len(values) < 2:
        return mean, mean, 0.0
    stdev = statistics.stdev(values)
    margin = 1.96 * stdev / math.sqrt(len(values))
    return mean - margin, mean + margin, stdev


def aggregate_repeat_results(repeat_results: list[dict[str, Any]]) -> dict[str, Any]:
    elapsed_samples = [
        float(sample)
        for repeat in repeat_results
        for sample in repeat["measurements"]["elapsed_ms"]
    ]
    first_row_samples = [
        float(sample)
        for repeat in repeat_results
        for sample in repeat["measurements"]["first_row_latency_ms"]
    ]
    ci_low, ci_high, stdev = confidence_interval_95(elapsed_samples)
    return {
        "repeat_count": len(repeat_results),
        "outlier_rule": "tukey_iqr_1_5x",
        "summary": {
            "count": len(elapsed_samples),
            "elapsed_ms": metric(statistics.fmean(elapsed_samples), "ms", "client_timer"),
            "p50_ms": metric(percentile(elapsed_samples, 0.50), "ms", "client_timer"),
            "p95_ms": metric(percentile(elapsed_samples, 0.95), "ms", "client_timer"),
            "first_row_latency_ms": metric(statistics.fmean(first_row_samples), "ms", "client_timer"),
            "elapsed_stdev_ms": metric(stdev, "ms", "client_timer"),
            "elapsed_ci95_low_ms": metric(ci_low, "ms", "client_timer"),
            "elapsed_ci95_high_ms": metric(ci_high, "ms", "client_timer"),
            "elapsed_outlier_count": metric(tukey_outlier_count(elapsed_samples), "count", "tukey_iqr_1_5x"),
        },
    }


def null_counters() -> dict[str, Any]:
    return {
        "scanned_rows": metric(None, "rows", None),
        "decoded_rows": metric(None, "rows", None),
        "fetched_base_rows": metric(None, "rows", None),
        "rows_scanned_before_stop": metric(None, "rows", None),
        "payload_bytes": metric(None, "bytes", None),
        "kv_requests": metric(None, "count", None),
        "sort_bytes": metric(None, "bytes", None),
        "aggregate_memory_bytes": metric(None, "bytes", None),
        "spill_bytes": metric(None, "bytes", None),
        "spill_passes": metric(None, "count", None),
        "plan_cache_hit_ratio": metric(None, "ratio", None),
        "planning_cpu_ms": metric(None, "ms", None),
        "cleanup_ok": None,
    }


CANONICAL_METRIC_UNITS: dict[str, str] = {
    "scanned_rows": "rows",
    "decoded_rows": "rows",
    "fetched_base_rows": "rows",
    "rows_scanned_before_stop": "rows",
    "payload_bytes": "bytes",
    "kv_requests": "count",
    "sort_bytes": "bytes",
    "aggregate_memory_bytes": "bytes",
    "spill_bytes": "bytes",
    "spill_passes": "count",
    "plan_cache_hit_ratio": "ratio",
    "planning_cpu_ms": "ms",
}


def apply_prometheus_deltas(
    counters: dict[str, Any],
    extra_metrics: dict[str, Any],
    specs: list[PromMetricSpec],
    before_snapshot: dict[tuple[str, tuple[tuple[str, str], ...]], float] | None,
    after_snapshot: dict[tuple[str, tuple[tuple[str, str], ...]], float] | None,
) -> None:
    if before_snapshot is None or after_snapshot is None:
        return

    for spec in specs:
        before_value = get_metric_snapshot_value(before_snapshot, spec)
        after_value = get_metric_snapshot_value(after_snapshot, spec)
        if before_value is None and after_value is None:
            delta = None
        else:
            delta = (after_value or 0.0) - (before_value or 0.0)
        source = f"prometheus:{spec.metric_name}"
        if spec.labels:
            source += "{" + ",".join(f'{k}=\"{v}\"' for k, v in spec.labels) + "}"
        if spec.alias in counters:
            unit = CANONICAL_METRIC_UNITS.get(spec.alias)
            counters[spec.alias] = metric(delta, unit, source)
        else:
            extra_metrics[spec.alias] = metric(delta, None, source)


def apply_session_settings(cur: Any, args: argparse.Namespace, scenario: ScenarioDef) -> dict[str, Any]:
    applied: dict[str, Any] = {}
    for item in args.set:
        if "=" not in item:
            raise ValueError(f"invalid --set item: {item}")
        key, value = item.split("=", 1)
        cur.execute(f"SET {key} = {value}")
        applied[key] = value
    if scenario.worker_count is not None and args.parallel_setting_template:
        stmt = args.parallel_setting_template.format(workers=scenario.worker_count)
        cur.execute(stmt)
        applied["parallel_template"] = stmt
    if scenario.memory_limit_mb is not None and args.memory_setting_template:
        stmt = args.memory_setting_template.format(mb=scenario.memory_limit_mb)
        cur.execute(stmt)
        applied["memory_template"] = stmt
    return applied


def run_one_iteration(cur: Any, scenario: ScenarioDef, params: tuple[Any, ...]) -> tuple[float, float]:
    start_ns = time.perf_counter_ns()
    cur.execute(scenario.sql, params)
    first_row_elapsed_ms: float
    if cur.description is None:
        first_row_elapsed_ms = (time.perf_counter_ns() - start_ns) / 1_000_000.0
    else:
        first = cur.fetchone()
        first_row_elapsed_ms = (time.perf_counter_ns() - start_ns) / 1_000_000.0
        consumed = 0
        if first is not None:
            consumed = 1
        if scenario.consume_rows is None:
            while first is not None:
                first = cur.fetchone()
        else:
            while first is not None and consumed < scenario.consume_rows:
                first = cur.fetchone()
                if first is not None:
                    consumed += 1
    elapsed_ms = (time.perf_counter_ns() - start_ns) / 1_000_000.0
    return elapsed_ms, first_row_elapsed_ms


def scenario_key(scenario: dict[str, Any]) -> str:
    return json.dumps(
        {
            "scenario_id": scenario["scenario_id"],
            "variant_kind": scenario["variant_kind"],
            "parameter_pack_id": scenario["parameter_pack_id"],
            "submode": scenario.get("submode"),
            "worker_count": scenario.get("worker_count"),
            "memory_limit_mb": scenario.get("memory_limit_mb"),
        },
        sort_keys=True,
    )


def get_scenarios_for_set(scenario_set: str) -> list[ScenarioDef]:
    if scenario_set not in {"all", "m2", "m2_m4", "m5", "m6", "m7", "m8"}:
        raise ValueError(f"unknown scenario set: {scenario_set}")
    selected = [scenario for scenario in SCENARIOS if scenario_set in scenario.scenario_sets]
    if not selected:
        raise ValueError(f"no scenarios found for set: {scenario_set}")
    return selected


def run_scenario(
    dsn: str,
    args: argparse.Namespace,
    scenario: ScenarioDef,
    repeats: int,
    warmup: int,
    measured: int,
) -> dict[str, Any]:
    conn = get_connection(dsn, autocommit=False)
    try:
        cur = conn.cursor()
        applied_settings = apply_session_settings(cur, args, scenario)
        total_iterations = warmup + measured
        param_sequence = resolve_parameter_sequence(cur, scenario, total_iterations * repeats)
        prom_specs = [parse_prom_metric_spec(raw) for raw in args.capture_prom_metric]

        explain_lines: list[str] | None = None
        if scenario.include_explain:
            explain_lines = explain_query(cur, scenario.sql, param_sequence[0])

        repeat_results: list[dict[str, Any]] = []
        sequence_offset = 0
        for repeat_no in range(1, repeats + 1):
            for _ in range(warmup):
                iter_cur = conn.cursor()
                run_one_iteration(iter_cur, scenario, param_sequence[sequence_offset])
                iter_cur.close()
                sequence_offset += 1

            before_prom = (
                scrape_prometheus_metrics(args.metrics_url, timeout_seconds=args.metrics_timeout_seconds)
                if args.metrics_url and prom_specs
                else None
            )
            elapsed_ms_samples: list[float] = []
            first_row_samples: list[float] = []
            for _ in range(measured):
                iter_cur = conn.cursor()
                elapsed_ms, first_row_ms = run_one_iteration(
                    iter_cur,
                    scenario,
                    param_sequence[sequence_offset],
                )
                iter_cur.close()
                sequence_offset += 1
                elapsed_ms_samples.append(elapsed_ms)
                first_row_samples.append(first_row_ms)

            summary = summarize_elapsed_and_first_row(elapsed_ms_samples, first_row_samples)
            counters = null_counters()
            extra_metrics: dict[str, Any] = {}
            after_prom = (
                scrape_prometheus_metrics(args.metrics_url, timeout_seconds=args.metrics_timeout_seconds)
                if args.metrics_url and prom_specs
                else None
            )
            apply_prometheus_deltas(counters, extra_metrics, prom_specs, before_prom, after_prom)
            repeat_results.append(
                {
                    "repeat": repeat_no,
                    "summary": summary,
                    "counters": counters,
                    "extra_metrics": extra_metrics,
                    "measurements": {
                        "elapsed_ms": elapsed_ms_samples,
                        "first_row_latency_ms": first_row_samples,
                    },
                }
            )
            print(
                f"PASS [{scenario.scenario_id}:{scenario.name}] repeat={repeat_no} "
                f"p50={summary['p50_ms']['value']:.3f}ms "
                f"p95={summary['p95_ms']['value']:.3f}ms"
            )

        rep_idx = choose_representative_repeat([item["summary"] for item in repeat_results])
        first_params = list(param_sequence[0])
        return {
            "scenario_id": scenario.scenario_id,
            "milestone_focus": scenario.milestone_focus,
            "variant_kind": scenario.variant_kind,
            "parameter_pack_id": scenario.parameter_pack_id,
            "sql": scenario.sql,
            "params": first_params,
            "execution_mode": scenario.execution_mode,
            "submode": scenario.submode,
            "worker_count": scenario.worker_count,
            "memory_limit_mb": scenario.memory_limit_mb,
            "explain": explain_lines,
            "repeats": repeat_results,
            "aggregate": aggregate_repeat_results(repeat_results),
            "representative_repeat": repeat_results[rep_idx]["repeat"],
            "applied_settings": applied_settings,
        }
    finally:
        conn.rollback()
        conn.close()


def write_json(path: str | None, payload: dict[str, Any]) -> None:
    encoded = json.dumps(payload, indent=2, sort_keys=True, default=str)
    if path:
        output_path = Path(path)
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(encoded, encoding="utf-8")
        print(f"Wrote JSON: {output_path}")
    else:
        print(encoded)


def benchmark_gen(args: argparse.Namespace) -> int:
    config = SCALES[args.scale]
    conn = get_connection(args.dsn, autocommit=False)
    try:
        cur = conn.cursor()
        started = time.monotonic()
        create_tables(cur)
        conn.commit()

        insert_users(cur, config, args.seed ^ 0x1111, args.batch_size)
        conn.commit()
        insert_orders(cur, config, args.seed ^ 0x2222, args.batch_size)
        conn.commit()
        insert_events(cur, config, args.seed ^ 0x3333, args.batch_size)
        conn.commit()

        create_indexes(cur)
        conn.commit()

        if args.analyze:
            analyze_tables(cur)
            conn.commit()

        engine_version = get_engine_version(conn)
        dataset = build_dataset_section(cur, config, args.seed)
        payload = {
            "schema_version": SCHEMA_VERSION,
            "meta": metadata(args.engine_label, engine_version, args.dsn),
            "dataset": dataset,
            "run_config": {
                "scenario_set": "gen_only",
                "warmup": 0,
                "measured": 0,
                "repeats": 1,
                "protocol_mode": None,
                "server_settings": {
                    "batch_size": args.batch_size,
                    "analyze": args.analyze,
                    "duration_seconds": time.monotonic() - started,
                },
            },
            "scenarios": [],
        }
        write_json(args.output, payload)
        return 0
    finally:
        conn.close()


def benchmark_run(args: argparse.Namespace) -> int:
    if args.capture_prom_metric and not args.metrics_url:
        raise ValueError("--capture-prom-metric requires --metrics-url")
    config = SCALES[args.scale]
    scenarios = get_scenarios_for_set(args.scenario_set)
    baseline_conn = get_connection(args.dsn, autocommit=False)
    try:
        engine_version = get_engine_version(baseline_conn)
        cur = baseline_conn.cursor()
        dataset = build_dataset_section(cur, config, args.seed)
        cur.close()
    finally:
        baseline_conn.close()

    started = time.monotonic()
    scenario_results = []
    for scenario in scenarios:
        scenario_results.append(
            run_scenario(
                args.dsn,
                args,
                scenario,
                repeats=args.repeats,
                warmup=args.warmup,
                measured=args.measured,
            )
        )

    payload = {
        "schema_version": SCHEMA_VERSION,
        "meta": metadata(args.engine_label, engine_version, args.dsn),
        "dataset": dataset,
        "run_config": {
            "scenario_set": args.scenario_set,
            "warmup": args.warmup,
            "measured": args.measured,
            "repeats": args.repeats,
            "protocol_mode": None,
            "server_settings": {
                "duration_seconds": time.monotonic() - started,
                "explicit_set": args.set,
                "parallel_setting_template": args.parallel_setting_template,
                "memory_setting_template": args.memory_setting_template,
                "metrics_url": args.metrics_url,
                "metrics_timeout_seconds": args.metrics_timeout_seconds,
                "capture_prom_metric": args.capture_prom_metric,
            },
        },
        "scenarios": scenario_results,
    }
    write_json(args.output, payload)
    return 0


def load_json(path: str) -> dict[str, Any]:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def compare_metric_values(
    before_value: float | None,
    after_value: float | None,
    postgres_value: float | None,
    better: str,
    unit: str | None,
) -> dict[str, Any]:
    def delta(base: float | None, current: float | None) -> dict[str, Any] | None:
        if base is None or current is None:
            return None
        abs_delta = current - base
        pct_delta = 0.0 if base == 0 else ((current - base) / base) * 100.0
        return {"absolute": abs_delta, "percent": pct_delta}

    return {
        "before": before_value,
        "after": after_value,
        "postgres_18_3": postgres_value,
        "better": better,
        "unit": unit,
        "after_vs_before": delta(before_value, after_value),
        "after_vs_postgres_18_3": delta(postgres_value, after_value),
    }


def representative_repeat(scenario: dict[str, Any]) -> dict[str, Any]:
    rep_no = int(scenario["representative_repeat"])
    for item in scenario["repeats"]:
        if int(item["repeat"]) == rep_no:
            return item
    raise KeyError(f"representative repeat {rep_no} not found")


def summary_for_compare(scenario: dict[str, Any]) -> dict[str, Any]:
    aggregate = scenario.get("aggregate")
    if isinstance(aggregate, dict) and isinstance(aggregate.get("summary"), dict):
        return aggregate["summary"]
    return representative_repeat(scenario)["summary"]


def compare_scenarios(before: dict[str, Any], after: dict[str, Any], postgres: dict[str, Any]) -> list[dict[str, Any]]:
    def index(raw: dict[str, Any]) -> dict[str, dict[str, Any]]:
        return {scenario_key(item): item for item in raw["scenarios"]}

    before_idx = index(before)
    after_idx = index(after)
    postgres_idx = index(postgres)

    keys = sorted(set(before_idx) & set(after_idx) & set(postgres_idx))
    if not keys:
        raise ValueError("no overlapping scenario keys across before/after/postgres inputs")

    preferred_direction = {
        "elapsed_ms": "lower",
        "p50_ms": "lower",
        "p95_ms": "lower",
        "first_row_latency_ms": "lower",
        "peak_rss_bytes": "lower",
        "cpu_time_ms": "lower",
        "scanned_rows": "lower",
        "decoded_rows": "lower",
        "fetched_base_rows": "lower",
        "rows_scanned_before_stop": "lower",
        "payload_bytes": "lower",
        "kv_requests": "lower",
        "sort_bytes": "lower",
        "aggregate_memory_bytes": "lower",
        "spill_bytes": "lower",
        "spill_passes": "lower",
        "plan_cache_hit_ratio": "higher",
        "planning_cpu_ms": "lower",
    }

    rows: list[dict[str, Any]] = []
    for key in keys:
        before_s = before_idx[key]
        after_s = after_idx[key]
        postgres_s = postgres_idx[key]

        before_rep = representative_repeat(before_s)
        after_rep = representative_repeat(after_s)
        postgres_rep = representative_repeat(postgres_s)
        before_summary = summary_for_compare(before_s)
        after_summary = summary_for_compare(after_s)
        postgres_summary = summary_for_compare(postgres_s)

        metric_rows: dict[str, Any] = {}
        summary_keys = set(before_summary.keys()) | set(after_summary.keys()) | set(postgres_summary.keys())
        counter_keys = set(before_rep["counters"].keys()) | set(after_rep["counters"].keys()) | set(postgres_rep["counters"].keys())

        for metric_name in sorted(summary_keys):
            if metric_name == "count":
                continue
            before_metric = before_summary.get(metric_name)
            after_metric = after_summary.get(metric_name)
            postgres_metric = postgres_summary.get(metric_name)
            before_value = before_metric["value"] if before_metric is not None else None
            after_value = after_metric["value"] if after_metric is not None else None
            postgres_value = postgres_metric["value"] if postgres_metric is not None else None
            unit = (
                before_metric.get("unit")
                if before_metric is not None
                else after_metric.get("unit")
                if after_metric is not None
                else postgres_metric.get("unit")
                if postgres_metric is not None
                else None
            )
            metric_rows[metric_name] = compare_metric_values(
                before_value,
                after_value,
                postgres_value,
                preferred_direction.get(metric_name, "lower"),
                unit,
            )

        for metric_name in sorted(counter_keys):
            before_metric = before_rep["counters"].get(metric_name)
            after_metric = after_rep["counters"].get(metric_name)
            postgres_metric = postgres_rep["counters"].get(metric_name)

            if metric_name == "cleanup_ok":
                metric_rows[metric_name] = {
                    "before": before_metric,
                    "after": after_metric,
                    "postgres_18_3": postgres_metric,
                }
                continue

            before_value = before_metric["value"] if before_metric is not None else None
            after_value = after_metric["value"] if after_metric is not None else None
            postgres_value = postgres_metric["value"] if postgres_metric is not None else None
            unit = (
                before_metric.get("unit")
                if before_metric is not None
                else after_metric.get("unit")
                if after_metric is not None
                else postgres_metric.get("unit")
                if postgres_metric is not None
                else None
            )
            metric_rows[metric_name] = compare_metric_values(
                before_value,
                after_value,
                postgres_value,
                preferred_direction.get(metric_name, "lower"),
                unit,
            )

        extra_metric_keys = (
            set(before_rep.get("extra_metrics", {}).keys())
            | set(after_rep.get("extra_metrics", {}).keys())
            | set(postgres_rep.get("extra_metrics", {}).keys())
        )
        for metric_name in sorted(extra_metric_keys):
            before_metric = before_rep.get("extra_metrics", {}).get(metric_name)
            after_metric = after_rep.get("extra_metrics", {}).get(metric_name)
            postgres_metric = postgres_rep.get("extra_metrics", {}).get(metric_name)
            before_value = before_metric["value"] if before_metric is not None else None
            after_value = after_metric["value"] if after_metric is not None else None
            postgres_value = postgres_metric["value"] if postgres_metric is not None else None
            unit = (
                before_metric.get("unit")
                if before_metric is not None
                else after_metric.get("unit")
                if after_metric is not None
                else postgres_metric.get("unit")
                if postgres_metric is not None
                else None
            )
            metric_rows[metric_name] = compare_metric_values(
                before_value,
                after_value,
                postgres_value,
                preferred_direction.get(metric_name, "lower"),
                unit,
            )

        rows.append(
            {
                "scenario_id": before_s["scenario_id"],
                "variant_kind": before_s["variant_kind"],
                "parameter_pack_id": before_s["parameter_pack_id"],
                "submode": before_s.get("submode"),
                "worker_count": before_s.get("worker_count"),
                "memory_limit_mb": before_s.get("memory_limit_mb"),
                "metrics": metric_rows,
                "summary_source": "aggregate" if "aggregate" in before_s else "representative_repeat",
                "postgres_reference_line": {
                    "engine_label": postgres["meta"]["engine_label"],
                    "engine_version": postgres["meta"]["engine_version"],
                },
            }
        )
    return rows


def validate_compatible_inputs(before: dict[str, Any], after: dict[str, Any], postgres: dict[str, Any]) -> None:
    def base_fields(raw: dict[str, Any]) -> tuple[Any, ...]:
        return (
            raw.get("schema_version"),
            raw.get("dataset", {}).get("scale"),
            raw.get("dataset", {}).get("seed"),
            raw.get("run_config", {}).get("scenario_set"),
        )

    if before.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("before input schema_version is not the expected raw schema")
    if after.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("after input schema_version is not the expected raw schema")
    if postgres.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("postgres input schema_version is not the expected raw schema")

    before_fields = base_fields(before)
    after_fields = base_fields(after)
    postgres_fields = base_fields(postgres)
    if before_fields != after_fields or before_fields != postgres_fields:
        raise ValueError("scale/seed/scenario_set mismatch across compare inputs")


def benchmark_compare(args: argparse.Namespace) -> int:
    before = load_json(args.before)
    after = load_json(args.after)
    postgres = load_json(args.postgres)
    validate_compatible_inputs(before, after, postgres)

    payload = {
        "schema_version": COMPARE_SCHEMA_VERSION,
        "generated_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "inputs": {
            "before": args.before,
            "after": args.after,
            "postgres": args.postgres,
        },
        "meta": {
            "scale": before["dataset"]["scale"],
            "seed": before["dataset"]["seed"],
            "scenario_set": before["run_config"]["scenario_set"],
        },
        "comparisons": compare_scenarios(before, after, postgres),
    }
    write_json(args.output, payload)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="db9-server M1 benchmark baseline harness")
    sub = parser.add_subparsers(dest="subcommand", required=True)

    gen = sub.add_parser("gen", help="Create schema and load deterministic benchmark data")
    gen.add_argument("--dsn", default=DEFAULT_DSN, help="Database DSN")
    gen.add_argument("--engine-label", default="db9_after", help="Engine label for metadata")
    gen.add_argument("--scale", choices=sorted(SCALES.keys()), default="S", help="Dataset scale")
    gen.add_argument("--seed", type=int, default=DEFAULT_SEED, help="Deterministic dataset seed")
    gen.add_argument("--batch-size", type=int, default=DEFAULT_BATCH_SIZE, help="Insert batch size")
    gen.add_argument("--analyze", action="store_true", help="Run ANALYZE after load")
    gen.add_argument("--output", help="Optional JSON output path for generation metadata")
    gen.set_defaults(func=benchmark_gen)

    run = sub.add_parser("run", help="Run a scenario set and emit raw JSON results")
    run.add_argument("--dsn", default=DEFAULT_DSN, help="Database DSN")
    run.add_argument(
        "--engine-label",
        choices=["db9_before", "db9_after", "postgres_18_3"],
        required=True,
        help="Engine label stored in raw output",
    )
    run.add_argument("--scale", choices=sorted(SCALES.keys()), default="S", help="Dataset scale")
    run.add_argument("--seed", type=int, default=DEFAULT_SEED, help="Deterministic dataset seed")
    run.add_argument(
        "--scenario-set",
        choices=["all", "m2", "m2_m4", "m5", "m6", "m7", "m8"],
        default="all",
        help="Scenario set to execute",
    )
    run.add_argument("--warmup", type=int, default=DEFAULT_WARMUP, help="Warmup iterations per repeat")
    run.add_argument("--measured", type=int, default=DEFAULT_MEASURED, help="Measured iterations per repeat")
    run.add_argument("--repeats", type=int, default=DEFAULT_REPEATS, help="Number of outer repeats")
    run.add_argument(
        "--set",
        action="append",
        default=[],
        help="Extra session setting in the form name=value; may be repeated",
    )
    run.add_argument(
        "--parallel-setting-template",
        help="Optional SQL template applied to worker-count scenarios, for example: SET max_parallel_workers_per_gather = {workers}",
    )
    run.add_argument(
        "--memory-setting-template",
        help="Optional SQL template applied to memory-limited scenarios, for example: SET work_mem = '{mb}MB'",
    )
    run.add_argument(
        "--metrics-url",
        help="Optional Prometheus scrape URL, for example: http://127.0.0.1:9090/internal/metrics",
    )
    run.add_argument(
        "--metrics-timeout-seconds",
        type=float,
        default=5.0,
        help="Timeout for optional Prometheus scraping",
    )
    run.add_argument(
        "--capture-prom-metric",
        action="append",
        default=[],
        help=(
            "Capture one Prometheus series delta into the raw result. "
            "Format: alias=metric_name or alias=metric_name{label=\"value\"}. "
            "If alias matches a canonical counter field such as kv_requests or spill_bytes, "
            "the value is written into that field; otherwise it is stored under extra_metrics."
        ),
    )
    run.add_argument("--output", help="Output JSON path")
    run.set_defaults(func=benchmark_run)

    compare = sub.add_parser("compare", help="Compare db9 before/after with PostgreSQL raw outputs")
    compare.add_argument("--before", required=True, help="Raw JSON path for db9_before")
    compare.add_argument("--after", required=True, help="Raw JSON path for db9_after")
    compare.add_argument("--postgres", required=True, help="Raw JSON path for postgres_18_3")
    compare.add_argument("--output", help="Comparison JSON path")
    compare.set_defaults(func=benchmark_compare)

    return parser


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except ValueError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
