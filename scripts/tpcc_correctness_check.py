#!/usr/bin/env python3
"""Run invariant-style TPC-C correctness checks against a PostgreSQL-compatible endpoint."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import textwrap
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, List, Optional
from urllib.parse import unquote, urlparse


TABLES = [
    "warehouse",
    "district",
    "customer",
    "history",
    "item",
    "stock",
    "orders",
    "new_order",
    "order_line",
]


@dataclass(frozen=True)
class ConnectionConfig:
    host: str
    port: int
    user: str
    password: str
    database: str


@dataclass(frozen=True)
class CheckDef:
    check_id: str
    description: str
    sql: str
    expected: int = 0


CHECKS: List[CheckDef] = [
    CheckDef(
        "warehouse_count_nonzero",
        "warehouse must contain at least one row",
        "SELECT CASE WHEN COUNT(*) > 0 THEN 0 ELSE 1 END FROM warehouse;",
    ),
    CheckDef(
        "districts_per_warehouse",
        "each warehouse must own exactly 10 districts",
        """
        SELECT COUNT(*)
        FROM (
            SELECT d_w_id
            FROM district
            GROUP BY d_w_id
            HAVING COUNT(*) <> CAST(10 AS BIGINT)
        ) AS anomalies;
        """,
    ),
    CheckDef(
        "customers_per_district",
        "each district must contain exactly 3000 customers",
        """
        SELECT COUNT(*)
        FROM (
            SELECT c_w_id, c_d_id
            FROM customer
            GROUP BY c_w_id, c_d_id
            HAVING COUNT(*) <> CAST(3000 AS BIGINT)
        ) AS anomalies;
        """,
    ),
    CheckDef(
        "stock_per_warehouse_matches_item_count",
        "each warehouse must have one stock row per item",
        """
        SELECT COUNT(*)
        FROM (
            SELECT s_w_id
            FROM stock
            GROUP BY s_w_id
            HAVING COUNT(*) <> (SELECT COUNT(*) FROM item)
        ) AS anomalies;
        """,
    ),
    CheckDef(
        "orders_match_d_next_o_id",
        "district.d_next_o_id must equal both max(order id)+1 and order count+1",
        """
        SELECT COUNT(*)
        FROM (
            SELECT per_district.d_w_id, per_district.d_id
            FROM (
                SELECT d.d_w_id, d.d_id,
                       CAST(d.d_next_o_id AS BIGINT) - CAST(1 AS BIGINT) AS expected_orders,
                       COUNT(o.o_id) AS order_count,
                       COALESCE(MAX(CAST(o.o_id AS BIGINT)), CAST(0 AS BIGINT)) AS max_o_id
                FROM district AS d
                LEFT JOIN orders AS o
                  ON o.o_w_id = d.d_w_id
                 AND o.o_d_id = d.d_id
                GROUP BY d.d_w_id, d.d_id, d.d_next_o_id
            ) AS per_district
            WHERE per_district.expected_orders <> per_district.order_count
               OR per_district.expected_orders <> per_district.max_o_id
        ) AS anomalies;
        """,
    ),
    CheckDef(
        "orders_reference_existing_customer",
        "every order must reference an existing customer",
        """
        SELECT COUNT(*)
        FROM orders AS o
        LEFT JOIN customer AS c
          ON c.c_w_id = o.o_w_id
         AND c.c_d_id = o.o_d_id
         AND c.c_id = o.o_c_id
        WHERE c.c_id IS NULL;
        """,
    ),
    CheckDef(
        "new_order_rows_reference_pending_orders",
        "every new_order row must reference an order whose carrier is still NULL",
        """
        SELECT COUNT(*)
        FROM new_order AS no
        LEFT JOIN orders AS o
          ON o.o_w_id = no.no_w_id
         AND o.o_d_id = no.no_d_id
         AND o.o_id = no.no_o_id
        WHERE o.o_id IS NULL
           OR o.o_carrier_id IS NOT NULL;
        """,
    ),
    CheckDef(
        "pending_orders_have_new_order_row",
        "every order without carrier assignment must still exist in new_order",
        """
        SELECT COUNT(*)
        FROM orders AS o
        LEFT JOIN new_order AS no
          ON no.no_w_id = o.o_w_id
         AND no.no_d_id = o.o_d_id
         AND no.no_o_id = o.o_id
        WHERE o.o_carrier_id IS NULL
          AND no.no_o_id IS NULL;
        """,
    ),
    CheckDef(
        "order_line_count_matches_o_ol_cnt",
        "each order must have 5..15 order_line rows and match orders.o_ol_cnt",
        """
        SELECT COUNT(*)
        FROM (
            SELECT per_order.o_w_id, per_order.o_d_id, per_order.o_id
            FROM orders AS o
            JOIN (
                SELECT o_inner.o_w_id, o_inner.o_d_id, o_inner.o_id, o_inner.o_ol_cnt,
                       COUNT(ol.ol_number) AS line_count
                FROM orders AS o_inner
                LEFT JOIN order_line AS ol
                  ON ol.ol_w_id = o_inner.o_w_id
                 AND ol.ol_d_id = o_inner.o_d_id
                 AND ol.ol_o_id = o_inner.o_id
                GROUP BY o_inner.o_w_id, o_inner.o_d_id, o_inner.o_id, o_inner.o_ol_cnt
            ) AS per_order
              ON per_order.o_w_id = o.o_w_id
             AND per_order.o_d_id = o.o_d_id
             AND per_order.o_id = o.o_id
            WHERE per_order.line_count <> CAST(per_order.o_ol_cnt AS BIGINT)
               OR per_order.line_count < CAST(5 AS BIGINT)
               OR per_order.line_count > CAST(15 AS BIGINT)
        ) AS anomalies;
        """,
    ),
    CheckDef(
        "order_line_delivery_matches_order_status",
        "delivered order lines and order carrier assignment must stay consistent",
        """
        SELECT COUNT(*)
        FROM order_line AS ol
        JOIN orders AS o
          ON o.o_w_id = ol.ol_w_id
         AND o.o_d_id = ol.ol_d_id
         AND o.o_id = ol.ol_o_id
        WHERE (o.o_carrier_id IS NULL AND ol.ol_delivery_d IS NOT NULL)
           OR (o.o_carrier_id IS NOT NULL AND ol.ol_delivery_d IS NULL);
        """,
    ),
    CheckDef(
        "history_references_existing_parents",
        "every history row must reference an existing customer, district, and warehouse",
        """
        SELECT COUNT(*)
        FROM history AS h
        LEFT JOIN customer AS c
          ON c.c_w_id = h.h_c_w_id
         AND c.c_d_id = h.h_c_d_id
         AND c.c_id = h.h_c_id
        LEFT JOIN district AS d
          ON d.d_w_id = h.h_w_id
         AND d.d_id = h.h_d_id
        LEFT JOIN warehouse AS w
          ON w.w_id = h.h_w_id
        WHERE c.c_id IS NULL
           OR d.d_id IS NULL
           OR w.w_id IS NULL;
        """,
    ),
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Validate TPC-C relational invariants against a PostgreSQL-compatible endpoint."
    )
    parser.add_argument("--dsn", help="PostgreSQL DSN, for example postgresql://user:pass@127.0.0.1:5432/db")
    parser.add_argument("--host")
    parser.add_argument("--port", type=int)
    parser.add_argument("--user")
    parser.add_argument("--password", default="")
    parser.add_argument("--db", dest="database")
    parser.add_argument("--label", required=True, help="Short label for the target engine, for example pg18 or db9")
    parser.add_argument("--phase", required=True, help="Validation phase label, for example after_prepare or after_run")
    parser.add_argument("--psql-bin", default="psql", help="Path to the psql client binary")
    parser.add_argument("--output", help="Optional JSON output path")
    args = parser.parse_args()
    if not args.dsn:
        missing = [name for name in ("host", "port", "user", "database") if getattr(args, name) in (None, "")]
        if missing:
            parser.error(f"either --dsn or all of --host/--port/--user/--db are required; missing: {', '.join(missing)}")
    return args


def connection_from_args(args: argparse.Namespace) -> ConnectionConfig:
    if not args.dsn:
        return ConnectionConfig(
            host=args.host,
            port=args.port,
            user=args.user,
            password=args.password,
            database=args.database,
        )

    parsed = urlparse(args.dsn)
    if not parsed.scheme.startswith("postgres"):
        raise SystemExit(f"unsupported DSN scheme in {args.dsn!r}")
    if not parsed.hostname or not parsed.username or not parsed.path or parsed.path == "/":
        raise SystemExit(f"incomplete DSN: {args.dsn!r}")
    return ConnectionConfig(
        host=parsed.hostname,
        port=parsed.port or 5432,
        user=unquote(parsed.username),
        password=unquote(parsed.password or ""),
        database=unquote(parsed.path.lstrip("/")),
    )


def run_scalar_query(psql_bin: str, conn: ConnectionConfig, sql: str) -> tuple[int, float]:
    command = [
        psql_bin,
        "-h",
        conn.host,
        "-p",
        str(conn.port),
        "-U",
        conn.user,
        "-d",
        conn.database,
        "-v",
        "ON_ERROR_STOP=1",
        "-Atqc",
        textwrap.dedent(sql).strip(),
    ]
    env = os.environ.copy()
    if conn.password:
        env["PGPASSWORD"] = conn.password
    started = time.monotonic()
    result = subprocess.run(
        command,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    elapsed = time.monotonic() - started
    if result.returncode != 0:
        stderr = result.stderr.strip()
        stdout = result.stdout.strip()
        detail = stderr or stdout or f"psql exited with code {result.returncode}"
        raise RuntimeError(detail)
    output = result.stdout.strip()
    if not output:
        raise RuntimeError("query returned no rows")
    try:
        value = int(output.splitlines()[-1].strip())
    except ValueError as exc:
        raise RuntimeError(f"expected integer scalar output, got: {output!r}") from exc
    return value, elapsed


def collect_table_counts(psql_bin: str, conn: ConnectionConfig) -> Dict[str, int]:
    counts: Dict[str, int] = {}
    for table_name in TABLES:
        count, _elapsed = run_scalar_query(psql_bin, conn, f"SELECT COUNT(*) FROM {table_name};")
        counts[table_name] = count
    return counts


def main() -> int:
    args = parse_args()
    conn = connection_from_args(args)
    try:
        table_counts = collect_table_counts(args.psql_bin, conn)
        check_results = []
        all_passed = True
        for check in CHECKS:
            value, elapsed = run_scalar_query(args.psql_bin, conn, check.sql)
            ok = value == check.expected
            all_passed = all_passed and ok
            check_results.append(
                {
                    "check_id": check.check_id,
                    "description": check.description,
                    "value": value,
                    "expected": check.expected,
                    "ok": ok,
                    "elapsed_ms": round(elapsed * 1000, 3),
                }
            )
        output = {
            "timestamp_utc": datetime.now(timezone.utc).isoformat(),
            "label": args.label,
            "phase": args.phase,
            "connection": {
                "host": conn.host,
                "port": conn.port,
                "user": conn.user,
                "database": conn.database,
            },
            "table_counts": table_counts,
            "all_passed": all_passed,
            "checks": check_results,
        }
    except Exception as exc:  # pragma: no cover - exercised via local validation
        output = {
            "timestamp_utc": datetime.now(timezone.utc).isoformat(),
            "label": args.label,
            "phase": args.phase,
            "all_passed": False,
            "error": str(exc),
        }
        if args.output:
            output_path = Path(args.output)
            output_path.parent.mkdir(parents=True, exist_ok=True)
            output_path.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
        json.dump(output, sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
        return 1

    if args.output:
        output_path = Path(args.output)
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    json.dump(output, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0 if output["all_passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
