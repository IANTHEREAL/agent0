#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import sys
from dataclasses import asdict, dataclass
from datetime import date, datetime, time, timezone
from pathlib import Path
from typing import Any

import psycopg


PROBE_TABLE = "db9_pushdown_pg18_oracle_probe"
TARGET_FILTER = "t.n = 20"
DEFAULT_DB9_DSN = "postgres://admin:admin@127.0.0.1:15433/postgres"
DEFAULT_PG_DSN = "postgresql://postgres@127.0.0.1:5432/postgres"
PG18_EXTENSION_SETUP = [
    "CREATE EXTENSION IF NOT EXISTS pgcrypto",
    'CREATE EXTENSION IF NOT EXISTS "uuid-ossp"',
]

LOCAL_ONLY_EXPLAIN_SKIP_CASE_PREFIXES: tuple[str, ...] = (
    "array_to_string",
    "concat",
    "concat_ws",
    "encode",
    "format",
    "initcap",
    "json_array_length",
    "json_build_array",
    "json_build_object",
    "json_extract_path",
    "json_extract_path_text",
    "json_typeof",
    "jsonb_array_length",
    "jsonb_build_array",
    "jsonb_build_object",
    "jsonb_exists",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_pretty",
    "jsonb_set",
    "jsonb_typeof",
    "lower",
    "lpad",
    "overlay",
    "quote_ident",
    "quote_literal",
    "quote_nullable",
    "regexp_match",
    "regexp_replace",
    "regexp_split_to_array",
    "replace",
    "repeat",
    "row_to_json",
    "rpad",
    "starts_with",
    "to_hex",
    "to_json",
    "to_jsonb",
    "translate",
    "upper",
)
LOCAL_ONLY_EXPLAIN_SKIP_CASE_NAMES: tuple[str, ...] = (
    "date_text",
    "decode_base64_invalid_end",
    "decode_base64_invalid_symbol",
    "decode_base64_noncanonical_suffix",
    "decode_base64_roundtrip",
    "decode_base64_unexpected_padding",
    "decode_escape_roundtrip",
    "decode_hex_invalid_digit",
    "decode_hex_odd_digits",
    "decode_hex_prefixed_input",
    "decode_hex_roundtrip",
    "decode_unknown_encoding_base64_padded",
    "decode_unknown_encoding_padded",
    "digest_sha256_bytea",
    "digest_sha256_text",
    "digest_sha256_timestamp",
    "digest_sha256_timestamptz",
    "digest_unknown_algorithm",
    "digest_unknown_algorithm_padded",
    "sha256_timestamp",
    "sha256_timestamptz",
    "substring_bytea_2",
    "substring_bytea_3",
    "substring_bytea_negative_start",
    "substring_bytea_zero_start",
    "to_char_date",
    "to_char_text",
)


def case_name_matches_prefix(name: str, prefix: str) -> bool:
    return name == prefix or name.startswith(prefix + "_")


def case_uses_local_only_explain_policy(name: str) -> bool:
    return name in LOCAL_ONLY_EXPLAIN_SKIP_CASE_NAMES or any(
        case_name_matches_prefix(name, prefix)
        for prefix in LOCAL_ONLY_EXPLAIN_SKIP_CASE_PREFIXES
    )


@dataclass(frozen=True)
class OracleCase:
    name: str
    expr: str
    query_sql: str | None = None
    pg_query_sql: str | None = None
    explain_policy: str = "pushdown"
    db9_settings: tuple[tuple[str, str], ...] = ()
    pg18_policy: str = "strict"
    compat_note: str | None = None
    setup_sql: tuple[str, ...] = ()
    transactional: bool = False
    tags: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        if self.explain_policy == "pushdown" and case_uses_local_only_explain_policy(self.name):
            object.__setattr__(self, "explain_policy", "skip")


@dataclass
class QueryOutcome:
    status: str
    value_type: str | None = None
    value_text: str | None = None
    sqlstate: str | None = None
    message: str | None = None


@dataclass
class CaseResult:
    name: str
    expr: str
    db9_on: QueryOutcome
    db9_off: QueryOutcome
    pg18: QueryOutcome
    explain_on_has_cop: bool
    explain_off_has_cop: bool
    explain_on_error: str | None
    explain_off_error: str | None
    pg18_policy: str
    compat_note: str | None
    notes: list[str]
    issues: list[str]


CASES: list[OracleCase] = [
    OracleCase("lower", "lower(t.txt)"),
    OracleCase("upper", "upper(t.txt)"),
    OracleCase("length", "length(t.txt_u)"),
    OracleCase("length_bytea", "length('\\x4142'::bytea)"),
    OracleCase("char_length", "char_length(t.txt_u)"),
    OracleCase("char_length_bytea", "char_length('\\x4142'::bytea)"),
    OracleCase("character_length", "character_length(t.txt_u)"),
    OracleCase("character_length_bytea", "character_length('\\x4142'::bytea)"),
    OracleCase("trim", "trim(both 'xy' from ('xy' || t.txt || 'xy'))"),
    OracleCase("concat", "concat(t.txt, NULL, t.txt2, t.n)", tags=("phase2_string_regex",)),
    OracleCase(
        "concat_ws",
        "concat_ws(',', t.txt, NULL, t.txt2, t.n)",
        tags=("phase2_string_regex",),
    ),
    OracleCase(
        "concat_temporal",
        "concat(t.ts, '|', t.ts_tz)",
        setup_sql=("SET TIME ZONE 'America/Los_Angeles'",),
    ),
    OracleCase(
        "concat_ws_temporal",
        "concat_ws(',', t.ts, NULL::text, t.ts_tz)",
        setup_sql=("SET TIME ZONE 'America/Los_Angeles'",),
    ),
    OracleCase("left", "left(t.txt, 2)", tags=("phase2_string_regex",)),
    OracleCase("left_bigint", "left(t.txt, 2::bigint)"),
    OracleCase("left_negative", "left(t.txt, -1)"),
    OracleCase("left_negative_bigint", "left(t.txt, -1::bigint)"),
    OracleCase("right", "right(t.txt, 2)", tags=("phase2_string_regex",)),
    OracleCase("right_negative", "right(t.txt, -1)"),
    OracleCase("right_negative_bigint", "right(t.txt, -1::bigint)"),
    OracleCase("repeat", "repeat(t.txt2, 3)", tags=("phase2_string_regex",)),
    OracleCase("repeat_bigint", "repeat(t.txt2, 3::bigint)"),
    OracleCase("reverse", "reverse(t.txt)", tags=("phase2_string_regex",)),
    OracleCase("initcap", "initcap('hello world from db9')", tags=("phase2_string_regex",)),
    OracleCase("ascii", "ascii('A')", tags=("phase2_string_regex",)),
    OracleCase("chr_int32", "chr(65)", tags=("phase2_string_regex",)),
    OracleCase("chr_bigint", "chr(t.n2)"),
    OracleCase("chr_zero", "chr(0)"),
    OracleCase("chr_negative", "chr(-1)"),
    OracleCase("chr_too_large", "chr(1114112)"),
    OracleCase("to_hex_int32", "to_hex(42)"),
    OracleCase("to_hex_negative_int32", "to_hex(-1)"),
    OracleCase("to_hex_bigint", "to_hex(2147483648::bigint)"),
    OracleCase("to_hex_negative_bigint", "to_hex(-1::bigint)"),
    OracleCase("substring_2", "substring(t.txt from 2)"),
    OracleCase("substring_bigint", "substring(t.txt from 2::bigint)"),
    OracleCase("substring_3", "substring(t.txt from 2 for 2)"),
    OracleCase(
        "substring_bytea_2",
        "encode(substring('\\x010203'::bytea from 2), 'hex')",
    ),
    OracleCase(
        "substring_bytea_3",
        "encode(substring('\\x010203'::bytea from 2 for 1), 'hex')",
    ),
    OracleCase(
        "substring_bytea_zero_start",
        "encode(substring('\\x010203'::bytea from 0 for 3), 'hex')",
    ),
    OracleCase(
        "substring_bytea_negative_start",
        "encode(substring('\\x010203'::bytea from -1 for 4), 'hex')",
    ),
    OracleCase("substring_zero_start", "substring('abc' from 0 for 3)"),
    OracleCase("substring_negative_start", "substring('abc' from -1 for 4)"),
    OracleCase("substring_negative_length", "substring('abc' from 2 for -1)"),
    OracleCase("substr_2", "substr(t.txt, 2)"),
    OracleCase("substr_bigint", "substr(t.txt, 2::bigint)"),
    OracleCase("substr_3", "substr(t.txt, 2, 2)"),
    OracleCase("btrim_1", "btrim('xy' || t.txt || 'xy')"),
    OracleCase("btrim_2", "btrim('xy' || t.txt || 'xy', 'xy')"),
    OracleCase("trim_tab_preserved", r"""trim(E'\tabc\t')"""),
    OracleCase("btrim_tab_preserved", r"""btrim(E'\tabc\t')"""),
    OracleCase("ltrim_tab_preserved", r"""ltrim(E'\tabc')"""),
    OracleCase("rtrim_tab_preserved", r"""rtrim(E'abc\t')"""),
    OracleCase("ltrim_1", "ltrim('   ' || t.txt)"),
    OracleCase("ltrim_2", "ltrim('xy' || t.txt, 'xy')"),
    OracleCase("rtrim_1", "rtrim(t.txt || '   ')"),
    OracleCase("rtrim_2", "rtrim(t.txt || 'xy', 'xy')"),
    OracleCase("lpad_2", "lpad(t.txt2, 5)"),
    OracleCase("lpad_3", "lpad(t.txt2, 5, '0')", tags=("phase2_string_regex",)),
    OracleCase("lpad_bigint", "lpad(t.txt2, 5::bigint, '0')"),
    OracleCase("rpad_2", "rpad(t.txt2, 5)"),
    OracleCase("rpad_3", "rpad(t.txt2, 5, '0')", tags=("phase2_string_regex",)),
    OracleCase("rpad_bigint", "rpad(t.txt2, 5::bigint, '0')"),
    OracleCase("replace", "replace('banana', 'na', 'X')", tags=("phase2_string_regex",)),
    OracleCase("translate", "translate('12345', '143', 'ax')", tags=("phase2_string_regex",)),
    OracleCase("strpos", "strpos('banana', 'na')"),
    OracleCase("starts_with", "starts_with('alphabet', 'alpha')"),
    OracleCase("position", "position('na' in 'banana')", tags=("phase2_string_regex",)),
    OracleCase("split_part", "split_part('a,b,c', ',', 2)", tags=("phase2_string_regex",)),
    OracleCase("split_part_bigint", "split_part('a,b,c', ',', 2::bigint)"),
    OracleCase("quote_ident", "quote_ident('select')", tags=("phase2_string_regex",)),
    OracleCase("quote_literal", "quote_literal('O''Reilly')", tags=("phase2_string_regex",)),
    OracleCase("quote_nullable", "quote_nullable(t.null_txt)", tags=("phase2_string_regex",)),
    OracleCase("quote_literal_timestamp", "quote_literal(t.ts)"),
    OracleCase("quote_nullable_timestamp", "quote_nullable(t.ts)"),
    OracleCase("quote_literal_timestamptz", "quote_literal(t.ts_tz)"),
    OracleCase("quote_nullable_timestamptz", "quote_nullable(t.ts_tz)"),
    OracleCase("overlay_3", "overlay('abcdef' placing t.txt2 from 3)"),
    OracleCase("overlay_4", "overlay('abcdef' placing t.txt2 from 3 for 2)", tags=("phase2_string_regex",)),
    OracleCase(
        "overlay_bytea_3",
        "encode(overlay('\\x01020304'::bytea placing '\\xaa'::bytea from 3), 'hex')",
    ),
    OracleCase(
        "overlay_bytea_4",
        "encode(overlay('\\x01020304'::bytea placing '\\xaabb'::bytea from 2 for 2), 'hex')",
    ),
    OracleCase(
        "overlay_bytea_zero_start_error",
        "overlay('\\x010203'::bytea placing '\\xff'::bytea from 0)",
    ),
    OracleCase("overlay_negative_count", "overlay('abcdef' placing 'Z' from 2 for -1)"),
    OracleCase("overlay_zero_start_error", "overlay('abcdef' placing 'Z' from 0)"),
    OracleCase(
        "overlay_overflow",
        "overlay('abcdef' placing 'Z' from 2147483647 for 1)",
    ),
    OracleCase(
        "overlay_bytea_overflow",
        "overlay('\\x010203'::bytea placing '\\xff'::bytea from 2147483647 for 1)",
    ),
    OracleCase(
        "overlay_bigint",
        "overlay('abcdef' placing t.txt2 from 3::bigint for 2::bigint)",
    ),
    OracleCase("format", "format('%s-%s-%s', t.txt2, t.n, t.null_txt)", tags=("phase2_string_regex",)),
    OracleCase(
        "format_timestamp",
        "format('%s|%L|%I', t.ts, t.ts, t.ts)",
    ),
    OracleCase(
        "format_timestamptz",
        "format('%s|%L|%I', t.ts_tz, t.ts_tz, t.ts_tz)",
        setup_sql=("SET TIME ZONE 'America/Los_Angeles'",),
    ),
    OracleCase("abs", "abs(-t.n)"),
    OracleCase("ceil", "ceil(-1.25::double precision)"),
    OracleCase("ceiling", "ceiling(1.25::double precision)"),
    OracleCase("floor", "floor(-1.25::double precision)"),
    OracleCase("trunc_1", "trunc(t.f8)"),
    OracleCase("trunc_2", "trunc(t.f8, 1)", explain_policy="skip"),
    OracleCase("trunc_null_precision", "trunc(4.567::numeric, NULL::int)", explain_policy="skip"),
    OracleCase("sqrt", "sqrt(16.0::double precision)"),
    OracleCase("sqrt_numeric", "sqrt(4::numeric)", explain_policy="skip"),
    OracleCase("sqrt_negative", "sqrt(-1.0::double precision)"),
    OracleCase("cbrt", "cbrt(27.0::double precision)"),
    OracleCase("cbrt_numeric", "cbrt(8::numeric)", explain_policy="skip"),
    OracleCase("exp", "exp(1.0::double precision)"),
    OracleCase("exp_numeric", "exp(1::numeric)", explain_policy="skip"),
    OracleCase("ln", "ln(1.0::double precision)"),
    OracleCase("ln_numeric", "ln(100::numeric)", explain_policy="skip"),
    OracleCase("ln_negative", "ln(-1.0::double precision)"),
    OracleCase("log", "log(100.0::double precision)"),
    OracleCase("log_numeric_one_arg", "log(100::numeric)", explain_policy="skip"),
    OracleCase("log_numeric_two_arg", "log(10::numeric, 1000::numeric)", explain_policy="skip"),
    OracleCase(
        "log_float8_two_arg_rejected",
        "log(10.0::double precision, 1000.0::double precision)",
        explain_policy="skip",
    ),
    OracleCase("log10", "log10(1000.0::double precision)"),
    OracleCase("log10_numeric", "log10(100::numeric)", explain_policy="skip"),
    OracleCase("log10_zero", "log10(0.0::double precision)"),
    OracleCase("degrees", "degrees(pi())"),
    OracleCase("radians", "radians(180.0::double precision)"),
    OracleCase("sin", "sin(0.5::double precision)"),
    OracleCase("cos", "cos(0.5::double precision)"),
    OracleCase("tan", "tan(0.5::double precision)"),
    OracleCase("asin", "asin(0.5::double precision)"),
    OracleCase("acos", "acos(0.5::double precision)"),
    OracleCase("atan", "atan(1.0::double precision)"),
    OracleCase("power", "power(2.0::double precision, 10.0::double precision)"),
    OracleCase(
        "power_negative_fractional",
        "power(-1.0::double precision, 0.5::double precision)",
    ),
    OracleCase(
        "power_zero_negative",
        "power(0.0::double precision, -1.0::double precision)",
    ),
    OracleCase("pow", "pow(2.0::double precision, 8.0::double precision)"),
    OracleCase("div_numeric", "div(9::numeric, 4::numeric)", explain_policy="skip"),
    OracleCase("div_negative_numeric", "div((-5.5)::numeric, 2::numeric)", explain_policy="skip"),
    OracleCase(
        "div_float_rejected",
        "div(9.0::double precision, 4::numeric)",
        explain_policy="skip",
    ),
    OracleCase("atan2", "atan2(1.0::double precision, 1.0::double precision)"),
    OracleCase("hashtext", "hashtext('hello')"),
    OracleCase("mod", "mod(17, 5)"),
    OracleCase("mod_float8", "mod(12.3::double precision, 5.0::double precision)"),
    OracleCase("sign", "sign(-42.5::double precision)"),
    OracleCase("width_bucket", "width_bucket(12.3456, 0.0, 20.0, 5)"),
    OracleCase(
        "width_bucket_bigint",
        "width_bucket(12.3456, 0.0::double precision, 20.0::double precision, 5::bigint)",
    ),
    OracleCase("pi", "pi()"),
    OracleCase("round_1", "round(t.f8)"),
    OracleCase("round_2", "round(t.f8, 1)", explain_policy="skip"),
    OracleCase("round_null_precision", "round(4.567::numeric, NULL::int)", explain_policy="skip"),
    OracleCase("date_part_timestamp", "date_part('month', t.ts)"),
    OracleCase(
        "date_part_interval",
        "date_part('epoch', make_interval(0, 0, 0, 2, 3, 4, 5.5))",
        explain_policy="skip",
    ),
    OracleCase("extract_timestamp", "extract(month from t.ts)"),
    OracleCase(
        "extract_interval",
        "extract(epoch from make_interval(0, 0, 0, 2, 3, 4, 5.5))",
        explain_policy="skip",
    ),
    OracleCase("date_timestamp", "date(t.ts)"),
    OracleCase("date_text", "date(t.txt_date)"),
    OracleCase(
        "date_timestamptz",
        "date(t.ts_tz)",
        explain_policy="skip",
    ),
    OracleCase("age", "age(t.ts2, t.ts)"),
    OracleCase(
        "age_timestamptz",
        "age(t.ts_tz, t.ts_tz)",
        explain_policy="skip",
    ),
    OracleCase("make_date", "make_date(2024, 3, 15)"),
    OracleCase("make_date_bigint", "make_date(2024::bigint, 3::bigint, 15::bigint)"),
    OracleCase("make_time", "make_time(1, 2, 3.5)"),
    OracleCase("make_time_bigint", "make_time(1::bigint, 2::bigint, 3.5)"),
    OracleCase(
        "make_timestamp",
        "make_timestamp(2024, 3, 15, 12, 34, 56.789)",
        explain_policy="skip",
    ),
    OracleCase(
        "make_timestamp_bigint",
        "make_timestamp(2024::bigint, 3::bigint, 15::bigint, 12::bigint, 34::bigint, 56.789)",
        explain_policy="skip",
    ),
    OracleCase("make_interval_0", "make_interval()", explain_policy="skip"),
    OracleCase("make_interval_7", "make_interval(1, 2, 0, 3, 4, 5, 6.5)", explain_policy="skip"),
    OracleCase(
        "make_interval_bigint",
        "make_interval(1::bigint, 2::bigint, 0::bigint, 3::bigint, 4::bigint, 5::bigint, 6.5)",
        explain_policy="skip",
    ),
    OracleCase("to_timestamp", "to_timestamp(1700000000.5)", explain_policy="skip"),
    OracleCase("to_char_timestamp", "to_char(t.ts, 'YYYY-MM-DD HH24:MI:SS')"),
    OracleCase("to_char_date", "to_char(date(t.ts), 'YYYY-MM-DD')"),
    OracleCase("to_char_text", "to_char(t.txt_date, 'YYYY-MM-DD')"),
    OracleCase("date_trunc", "date_trunc('hour', t.ts)"),
    OracleCase(
        "now_eq_current_timestamp",
        "now() = current_timestamp",
        query_sql="SELECT (now() = current_timestamp) AS value",
        explain_policy="skip",
    ),
    OracleCase(
        "transaction_timestamp_eq_now",
        "transaction_timestamp() = now()",
        query_sql="SELECT (transaction_timestamp() = now()) AS value",
        explain_policy="skip",
    ),
    OracleCase(
        "statement_timestamp_non_null",
        "statement_timestamp() IS NOT NULL",
        query_sql="SELECT (statement_timestamp() IS NOT NULL) AS value",
        explain_policy="skip",
    ),
    OracleCase(
        "current_date_matches_timestamp",
        "current_date = date(current_timestamp)",
        query_sql="SELECT (current_date = date(current_timestamp)) AS value",
        explain_policy="skip",
    ),
    OracleCase(
        "current_time_hour_in_range",
        "extract(hour from current_time) between 0 and 23",
        query_sql=(
            "SELECT (extract(hour from current_time) >= 0 "
            "AND extract(hour from current_time) <= 23) AS value"
        ),
        explain_policy="skip",
    ),
    OracleCase(
        "current_time_pg_typeof",
        "pg_typeof(current_time)",
        query_sql="SELECT pg_typeof(current_time)::text AS value",
        explain_policy="skip",
    ),
    OracleCase("logical_and", "(t.n = 20) AND t.flag"),
    OracleCase("logical_or", "(t.n = 10) OR t.flag"),
    OracleCase("logical_not", "NOT (t.n = 10)"),
    OracleCase("eq", "t.n = 20"),
    OracleCase("not_eq", "t.n <> 10"),
    OracleCase("lt", "t.n < 30"),
    OracleCase("lte", "t.n <= 20"),
    OracleCase("gt", "t.n > 10"),
    OracleCase("gte", "t.n >= 20"),
    OracleCase("is_null", "t.null_txt IS NULL"),
    OracleCase("is_not_null", "t.txt IS NOT NULL"),
    OracleCase("like", "t.txt LIKE 'A%'"),
    OracleCase("ilike", "t.txt ILIKE 'a%'"),
    OracleCase("not_like", "t.txt NOT LIKE 'z%'"),
    OracleCase("not_ilike", "t.txt NOT ILIKE 'z%'"),
    OracleCase(
        "like_escape",
        r"""'A%C' LIKE 'A#%C' ESCAPE '#'""",
        query_sql=(
            f"SELECT ('A%C' LIKE 'A#%C' ESCAPE '#') AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER}"
        ),
    ),
    OracleCase("between", "t.n BETWEEN 10 AND 20"),
    OracleCase("not_between", "t.n NOT BETWEEN 21 AND 30"),
    OracleCase("in_list", "t.n IN (10, 20, NULL)"),
    OracleCase("not_in_with_null", "t.n NOT IN (10, NULL)"),
    OracleCase("is_true", "t.flag IS TRUE"),
    OracleCase("is_not_true", "t.flag IS NOT TRUE"),
    OracleCase("is_false", "t.flag IS FALSE"),
    OracleCase("is_not_false", "t.flag IS NOT FALSE"),
    OracleCase(
        "is_unknown",
        "(NULL::boolean) IS UNKNOWN",
        query_sql=(
            f"SELECT ((NULL::boolean) IS UNKNOWN) AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER}"
        ),
    ),
    OracleCase("is_not_unknown", "t.flag IS NOT UNKNOWN"),
    OracleCase("is_distinct_from", "t.n IS DISTINCT FROM 10"),
    OracleCase("is_not_distinct_from", "t.n IS NOT DISTINCT FROM 20"),
    OracleCase("unary_plus", "+t.n"),
    OracleCase("unary_minus", "-t.n"),
    OracleCase("bit_and", "t.n & 7"),
    OracleCase("bit_or", "t.n | 1"),
    OracleCase("bit_xor", "t.n # 7"),
    OracleCase("shift_left", "t.n << 1"),
    OracleCase("shift_right", "t.n2 >> 1"),
    OracleCase("bit_not", "~t.n"),
    OracleCase("md5", "md5('hello')"),
    OracleCase(
        "md5_timestamp",
        "md5(t.ts)",
        pg_query_sql=f"SELECT md5((t.ts)::text) AS value FROM {PROBE_TABLE} t WHERE {TARGET_FILTER} LIMIT 1",
    ),
    OracleCase(
        "md5_timestamptz",
        "md5(t.ts_tz)",
        setup_sql=("SET TIME ZONE 'America/Los_Angeles'",),
        pg_query_sql=(
            f"SELECT md5((t.ts_tz)::text) AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER} LIMIT 1"
        ),
    ),
    OracleCase(
        "sha256",
        "sha256('hello')",
        pg_query_sql=(
            f"SELECT digest('hello', 'sha256') AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER} LIMIT 1"
        ),
    ),
    OracleCase(
        "sha256_timestamp",
        "encode(sha256(t.ts), 'hex')",
        pg_query_sql=(
            f"SELECT encode(digest((t.ts)::text, 'sha256'), 'hex') AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER} LIMIT 1"
        ),
    ),
    OracleCase(
        "sha256_timestamptz",
        "encode(sha256(t.ts_tz), 'hex')",
        setup_sql=("SET TIME ZONE 'America/Los_Angeles'",),
        pg_query_sql=(
            f"SELECT encode(digest((t.ts_tz)::text, 'sha256'), 'hex') AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER} LIMIT 1"
        ),
    ),
    OracleCase(
        "digest_sha256_text",
        "encode(digest('hello', 'sha256'), 'hex')",
    ),
    OracleCase(
        "digest",
        "digest('hello', 'sha256')",
    ),
    OracleCase(
        "digest_sha256_timestamp",
        "encode(digest(t.ts, 'sha256'), 'hex')",
        pg_query_sql=(
            f"SELECT encode(digest((t.ts)::text, 'sha256'), 'hex') AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER} LIMIT 1"
        ),
    ),
    OracleCase(
        "digest_sha256_timestamptz",
        "encode(digest(t.ts_tz, 'sha256'), 'hex')",
        setup_sql=("SET TIME ZONE 'America/Los_Angeles'",),
        pg_query_sql=(
            f"SELECT encode(digest((t.ts_tz)::text, 'sha256'), 'hex') AS value "
            f"FROM {PROBE_TABLE} t WHERE {TARGET_FILTER} LIMIT 1"
        ),
    ),
    OracleCase(
        "digest_sha256_bytea",
        "encode(digest('\\x00ff'::bytea, 'sha256'), 'hex')",
    ),
    OracleCase("encode_hex", "encode('\\xdeadbeef'::bytea, 'hex')"),
    OracleCase("encode_base64", "encode('hello'::bytea, 'base64')"),
    OracleCase("encode_escape", "encode('\\x5c001f207fff'::bytea, 'escape')"),
    OracleCase(
        "decode_hex_roundtrip",
        "encode(decode('deadbeef', 'hex'), 'hex')",
    ),
    OracleCase(
        "decode",
        "decode('deadbeef', 'hex')",
    ),
    OracleCase(
        "decode_base64_roundtrip",
        "encode(decode('aGVsbG8=', 'base64'), 'hex')",
    ),
    OracleCase(
        "decode_escape_roundtrip",
        r"""encode(decode('\\\000\037 \177\377', 'escape'), 'hex')""",
    ),
    OracleCase("decode_hex_invalid_digit", "encode(decode('zz', 'hex'), 'hex')"),
    OracleCase(
        "decode_hex_prefixed_input",
        r"""encode(decode('\x61', 'hex'), 'hex')""",
    ),
    OracleCase("decode_hex_odd_digits", "encode(decode('f', 'hex'), 'hex')"),
    OracleCase(
        "decode_base64_invalid_symbol",
        "encode(decode('***', 'base64'), 'hex')",
    ),
    OracleCase(
        "decode_base64_invalid_end",
        "encode(decode('QQ=', 'base64'), 'hex')",
    ),
    OracleCase(
        "decode_base64_noncanonical_suffix",
        "encode(decode('QQ=Q', 'base64'), 'hex')",
    ),
    OracleCase(
        "decode_base64_unexpected_padding",
        "encode(decode('Q=Q=', 'base64'), 'hex')",
    ),
    OracleCase("encode_unknown_encoding", "encode('abc'::bytea, 'bogus')"),
    OracleCase(
        "encode_unknown_encoding_padded",
        "encode('abc'::bytea, ' hex ')",
    ),
    OracleCase(
        "decode_unknown_encoding_padded",
        "encode(decode('616263', ' hex '), 'hex')",
    ),
    OracleCase(
        "decode_unknown_encoding_base64_padded",
        "encode(decode('***', ' BASE64 '), 'hex')",
    ),
    OracleCase(
        "digest_unknown_algorithm",
        "encode(digest('abc', 'bogus'), 'hex')",
    ),
    OracleCase(
        "digest_unknown_algorithm_padded",
        "encode(digest('abc', ' sha256 '), 'hex')",
    ),
    OracleCase(
        "regex_match",
        "'AbC' ~ 'bC'",
        explain_policy="skip",
        tags=("phase2_string_regex",),
    ),
    OracleCase(
        "regex_imatch",
        "'AbC' ~* 'ab'",
        explain_policy="skip",
        tags=("phase2_string_regex",),
    ),
    OracleCase(
        "regex_not_match",
        "'hello' !~ 'zz'",
        explain_policy="skip",
        tags=("phase2_string_regex",),
    ),
    OracleCase(
        "regex_not_imatch",
        "'AbC' !~* 'zz'",
        explain_policy="skip",
        tags=("phase2_string_regex",),
    ),
    OracleCase("regex_bad_pattern", "'abc' ~ '(['"),
    OracleCase(
        "regexp_replace",
        "regexp_replace('hello hello', 'hello', 'hi', 'g')",
    ),
    OracleCase(
        "regexp_replace_bad_pattern",
        "regexp_replace('abc', '([', 'x')",
    ),
    OracleCase(
        "regexp_replace_bad_flag",
        "regexp_replace('abc', 'a', 'x', 'z')",
    ),
    OracleCase(
        "regexp_replace_null_replacement",
        "regexp_replace('abc', '([', NULL::text)",
    ),
    OracleCase(
        "regexp_replace_null_flags",
        "regexp_replace('abc', '([', 'x', NULL::text)",
    ),
    OracleCase(
        "regexp_replace_extended_flag",
        "regexp_replace('Ab', 'a b', 'X', 'ix')",
    ),
    OracleCase(
        "regexp_matches",
        r"regexp_matches('abc123xyz', '(\d+)')",
        explain_policy="skip",
    ),
    OracleCase("regexp_match", r"regexp_match('abc123xyz', '(\d+)')"),
    OracleCase(
        "regexp_matches_bad_flag",
        r"regexp_matches('abc', 'a', 'z')",
        explain_policy="skip",
    ),
    OracleCase(
        "regexp_split_to_array",
        "regexp_split_to_array('a,b,c', ',')",
    ),
    OracleCase(
        "regexp_split_to_array_bad_flag",
        "regexp_split_to_array('a,b', ',', 'z')",
    ),
    OracleCase(
        "regexp_split_to_array_null_pattern",
        "regexp_split_to_array('a,b', NULL::text)",
    ),
    OracleCase(
        "regexp_split_to_array_null_flags",
        "regexp_split_to_array('a,b', '([', NULL::text)",
    ),
    OracleCase(
        "regexp_split_to_array_extended_flag",
        "regexp_split_to_array('Ab', 'a b', 'ix')",
    ),
    OracleCase(
        "regexp_split_to_array_global_flag",
        "regexp_split_to_array('ab', 'a b', 'g')",
    ),
    OracleCase("regexp_replace_multiline", "regexp_replace('alpha beta', '^beta$', 'X', 'm')"),
    OracleCase("regexp_split_to_array_words", "regexp_split_to_array('alpha beta', ' +')"),
    OracleCase("array_length", "array_length(array[1,2,3], 1)"),
    OracleCase("array_upper", "array_upper(array[1,2,3], 1)"),
    OracleCase("array_lower", "array_lower(array[1,2,3], 1)"),
    OracleCase("cardinality", "cardinality(array[1,2,3])"),
    OracleCase("array_position_null", "array_position(array[1,2,null,2], null)"),
    OracleCase("array_cat", "array_cat(array[1,2], array[3])"),
    OracleCase("array_append", "array_append(array[1,2], 3)"),
    OracleCase("array_prepend", "array_prepend(1, array[2,3])"),
    OracleCase("array_remove", "array_remove(array[1,2,null,2], 2)"),
    OracleCase(
        "array_to_string",
        "array_to_string(array['a', null, 'b'], ',', '*')",
    ),
    OracleCase(
        "array_to_string_null_delimiter",
        "array_to_string(array['a', null, 'b'], NULL::text)",
    ),
    OracleCase("string_to_array", "string_to_array('a,b,c', ',')"),
    OracleCase("json_array_length", r"""json_array_length('[1,2,3]'::json)"""),
    OracleCase("jsonb_array_length", r"""jsonb_array_length('[1,2,3]'::jsonb)"""),
    OracleCase("json_typeof", r"""json_typeof('{"a":1}'::json)"""),
    OracleCase("jsonb_typeof", r"""jsonb_typeof('[1,2,3]'::jsonb)"""),
    OracleCase(
        "json_build_object",
        "json_build_object('b', 2, 'a', 1)",
    ),
    OracleCase(
        "jsonb_build_object",
        "jsonb_build_object('b', 2, 'a', 1)",
    ),
    OracleCase("json_build_array", "json_build_array(1, 'hello')"),
    OracleCase("jsonb_build_array", "jsonb_build_array(1, 'hello')"),
    OracleCase(
        "json_extract_path",
        r"""json_extract_path('{"b":2,"a":{"y":2,"x":1}}'::json, 'a')""",
    ),
    OracleCase(
        "jsonb_extract_path",
        r"""jsonb_extract_path('{"b":2,"a":{"y":2,"x":1}}'::jsonb, 'a')""",
    ),
    OracleCase(
        "json_extract_path_text",
        r"""json_extract_path_text('{"b":2,"a":{"y":2,"x":1}}'::json, 'a')""",
    ),
    OracleCase(
        "jsonb_extract_path_text",
        r"""jsonb_extract_path_text('{"b":2,"a":{"y":2,"x":1}}'::jsonb, 'a')""",
    ),
    OracleCase(
        "jsonb_pretty",
        r"""jsonb_pretty('{"b":2,"a":{"y":2,"x":1}}'::jsonb)""",
    ),
    OracleCase("to_json", "to_json(t.n)"),
    OracleCase("to_jsonb", "to_jsonb(t.n)"),
    OracleCase("row_to_json", "row_to_json(row(t.id, t.n))"),
    OracleCase(
        "jsonb_set",
        r"""jsonb_set('{"b":2,"a":{"y":2,"x":1}}'::jsonb, '{a,x}', '42'::jsonb)""",
    ),
    OracleCase(
        "jsonb_exists",
        r"""jsonb_exists('{"b":2,"a":{"y":2,"x":1}}'::jsonb, 'a')""",
    ),
    OracleCase(
        "jsonb_exists_any",
        r"""jsonb_exists_any('{"b":2,"a":{"y":2,"x":1}}'::jsonb, array['z','a'])""",
    ),
    OracleCase(
        "jsonb_exists_all",
        r"""jsonb_exists_all('{"b":2,"a":{"y":2,"x":1}}'::jsonb, array['a'])""",
    ),
    OracleCase(
        "json_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::json -> 'a'""",
        explain_policy="skip",
    ),
    OracleCase(
        "json_long_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::json ->> 'a'""",
        explain_policy="skip",
    ),
    OracleCase(
        "json_hash_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::json #> '{a}'""",
        explain_policy="skip",
    ),
    OracleCase(
        "json_hash_long_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::json #>> '{a}'""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::jsonb -> 'a'""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_long_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::jsonb ->> 'a'""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_hash_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::jsonb #> '{a}'""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_hash_long_arrow",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::jsonb #>> '{a}'""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_hash_minus",
        r"'[1,2,3]'::jsonb #- '{1}'",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_contains",
        r"""'{"a":{"x":1,"y":2},"b":2}'::jsonb @> '{"a":{"x":1}}'::jsonb""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_contained_by",
        r"""'{"a":{"x":1}}'::jsonb <@ '{"a":{"x":1,"y":2},"b":2}'::jsonb""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_exists_op",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::jsonb ? 'a'""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_exists_any_op",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::jsonb ?| array['z','a']""",
        explain_policy="skip",
    ),
    OracleCase(
        "jsonb_exists_all_op",
        r"""'{"b":2,"a":{"y":2,"x":1}}'::jsonb ?& array['a']""",
        explain_policy="skip",
    ),
    OracleCase(
        "random_in_range",
        "random() in [0,1)",
        query_sql="WITH sample AS (SELECT random() AS r) SELECT (r >= 0.0 AND r < 1.0) AS value FROM sample",
        explain_policy="skip",
    ),
    OracleCase(
        "clock_timestamp_non_null",
        "clock_timestamp() IS NOT NULL",
        query_sql="SELECT (clock_timestamp() IS NOT NULL) AS value",
        explain_policy="skip",
    ),
    OracleCase(
        "gen_random_uuid_non_null",
        "gen_random_uuid() IS NOT NULL",
        query_sql=(
            "WITH sample AS (SELECT gen_random_uuid() AS v) "
            "SELECT (pg_typeof(v)::text = 'uuid' AND v IS NOT NULL) AS value FROM sample"
        ),
        explain_policy="skip",
    ),
    OracleCase(
        "uuid_generate_v4_non_null",
        "uuid_generate_v4() IS NOT NULL",
        query_sql=(
            "WITH sample AS (SELECT uuid_generate_v4() AS v) "
            "SELECT (pg_typeof(v)::text = 'uuid' AND v IS NOT NULL) AS value FROM sample"
        ),
        pg_query_sql=(
            "WITH sample AS (SELECT gen_random_uuid() AS v) "
            "SELECT (pg_typeof(v)::text = 'uuid' AND v IS NOT NULL) AS value FROM sample"
        ),
        explain_policy="skip",
    ),
    OracleCase(
        "uuidv7_non_null",
        "uuidv7() IS NOT NULL",
        query_sql=(
            "WITH sample AS (SELECT uuidv7() AS v) "
            "SELECT (pg_typeof(v)::text = 'uuid' AND v IS NOT NULL) AS value FROM sample"
        ),
        explain_policy="skip",
    ),
    OracleCase(
        "count_star",
        "count(*)",
        query_sql=f"SELECT count(*) AS value FROM {PROBE_TABLE} t WHERE t.n >= 20",
    ),
    OracleCase(
        "sum_int",
        "sum(t.n)",
        query_sql=f"SELECT sum(t.n) AS value FROM {PROBE_TABLE} t WHERE t.n >= 20",
    ),
    OracleCase(
        "avg_float8",
        "avg(t.f8)",
        query_sql=f"SELECT avg(t.f8) AS value FROM {PROBE_TABLE} t WHERE t.n >= 20",
    ),
    OracleCase(
        "min_int",
        "min(t.n)",
        query_sql=f"SELECT min(t.n) AS value FROM {PROBE_TABLE} t WHERE t.n >= 20",
    ),
    OracleCase(
        "max_int",
        "max(t.n)",
        query_sql=f"SELECT max(t.n) AS value FROM {PROBE_TABLE} t WHERE t.n >= 20",
    ),
    OracleCase(
        "bool_and",
        "bool_and(t.flag)",
        query_sql=f"SELECT bool_and(t.flag) AS value FROM {PROBE_TABLE} t WHERE t.n >= 10",
    ),
    OracleCase(
        "bool_or",
        "bool_or(t.flag)",
        query_sql=f"SELECT bool_or(t.flag) AS value FROM {PROBE_TABLE} t WHERE t.n >= 10",
    ),
    OracleCase(
        "every",
        "every(t.flag)",
        query_sql=f"SELECT every(t.flag) AS value FROM {PROBE_TABLE} t WHERE t.n >= 10",
    ),
    OracleCase("coalesce", "coalesce(t.null_txt, t.txt2, 'fallback')"),
    OracleCase("nullif", "nullif(t.txt2, t.txt2)"),
    OracleCase("cast_to_text", "cast(t.n as text)"),
    OracleCase(
        "cast_identity",
        "cast(date_part('month', t.ts) as double precision)",
    ),

    # --- Edge cases: NULL inputs ---
    # Functions that accept nullable args must propagate NULL correctly.
    OracleCase("length_null", "length(t.null_txt)"),
    OracleCase("lower_null", "lower(t.null_txt)"),
    OracleCase("upper_null", "upper(t.null_txt)"),
    OracleCase("abs_null", "abs(t.null_int)"),
    OracleCase("sign_null", "sign(t.null_f8)"),
    OracleCase("sqrt_null", "sqrt(t.null_f8)"),
    OracleCase("array_cat_null_left", "array_cat(NULL::int[], array[1,2])"),
    OracleCase("array_cat_null_right", "array_cat(array[1,2], NULL::int[])"),
    OracleCase("array_append_null_elem", "array_append(array[1,2], NULL::int)"),
    OracleCase("array_prepend_null_elem", "array_prepend(NULL::int, array[2,3])"),
    OracleCase(
        "array_to_string_null_array",
        "array_to_string(NULL::text[], ',')",
    ),
    OracleCase("jsonb_typeof_null", "jsonb_typeof(NULL::jsonb)"),
    OracleCase("date_part_null", "date_part('month', t.null_ts)"),
    OracleCase("date_trunc_null", "date_trunc('hour', t.null_ts)"),
    OracleCase("to_char_null", "to_char(t.null_ts, 'YYYY-MM-DD')"),
    OracleCase("coalesce_all_null", "coalesce(t.null_txt, t.null_txt)"),
    OracleCase("nullif_null_left", "nullif(t.null_txt, 'x')"),
    OracleCase("lpad_null_str", "lpad(t.null_txt, 5, '0')"),
    OracleCase("rpad_null_str", "rpad(t.null_txt, 5, '0')"),
    OracleCase("btrim_null_chars", "btrim('xydb9xy', NULL::text)"),
    OracleCase("ltrim_null_chars", "ltrim('xydb9', NULL::text)"),
    OracleCase("rtrim_null_chars", "rtrim('db9xy', NULL::text)"),
    OracleCase("lpad_null_fill", "lpad('db9', 5, NULL::text)"),
    OracleCase("rpad_null_fill", "rpad('db9', 5, NULL::text)"),
    OracleCase("replace_null_from", "replace('banana', NULL::text, 'X')"),
    OracleCase("replace_null_to", "replace('banana', 'na', NULL::text)"),
    OracleCase("translate_null_from", "translate('12345', NULL::text, 'ax')"),
    OracleCase("translate_null_to", "translate('12345', '143', NULL::text)"),
    OracleCase("strpos_null_needle", "strpos('banana', NULL::text)"),
    OracleCase("position_null_needle", "position(NULL::text in 'banana')"),
    OracleCase("split_part_null", "split_part(t.null_txt, ',', 1)"),
    OracleCase("regexp_replace_null", "regexp_replace(t.null_txt, 'x', 'y', 'g')"),
    OracleCase(
        "regexp_split_to_array_null",
        "regexp_split_to_array(t.null_txt, ',')",
    ),

    # --- Edge cases: domain errors that should match PostgreSQL ---
    OracleCase("ln_zero", "ln(0.0::double precision)"),
    OracleCase("log_zero", "log(0.0::double precision)"),
    # power(0, 0) → 1.0
    OracleCase("power_zero_zero", "power(0.0::double precision, 0.0::double precision)"),
    OracleCase("asin_out_of_domain", "asin(2.0::double precision)"),
    OracleCase("acos_out_of_domain", "acos(-2.0::double precision)"),
    # width_bucket count = 0 → PG18 2201G invalid_argument_for_width_bucket_function.
    OracleCase(
        "width_bucket_zero_count",
        "width_bucket(5.0::double precision, 0.0::double precision, 10.0::double precision, 0)",
    ),

    # --- Edge cases: string boundary values ---
    OracleCase("lpad_truncate", "lpad('hello world', 3)"),
    OracleCase("rpad_truncate", "rpad('hello world', 3)"),
    OracleCase("overlay_beyond_end", "overlay('abc' placing 'XY' from 3 for 10)"),
    OracleCase("split_part_out_of_range", "split_part('a,b,c', ',', 99)"),
    OracleCase("split_part_negative_index", "split_part('a,b,c,d', ',', -2)"),
    OracleCase("split_part_zero_index", "split_part('a,b,c', ',', 0)"),
    OracleCase("split_part_empty_delimiter", "split_part('abc', '', 1)"),
    OracleCase("format_no_args", "format('literal%%text')"),
    OracleCase("regexp_replace_no_flags", "regexp_replace('aaa', 'a', 'x')"),

    # --- Edge cases: date/time boundary values ---
    OracleCase("date_trunc_day", "date_trunc('day', t.ts)"),
    OracleCase("date_trunc_month", "date_trunc('month', t.ts)"),
    OracleCase("date_trunc_year", "date_trunc('year', t.ts)"),
    OracleCase(
        "date_trunc_week",
        "date_trunc('week', t.ts)",
        pg18_policy="parity_only",
        compat_note="DB9 does not support DATE_TRUNC('week'); PG18 does.",
    ),
    OracleCase(
        "date_trunc_quarter",
        "date_trunc('quarter', t.ts)",
        pg18_policy="parity_only",
        compat_note="DB9 does not support DATE_TRUNC('quarter'); PG18 does.",
    ),
    OracleCase(
        "date_trunc_microseconds",
        "date_trunc('microseconds', t.ts)",
        pg18_policy="parity_only",
        compat_note="DB9 does not support DATE_TRUNC('microseconds'); PG18 does.",
    ),
    OracleCase("age_same_ts", "age(t.ts, t.ts)"),
    OracleCase(
        "make_interval_negative",
        "make_interval(-1, -2, 0, -3, -4, -5, -6.5)",
        explain_policy="skip",
        pg18_policy="parity_only",
        compat_note="DB9 interval text formatting differs slightly from PG18 for negative year pluralization.",
    ),
    OracleCase(
        "make_date_year_1",
        "make_date(1, 1, 1)",
    ),

    # --- Edge cases: math boundary values ---
    OracleCase(
        "mod_zero_divisor_int",
        "mod(5, 0)",
    ),
    OracleCase("width_bucket_below_low", "width_bucket(-5.0::double precision, 0.0, 10.0, 5)"),
    OracleCase("width_bucket_above_high", "width_bucket(15.0::double precision, 0.0, 10.0, 5)"),
    OracleCase("pi_precision", "pi() = pi()"),
    OracleCase("exp_large", "exp(100.0::double precision)"),
    # --- Edge cases: array functions ---
    OracleCase("array_remove_null_elem", "array_remove(array[1, NULL::int, 2], NULL::int)"),
    OracleCase("array_remove_nonexistent", "array_remove(array[1,2,3], 9)"),
    OracleCase("array_position_not_found", "array_position(array[1,2,3], 9)"),
    OracleCase("string_to_array_null_delimiter", "string_to_array('abc', NULL)"),
    OracleCase("string_to_array_empty", "string_to_array('', ',')"),

    # --- Edge cases: JSON ---
    OracleCase("jsonb_set_nonexistent_path", r"""jsonb_set('{"a":1}'::jsonb, '{b}', '2'::jsonb)"""),
    OracleCase(
        "jsonb_set_nonexistent_path_no_create",
        r"""jsonb_set('{"a":1}'::jsonb, '{b}', '2'::jsonb, false)""",
    ),
    OracleCase("jsonb_set_null_value", r"""jsonb_set('{"a":1}'::jsonb, '{a}', 'null'::jsonb)"""),
    OracleCase(
        "json_set_existing_key",
        r"""json_set('{"a":1}'::jsonb, '{a}', '42'::jsonb)""",
        pg18_policy="parity_only",
        compat_note="json_set is a DB9 extension; PG18 has no matching builtin.",
    ),
    OracleCase("json_build_object_empty", "json_build_object()"),
    OracleCase("jsonb_typeof_null_value", r"""jsonb_typeof('{"a": null}'::jsonb -> 'a')"""),

    # --- Transaction visibility: write then read in same txn ---
    # DB9 must NOT push down the SELECT when the table was written in the txn.
    # Result must match pg18 (the freshly written row must be visible).
    OracleCase(
        "txn_write_then_read_sees_own_write",
        "n = 999",
        query_sql=f"SELECT (COUNT(*) = 1)::boolean AS value FROM {PROBE_TABLE} WHERE n = 999",
        setup_sql=(
            f"INSERT INTO {PROBE_TABLE}(id, n, txt, txt2, txt_u, txt_date, null_txt, flag, n2, f8, ts, ts2, ts_tz) "
            f"VALUES (999, 999, 'txn_test', 'tt', 'tt', '2024-01-01', NULL, true, 999, 1.0, "
            f"'2024-01-01 00:00:00', '2024-01-01 00:00:00', '2024-01-01 00:00:00+00')",
        ),
        transactional=True,
        explain_policy="skip",
    ),
]


PROBE_SETUP = [
    # DB9 runtime replay may rebuild this probe table multiple times against the
    # same DSN (for pushdown off/on connections). Drop the secondary index
    # explicitly first so repeated setup remains idempotent on the current pair.
    f"DROP INDEX IF EXISTS {PROBE_TABLE}_n_idx",
    f"DROP TABLE IF EXISTS {PROBE_TABLE}",
    f"""
    CREATE TABLE {PROBE_TABLE}(
        id INT PRIMARY KEY,
        n INT NOT NULL,
        txt TEXT NOT NULL,
        txt2 TEXT NOT NULL,
        txt_u TEXT NOT NULL,
        txt_date TEXT NOT NULL,
        null_txt TEXT,
        flag BOOLEAN NOT NULL,
        n2 BIGINT NOT NULL,
        f8 DOUBLE PRECISION NOT NULL,
        ts TIMESTAMP NOT NULL,
        ts2 TIMESTAMP NOT NULL,
        ts_tz TIMESTAMPTZ NOT NULL,
        null_int INT,
        null_f8 DOUBLE PRECISION,
        null_ts TIMESTAMP
    )
    """,
    f"CREATE INDEX IF NOT EXISTS {PROBE_TABLE}_n_idx ON {PROBE_TABLE}(n)",
    f"""
    INSERT INTO {PROBE_TABLE}(
        id, n, txt, txt2, txt_u, txt_date, null_txt, flag, n2, f8, ts, ts2, ts_tz,
        null_int, null_f8, null_ts
    ) VALUES
        (1, 10, 'first', 'aa', 'hé你', '2024-01-01', 'x', true, 66, 3.5,
         '2024-01-01 00:00:00', '2024-01-15 00:00:00', '2024-01-01 00:00:00+00',
         NULL, NULL, NULL),
        (2, 20, 'AbC', 'xy', 'hé你', '2024-03-15', NULL, true, 65, 12.3456,
         '2024-03-15 12:34:56.789', '2024-05-01 08:00:00', '2024-03-15 12:34:56+00',
         NULL, NULL, NULL),
        (3, 30, 'last', 'zz', 'hé你', '2024-12-31', NULL, false, 67, 99.9,
         '2024-12-31 23:59:59', '2025-01-01 00:00:00', '2024-12-31 23:59:59+00',
         NULL, NULL, NULL)
    """,
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Validate DB9 PushdownSafe builtins against PostgreSQL 18.3 and "
            "ensure EXPLAIN shows DB9 Cop under pushdown."
        )
    )
    parser.add_argument("--db9-dsn", default=DEFAULT_DB9_DSN)
    parser.add_argument("--pg-dsn", default=DEFAULT_PG_DSN)
    parser.add_argument(
        "--report-json",
        default="",
        help="Optional path for the JSON report. Defaults to test-reports/<timestamp>.json",
    )
    parser.add_argument(
        "--case",
        action="append",
        default=[],
        help="Run only OracleCase entries whose name exactly matches this value. Repeatable.",
    )
    parser.add_argument(
        "--case-prefix",
        action="append",
        default=[],
        help="Run only OracleCase entries whose name starts with this prefix. Repeatable.",
    )
    parser.add_argument(
        "--tag",
        action="append",
        default=[],
        help="Run only OracleCase entries that carry this tag. Repeatable.",
    )
    parser.add_argument("--verbose", action="store_true")
    return parser.parse_args()


def case_selected(
    case: OracleCase,
    *,
    names: set[str],
    prefixes: tuple[str, ...],
    tags: set[str],
) -> bool:
    if not names and not prefixes and not tags:
        return True
    if case.name in names:
        return True
    if any(case.name == prefix or case.name.startswith(prefix) for prefix in prefixes):
        return True
    return any(tag in tags for tag in case.tags)


def connect(dsn: str) -> psycopg.Connection[Any]:
    return psycopg.connect(dsn, autocommit=True)


def configure_connection(
    conn: psycopg.Connection[Any],
    *,
    pushdown: str | None,
    case: OracleCase | None = None,
) -> None:
    with conn.cursor() as cur:
        cur.execute("SET TIME ZONE 'UTC'")
        if pushdown is not None:
            cur.execute(f"SET db9.enable_cop_pushdown = {pushdown}")
            if case is not None:
                for setting_name, setting_value in case.db9_settings:
                    cur.execute(f"SET {setting_name} = {setting_value}")


def prepare_probe(conn: psycopg.Connection[Any]) -> None:
    with conn.cursor() as cur:
        for statement in PROBE_SETUP:
            cur.execute(statement)


def prepare_pg18_extensions(conn: psycopg.Connection[Any]) -> None:
    with conn.cursor() as cur:
        for statement in PG18_EXTENSION_SETUP:
            cur.execute(statement)


def query_sql(case: OracleCase, *, for_pg: bool) -> str:
    if for_pg and case.pg_query_sql is not None:
        return case.pg_query_sql
    if case.query_sql is not None:
        return case.query_sql
    return f"""
    SELECT ({case.expr}) AS value
    FROM {PROBE_TABLE} t
    WHERE {TARGET_FILTER}
    LIMIT 1
    """


def normalize_select_sql(case: OracleCase, *, for_pg: bool) -> str:
    return f"""
    WITH probe AS (
        {query_sql(case, for_pg=for_pg)}
    )
    SELECT
        pg_typeof(value)::text AS value_type,
        CASE
            WHEN value IS NULL THEN NULL
            ELSE value::text
        END AS value_text,
        value
    FROM probe
    """


def explain_sql(case: OracleCase) -> str:
    return f"""
    EXPLAIN
    {query_sql(case, for_pg=False)}
    """


def normalize_error(exc: Exception) -> QueryOutcome:
    sqlstate = getattr(exc, "sqlstate", None)
    message = str(exc).strip().splitlines()[0] if str(exc).strip() else exc.__class__.__name__
    return QueryOutcome(status="error", sqlstate=sqlstate, message=message)


def execute_normalized_select(
    cur: psycopg.Cursor[Any],
    case: OracleCase,
    *,
    for_pg: bool,
) -> QueryOutcome:
    cur.execute(normalize_select_sql(case, for_pg=for_pg))
    row = cur.fetchone()
    if row is None:
        return QueryOutcome(status="error", message="query returned no rows")

    return QueryOutcome(
        status="ok",
        value_type=row[0],
        value_text=normalize_value(row[0], row[1], row[2]),
    )


def run_outcome(conn: psycopg.Connection[Any], case: OracleCase, *, pushdown: str | None) -> QueryOutcome:
    configure_connection(conn, pushdown=pushdown, case=case)
    for_pg = pushdown is None
    if not case.transactional and not case.setup_sql:
        try:
            with conn.cursor() as cur:
                return execute_normalized_select(cur, case, for_pg=for_pg)
        except Exception as exc:  # noqa: BLE001
            return normalize_error(exc)

    previous_autocommit = conn.autocommit
    try:
        if previous_autocommit:
            conn.autocommit = False
        with conn.cursor() as cur:
            for statement in case.setup_sql:
                cur.execute(statement)
            outcome = execute_normalized_select(cur, case, for_pg=for_pg)
        conn.rollback()
        return outcome
    except Exception as exc:  # noqa: BLE001
        try:
            conn.rollback()
        except Exception:  # noqa: BLE001
            pass
        return normalize_error(exc)
    finally:
        if previous_autocommit:
            conn.autocommit = True


def normalize_value(value_type: str, value_text: str | None, raw_value: Any) -> str:
    if raw_value is None:
        return "<NULL>"
    if isinstance(raw_value, float):
        text = f"{raw_value:.12f}".rstrip("0").rstrip(".")
        return text or "0"
    if isinstance(raw_value, datetime):
        if raw_value.tzinfo is not None:
            return raw_value.astimezone(timezone.utc).isoformat(sep=" ", timespec="microseconds")
        return raw_value.isoformat(sep=" ", timespec="microseconds")
    if isinstance(raw_value, date) and not isinstance(raw_value, datetime):
        return raw_value.isoformat()
    if isinstance(raw_value, time):
        return raw_value.isoformat(timespec="microseconds")
    if isinstance(raw_value, (int, str, bool)):
        return str(raw_value)
    if value_text is not None:
        return value_text
    return str(raw_value)


def run_explain(
    conn: psycopg.Connection[Any],
    case: OracleCase,
    *,
    pushdown: str | None,
) -> tuple[bool, str | None]:
    configure_connection(conn, pushdown=pushdown, case=case)
    try:
        with conn.cursor() as cur:
            cur.execute(explain_sql(case))
            lines = [row[0] for row in cur.fetchall()]
    except Exception as exc:  # noqa: BLE001
        return False, str(exc).strip().splitlines()[0] if str(exc).strip() else exc.__class__.__name__

    text = "\n".join(lines)
    return "DB9 Cop Output: value" in text, None


def outcomes_match(left: QueryOutcome, right: QueryOutcome) -> bool:
    if left.status != right.status:
        return False
    if left.status == "ok":
        return left.value_type == right.value_type and left.value_text == right.value_text
    return left.sqlstate == right.sqlstate


def requires_explain_validation(case: OracleCase, *outcomes: QueryOutcome) -> bool:
    if case.explain_policy == "skip":
        return False
    if case.pg18_policy == "parity_only":
        return all(outcome.status == "ok" for outcome in outcomes[:2])
    return all(outcome.status == "ok" for outcome in outcomes)


def default_report_path() -> Path:
    root = Path(__file__).resolve().parents[1]
    report_dir = root / "test-reports"
    report_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    return report_dir / f"pg18-pushdown-oracle-{stamp}.json"


def evaluate_case(
    case: OracleCase,
    db9_on: psycopg.Connection[Any],
    db9_off: psycopg.Connection[Any],
    pg18: psycopg.Connection[Any],
) -> CaseResult:
    db9_on_outcome = run_outcome(db9_on, case, pushdown="on")
    db9_off_outcome = run_outcome(db9_off, case, pushdown="off")
    pg18_outcome = run_outcome(pg18, case, pushdown=None)
    if case.explain_policy == "skip":
        explain_on_has_cop, explain_on_error = False, None
        explain_off_has_cop, explain_off_error = False, None
    else:
        explain_on_has_cop, explain_on_error = run_explain(db9_on, case, pushdown="on")
        explain_off_has_cop, explain_off_error = run_explain(db9_off, case, pushdown="off")

    issues: list[str] = []
    notes: list[str] = []
    if not outcomes_match(db9_on_outcome, db9_off_outcome):
        issues.append("db9_on_vs_off_mismatch")
    if not outcomes_match(db9_off_outcome, pg18_outcome):
        if case.pg18_policy == "parity_only":
            notes.append("allowed_db9_vs_pg18_compat_gap")
        else:
            issues.append("db9_vs_pg18_mismatch")
    if requires_explain_validation(case, db9_on_outcome, db9_off_outcome, pg18_outcome):
        if explain_on_error is not None:
            issues.append("explain_on_failed")
        elif not explain_on_has_cop:
            issues.append("missing_db9_cop_on_explain")
        if explain_off_error is not None:
            issues.append("explain_off_failed")
        elif explain_off_has_cop:
            issues.append("pushdown_disabled_but_db9_cop_present")

    return CaseResult(
        name=case.name,
        expr=case.expr,
        db9_on=db9_on_outcome,
        db9_off=db9_off_outcome,
        pg18=pg18_outcome,
        explain_on_has_cop=explain_on_has_cop,
        explain_off_has_cop=explain_off_has_cop,
        explain_on_error=explain_on_error,
        explain_off_error=explain_off_error,
        pg18_policy=case.pg18_policy,
        compat_note=case.compat_note,
        notes=notes,
        issues=issues,
    )


def print_case(result: CaseResult, *, verbose: bool) -> None:
    if not verbose and not result.issues:
        return
    status = "PASS" if not result.issues else "FAIL"
    print(f"[{status}] {result.name}: {result.expr}")
    if result.issues:
        print(f"  issues={','.join(result.issues)}")
        print(f"  db9_on={asdict(result.db9_on)}")
        print(f"  db9_off={asdict(result.db9_off)}")
        print(f"  pg18={asdict(result.pg18)}")
        print(
            f"  explain_on_has_cop={result.explain_on_has_cop} "
            f"explain_off_has_cop={result.explain_off_has_cop}"
        )
        if result.explain_on_error:
            print(f"  explain_on_error={result.explain_on_error}")
        if result.explain_off_error:
            print(f"  explain_off_error={result.explain_off_error}")
    elif verbose and result.notes:
        print(f"  notes={','.join(result.notes)}")
        if result.compat_note:
            print(f"  compat_note={result.compat_note}")


def main() -> int:
    args = parse_args()
    report_path = Path(args.report_json) if args.report_json else default_report_path()
    selected_cases = [
        case
        for case in CASES
        if case_selected(
            case,
            names=set(args.case),
            prefixes=tuple(args.case_prefix),
            tags=set(args.tag),
        )
    ]
    if not selected_cases:
        print("no OracleCase entries matched the requested filters", file=sys.stderr)
        return 1

    with connect(args.db9_dsn) as db9_on, connect(args.db9_dsn) as db9_off, connect(args.pg_dsn) as pg18:
        prepare_probe(db9_on)
        prepare_pg18_extensions(pg18)
        prepare_probe(pg18)

        results = [evaluate_case(case, db9_on, db9_off, pg18) for case in selected_cases]

    failures = [result for result in results if result.issues]
    allowed_pg18_gaps = [result for result in results if result.notes]
    for result in results:
        print_case(result, verbose=args.verbose)

    report = {
        "db9_dsn": args.db9_dsn,
        "pg_dsn": args.pg_dsn,
        "selected_case_names": [case.name for case in selected_cases],
        "case_count": len(results),
        "failure_count": len(failures),
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "results": [asdict(result) for result in results],
    }
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")

    print("")
    print(
        f"cases={len(results)} failures={len(failures)} "
        f"allowed_pg18_gaps={len(allowed_pg18_gaps)} report={report_path}"
    )
    if failures:
        print("failing_cases=" + ", ".join(result.name for result in failures))
        return 1

    print("all cases matched DB9 on/off and PostgreSQL 18.3, with DB9 Cop present only when enabled")
    return 0


if __name__ == "__main__":
    sys.exit(main())
