#!/usr/bin/env python3
"""
check_oracle_coverage.py

Verifies that:
1. Every PushdownSafe builtin function registered in
   src/sql/optimizer/pushdown/db9_cop.rs has at least one corresponding
   OracleCase in scripts/pg18_pushdown_oracle.py.
2. Every pushed-down operator / predicate / operator-like expression form
   that db9-server documents and rewrites has at least one corresponding
   OracleCase.
3. Local-only operator families that are intentionally kept out of DB9 Cop
   still have parity OracleCases, but are reported separately from pushed
   coverage.

Exit code 0: all covered.
Exit code 1: one or more functions or operators lack oracle coverage.

Usage:
    python3 scripts/check_oracle_coverage.py
    python3 scripts/check_oracle_coverage.py --verbose
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DB9_COP_RS = REPO_ROOT / "src" / "sql" / "optimizer" / "pushdown" / "db9_cop.rs"
ORACLE_PY = REPO_ROOT / "scripts" / "pg18_pushdown_oracle.py"

PUSHDOWN_OPERATOR_CASE_PREFIXES: dict[str, tuple[str, ...]] = {
    "logical_and": ("logical_and",),
    "logical_or": ("logical_or",),
    "logical_not": ("logical_not",),
    "eq": ("eq",),
    "not_eq": ("not_eq",),
    "lt": ("lt",),
    "lte": ("lte",),
    "gt": ("gt",),
    "gte": ("gte",),
    "is_null": ("is_null",),
    "is_not_null": ("is_not_null",),
    "is_distinct_from": ("is_distinct_from",),
    "is_not_distinct_from": ("is_not_distinct_from",),
    "between": ("between",),
    "not_between": ("not_between",),
    "in_list": ("in_list",),
    "not_in": ("not_in",),
    "is_true": ("is_true",),
    "is_not_true": ("is_not_true",),
    "is_false": ("is_false",),
    "is_not_false": ("is_not_false",),
    "is_unknown": ("is_unknown",),
    "is_not_unknown": ("is_not_unknown",),
    "like": ("like",),
    "not_like": ("not_like",),
    "ilike": ("ilike",),
    "not_ilike": ("not_ilike",),
    "unary_plus": ("unary_plus",),
    "unary_minus": ("unary_minus",),
    "bit_not": ("bit_not",),
    "bit_and": ("bit_and",),
    "bit_or": ("bit_or",),
    "bit_xor": ("bit_xor",),
    "shift_left": ("shift_left",),
    "shift_right": ("shift_right",),
}

LOCAL_ONLY_OPERATOR_CASE_PREFIXES: dict[str, tuple[str, ...]] = {
    "regex_match": ("regex_match",),
    "regex_not_match": ("regex_not_match",),
    "regex_imatch": ("regex_imatch",),
    "regex_not_imatch": ("regex_not_imatch",),
    "json_arrow": ("json_arrow", "jsonb_arrow"),
    "json_long_arrow": ("json_long_arrow", "jsonb_long_arrow"),
    "json_hash_arrow": ("json_hash_arrow", "jsonb_hash_arrow"),
    "json_hash_long_arrow": ("json_hash_long_arrow", "jsonb_hash_long_arrow"),
    "json_hash_minus": ("jsonb_hash_minus",),
    "json_contains": ("jsonb_contains",),
    "json_contained_by": ("jsonb_contained_by",),
    "json_exists": ("jsonb_exists_op",),
    "json_exists_any": ("jsonb_exists_any_op",),
    "json_exists_all": ("jsonb_exists_all_op",),
}


@dataclass(frozen=True)
class OracleCoverageCase:
    name: str
    expr: str
    query_sql: str | None
    pg_query_sql: str | None
    explain_policy: str

    def has_pushdown_explain_coverage(self) -> bool:
        return self.explain_policy != "skip"


def extract_builtin_policy_match_arms(rust_source: str) -> list[str]:
    """
    Return the top-level match arms inside db9_cop_builtin_function_policy.

    This stays bracket-aware so quoted function names only get attributed to
    the arm that actually owns the policy decision.
    """
    fn_start = rust_source.find("fn db9_cop_builtin_function_policy")
    if fn_start == -1:
        raise ValueError("Could not locate db9_cop_builtin_function_policy in db9_cop.rs")

    match_anchor = "match name.to_ascii_lowercase().as_str() {"
    match_start = rust_source.find(match_anchor, fn_start)
    if match_start == -1:
        raise ValueError("Could not locate builtin policy match in db9_cop.rs")

    body_start = rust_source.find("{", match_start) + 1
    depth = 1
    cursor = body_start
    while cursor < len(rust_source) and depth > 0:
        if rust_source[cursor] == "{":
            depth += 1
        elif rust_source[cursor] == "}":
            depth -= 1
        cursor += 1

    if depth != 0:
        raise ValueError("Could not parse builtin policy match body in db9_cop.rs")

    arms: list[str] = []
    current_arm: list[str] = []
    arm_depth = 0
    for line in rust_source[body_start : cursor - 1].splitlines():
        if arm_depth == 0 and "=>" in line:
            if current_arm:
                arms.append("\n".join(current_arm))
            current_arm = [line]
        elif current_arm:
            current_arm.append(line)

        line_without_strings = re.sub(r'"(?:\\.|[^"\\])*"', '""', line)
        arm_depth += line_without_strings.count("{") - line_without_strings.count("}")

    if current_arm:
        arms.append("\n".join(current_arm))

    return arms


def extract_functions_for_policy(rust_source: str, policy: str) -> set[str]:
    """
    Extract builtin names whose own match arm can return the requested policy.
    """
    functions: set[str] = set()
    for arm in extract_builtin_policy_match_arms(rust_source):
        if f"Db9CopBuiltinPolicy::{policy}" not in arm:
            continue
        pattern = arm.split("=>", 1)[0]
        for fn_name in re.findall(r'"([a-z][a-z0-9_]*)"', pattern):
            functions.add(fn_name)

    return functions


def extract_pushdown_safe_functions(rust_source: str) -> set[str]:
    """
    Extract builtin names whose own match arm can return PushdownSafe.
    """
    return extract_functions_for_policy(rust_source, "PushdownSafe")


def extract_local_only_functions(rust_source: str) -> set[str]:
    """
    Extract builtin names whose own match arm can return LocalOnly.
    """
    return extract_functions_for_policy(rust_source, "LocalOnly")


def ast_string_value(node: ast.AST) -> str | None:
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    if isinstance(node, ast.JoinedStr):
        parts: list[str] = []
        for value in node.values:
            if isinstance(value, ast.Constant) and isinstance(value.value, str):
                parts.append(value.value)
            elif isinstance(value, ast.FormattedValue):
                parts.append("{}")
            else:
                return None
        return "".join(parts)
    return None


def extract_string_tuple_assignment(module: ast.Module, name: str) -> tuple[str, ...]:
    for node in module.body:
        value: ast.AST | None = None
        if isinstance(node, ast.Assign):
            if any(isinstance(target, ast.Name) and target.id == name for target in node.targets):
                value = node.value
        elif isinstance(node, ast.AnnAssign):
            if isinstance(node.target, ast.Name) and node.target.id == name:
                value = node.value
        if value is None:
            continue
        literal = ast.literal_eval(value)
        if not isinstance(literal, tuple) or not all(isinstance(item, str) for item in literal):
            raise ValueError(f"{name} must be a tuple[str, ...]")
        return literal
    return ()


def case_name_matches_prefix(name: str, prefix: str) -> bool:
    return name == prefix or name.startswith(prefix + "_")


def effective_explain_policy(
    name: str,
    explicit_policy: str,
    local_only_names: tuple[str, ...],
    local_only_prefixes: tuple[str, ...],
) -> str:
    if explicit_policy != "pushdown":
        return explicit_policy
    if name in local_only_names or any(
        case_name_matches_prefix(name, prefix) for prefix in local_only_prefixes
    ):
        return "skip"
    return explicit_policy


def extract_oracle_cases(oracle_source: str) -> list[OracleCoverageCase]:
    """
    Extract OracleCase metadata with the same local-only defaulting rules used
    by pg18_pushdown_oracle.py.
    """
    module = ast.parse(oracle_source)
    local_only_prefixes = extract_string_tuple_assignment(
        module, "LOCAL_ONLY_EXPLAIN_SKIP_CASE_PREFIXES"
    )
    local_only_names = extract_string_tuple_assignment(module, "LOCAL_ONLY_EXPLAIN_SKIP_CASE_NAMES")

    cases: list[OracleCoverageCase] = []
    for node in ast.walk(module):
        if not isinstance(node, ast.Call):
            continue
        if not isinstance(node.func, ast.Name) or node.func.id != "OracleCase":
            continue
        if len(node.args) < 2:
            raise ValueError("OracleCase must have name and expr positional args")
        name = ast_string_value(node.args[0])
        expr = ast_string_value(node.args[1])
        if name is None or expr is None:
            raise ValueError("OracleCase name and expr must be string literals")

        query_sql = None
        pg_query_sql = None
        explain_policy = "pushdown"
        for keyword in node.keywords:
            if keyword.arg == "query_sql":
                query_sql = ast_string_value(keyword.value)
            elif keyword.arg == "pg_query_sql":
                pg_query_sql = ast_string_value(keyword.value)
            elif keyword.arg == "explain_policy":
                value = ast_string_value(keyword.value)
                if value is None:
                    raise ValueError(f"OracleCase {name} explain_policy must be a string literal")
                explain_policy = value

        cases.append(
            OracleCoverageCase(
                name=name,
                expr=expr,
                query_sql=query_sql,
                pg_query_sql=pg_query_sql,
                explain_policy=effective_explain_policy(
                    name,
                    explain_policy,
                    local_only_names,
                    local_only_prefixes,
                ),
            )
        )

    return cases


def sql_contains_function_call(sql: str, fn_name: str) -> bool:
    pattern = re.compile(rf"(?<![A-Za-z0-9_]){re.escape(fn_name)}\s*\(")
    return pattern.search(sql) is not None


def case_name_covers_function(case_name: str, fn_name: str, all_fn_names: set[str]) -> bool:
    if case_name == fn_name:
        return True
    if not case_name.startswith(fn_name + "_"):
        return False
    return not any(
        other != fn_name
        and other.startswith(fn_name + "_")
        and (case_name == other or case_name.startswith(other + "_"))
        for other in all_fn_names
    )


def case_covers_function(case: OracleCoverageCase, fn_name: str, all_fn_names: set[str]) -> bool:
    if case_name_covers_function(case.name, fn_name, all_fn_names):
        return True
    if sql_contains_function_call(case.expr, fn_name):
        return True
    if case.query_sql and sql_contains_function_call(case.query_sql, fn_name):
        return True
    return bool(case.pg_query_sql and sql_contains_function_call(case.pg_query_sql, fn_name))


def function_is_covered(
    fn_name: str,
    cases: list[OracleCoverageCase],
    all_fn_names: set[str],
) -> bool:
    """
    A function is covered if:
    1. Any case name starts with fn_name (e.g. "sqrt" covers "sqrt", "sqrt_negative", etc.)
    2. Any expr contains fn_name followed by '(' (e.g. "sqrt(16.0..." covers "sqrt")
    3. Any query_sql contains fn_name followed by '(' or space
    """
    for case in cases:
        if case.has_pushdown_explain_coverage() and case_covers_function(
            case,
            fn_name,
            all_fn_names,
        ):
            return True

    return False


def operator_group_is_covered(
    cases: list[OracleCoverageCase],
    prefixes: tuple[str, ...],
    *,
    require_pushdown_explain: bool,
) -> bool:
    for case in cases:
        if require_pushdown_explain != case.has_pushdown_explain_coverage():
            continue
        for prefix in prefixes:
            if case_name_matches_prefix(case.name, prefix):
                return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description="Check oracle coverage for PushdownSafe functions")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    rust_source = DB9_COP_RS.read_text()
    oracle_source = ORACLE_PY.read_text()

    try:
        safe_fns = extract_pushdown_safe_functions(rust_source)
    except ValueError as e:
        print(f"ERROR: {e}", file=sys.stderr)
        return 1

    try:
        oracle_cases = extract_oracle_cases(oracle_source)
    except ValueError as e:
        print(f"ERROR: {e}", file=sys.stderr)
        return 1

    local_only_fns = extract_local_only_functions(rust_source) - safe_fns
    all_fn_names = safe_fns | local_only_fns

    missing: list[str] = []
    covered: list[str] = []

    for fn_name in sorted(safe_fns):
        if function_is_covered(fn_name, oracle_cases, all_fn_names):
            covered.append(fn_name)
        else:
            missing.append(fn_name)

    missing_operator_groups: list[str] = []
    covered_operator_groups: list[str] = []
    for group_name, prefixes in sorted(PUSHDOWN_OPERATOR_CASE_PREFIXES.items()):
        if operator_group_is_covered(
            oracle_cases,
            prefixes,
            require_pushdown_explain=True,
        ):
            covered_operator_groups.append(group_name)
        else:
            missing_operator_groups.append(group_name)

    missing_local_only_operator_groups: list[str] = []
    covered_local_only_operator_groups: list[str] = []
    for group_name, prefixes in sorted(LOCAL_ONLY_OPERATOR_CASE_PREFIXES.items()):
        if operator_group_is_covered(
            oracle_cases,
            prefixes,
            require_pushdown_explain=False,
        ):
            covered_local_only_operator_groups.append(group_name)
        else:
            missing_local_only_operator_groups.append(group_name)

    misclassified_local_only_cases: list[str] = []
    for case in oracle_cases:
        if not case.has_pushdown_explain_coverage():
            continue
        for fn_name in sorted(local_only_fns):
            if case_covers_function(case, fn_name, all_fn_names):
                misclassified_local_only_cases.append(f"{case.name} -> {fn_name}")
                break

    if args.verbose:
        print(f"PushdownSafe functions found: {len(safe_fns)}")
        print(f"Covered: {len(covered)}")
        for fn in covered:
            print(f"  ✓ {fn}")
        print(f"\nPushed-down operator groups tracked: {len(PUSHDOWN_OPERATOR_CASE_PREFIXES)}")
        print(f"Covered operator groups: {len(covered_operator_groups)}")
        for group in covered_operator_groups:
            print(f"  ✓ {group}")
        print(
            "\nLocal-only operator parity groups tracked: "
            f"{len(LOCAL_ONLY_OPERATOR_CASE_PREFIXES)}"
        )
        print(f"Covered local-only operator groups: {len(covered_local_only_operator_groups)}")
        for group in covered_local_only_operator_groups:
            print(f"  ✓ {group}")

    if missing:
        print(f"\nMISSING oracle coverage for {len(missing)} PushdownSafe function(s):")
        for fn in missing:
            print(f"  ✗ {fn}")
        print(
            "\nAdd at least one OracleCase whose name starts with the function name "
            "or whose expr contains '<function_name>(' to pg18_pushdown_oracle.py."
        )
        return 1

    if missing_operator_groups:
        print(
            f"\nMISSING oracle coverage for {len(missing_operator_groups)} pushed-down operator group(s):"
        )
        for group in missing_operator_groups:
            print(f"  ✗ {group}")
        print(
            "\nAdd at least one OracleCase whose name matches the missing operator-group "
            "prefix in scripts/pg18_pushdown_oracle.py."
        )
        return 1

    if missing_local_only_operator_groups:
        print(
            "\nMISSING oracle parity coverage for "
            f"{len(missing_local_only_operator_groups)} local-only operator group(s):"
        )
        for group in missing_local_only_operator_groups:
            print(f"  ✗ {group}")
        print(
            "\nAdd at least one local-only OracleCase whose name matches the missing "
            "operator-group prefix in scripts/pg18_pushdown_oracle.py."
        )
        return 1

    if misclassified_local_only_cases:
        print(
            "\nMISCLASSIFIED oracle case(s): local-only function cases still require "
            "pushdown EXPLAIN validation:"
        )
        for case in misclassified_local_only_cases:
            print(f"  ✗ {case}")
        print(
            "\nAdd the case name or prefix to LOCAL_ONLY_EXPLAIN_SKIP_CASE_* in "
            "scripts/pg18_pushdown_oracle.py, or update the planner policy if the "
            "function really became PushdownSafe."
        )
        return 1

    print(
        "OK: all "
        f"{len(safe_fns)} PushdownSafe functions and "
        f"{len(PUSHDOWN_OPERATOR_CASE_PREFIXES)} pushed-down operator groups "
        "have oracle coverage; "
        f"{len(LOCAL_ONLY_OPERATOR_CASE_PREFIXES)} local-only operator groups "
        "have parity coverage."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
