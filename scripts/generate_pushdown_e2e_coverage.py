#!/usr/bin/env python3
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST_PATH = REPO_ROOT / "e2e" / "pushdown_coverage_manifest.json"
POINT_ACCESS = "DB9 Cop Access: point (20)"
SELECT_FILTER = "n = 20"


@dataclass(frozen=True)
class Case:
    name: str
    expr: str
    expected_sql: str

    def to_dict(self) -> dict[str, str]:
        return {
            "name": self.name,
            "expr": self.expr,
            "expected_sql": self.expected_sql,
        }


@dataclass(frozen=True)
class Suite:
    name: str
    comment: str
    base_table: str
    setup_sql: tuple[str, ...]
    cases: tuple[Case, ...]
    update_strategy: str = "direct_filter"

    def to_dict(self) -> dict[str, object]:
        return {
            "name": self.name,
            "comment": self.comment,
            "base_table": self.base_table,
            "setup_sql": list(self.setup_sql),
            "cases": [case.to_dict() for case in self.cases],
            "update_strategy": self.update_strategy,
        }


# Every case here must be fully pushed; the ORM E2E tests assert that the
# expression alias appears in `DB9 Cop Output`.
SUITES: tuple[Suite, ...] = (
    Suite(
        name="operator",
        comment=(
            "Admitted logical, comparison, predicate, unary, LIKE, bitwise, and shift operators "
            "on the exact pair."
        ),
        base_table="pushdown_operator_rows",
        setup_sql=(
            """
            CREATE TABLE {schema}.pushdown_operator_rows (
                id INTEGER PRIMARY KEY,
                n INTEGER NOT NULL,
                n2 BIGINT NOT NULL,
                like_txt TEXT NOT NULL,
                ilike_txt TEXT NOT NULL,
                flag BOOLEAN NOT NULL,
                maybe_flag BOOLEAN,
                marker TEXT
            )
            """.strip(),
            "CREATE INDEX idx_pushdown_operator_rows_n ON {schema}.pushdown_operator_rows(n)",
            """
            INSERT INTO {schema}.pushdown_operator_rows (id, n, n2, like_txt, ilike_txt, flag, maybe_flag, marker)
            VALUES
                (1, 10, 33, 'A_10', 'tmpAlpha', TRUE, FALSE, NULL),
                (2, 20, 65, 'A_20', 'A%Twenty', TRUE, NULL, NULL),
                (3, 30, 99, 'AB30', 'tmpThirty', FALSE, TRUE, NULL)
            """.strip(),
            "ANALYZE {schema}.pushdown_operator_rows",
        ),
        cases=(
            Case("logical_and", "flag AND (n = 20)", "TRUE"),
            Case("logical_or", "flag OR (n = 99)", "TRUE"),
            Case("logical_not", "NOT flag", "FALSE"),
            Case("eq", "n = 20", "TRUE"),
            Case("not_eq", "n <> 10", "TRUE"),
            Case("lt", "n < 21", "TRUE"),
            Case("lte", "n <= 20", "TRUE"),
            Case("gt", "n > 19", "TRUE"),
            Case("gte", "n >= 20", "TRUE"),
            Case("is_null", "maybe_flag IS NULL", "TRUE"),
            Case("is_not_null", "ilike_txt IS NOT NULL", "TRUE"),
            Case("is_distinct_from", "maybe_flag IS DISTINCT FROM TRUE", "TRUE"),
            Case("is_not_distinct_from", "maybe_flag IS NOT DISTINCT FROM NULL", "TRUE"),
            Case("between", "n BETWEEN 10 AND 20", "TRUE"),
            Case("not_between", "n NOT BETWEEN 21 AND 30", "TRUE"),
            Case("in_list", "n IN (10, 20, 30)", "TRUE"),
            Case("not_in", "n NOT IN (10, 30)", "TRUE"),
            Case("is_true", "flag IS TRUE", "TRUE"),
            Case("is_not_true", "maybe_flag IS NOT TRUE", "TRUE"),
            Case("is_false", "flag IS FALSE", "FALSE"),
            Case("is_not_false", "flag IS NOT FALSE", "TRUE"),
            Case("is_unknown", "maybe_flag IS UNKNOWN", "TRUE"),
            Case("is_not_unknown", "flag IS NOT UNKNOWN", "TRUE"),
            Case("like", "like_txt LIKE 'A!_%' ESCAPE '!'", "TRUE"),
            Case("not_like", "like_txt NOT LIKE 'z%'", "TRUE"),
            Case("ilike", "ilike_txt ILIKE 'a!%%' ESCAPE '!'", "TRUE"),
            Case("not_ilike", "ilike_txt NOT ILIKE 'tmp%'", "TRUE"),
            Case("unary_plus", "+n", "20"),
            Case("unary_minus", "-n", "-20"),
            Case("bitwise_not", "~n", "-21"),
            Case("bitwise_and", "n & 12", "4"),
            Case("bitwise_or", "n | 3", "23"),
            Case("bitwise_xor", "n # 7", "19"),
            Case("shift_left", "n << 1", "40"),
            Case("shift_right", "n >> 2", "5"),
        ),
    ),
    Suite(
        name="math_conditional",
        comment="Admitted math and conditional functions on the exact pair.",
        base_table="pushdown_math_conditional_rows",
        setup_sql=(
            """
            CREATE TABLE {schema}.pushdown_math_conditional_rows (
                id INTEGER PRIMARY KEY,
                n INTEGER NOT NULL,
                maybe_txt TEXT,
                fallback_txt TEXT,
                neg_big BIGINT NOT NULL,
                f8 DOUBLE PRECISION NOT NULL,
                maybe_precision INTEGER,
                cmp_big BIGINT,
                marker TEXT
            )
            """.strip(),
            "CREATE INDEX idx_pushdown_math_conditional_rows_n ON {schema}.pushdown_math_conditional_rows(n)",
            """
            INSERT INTO {schema}.pushdown_math_conditional_rows (
                id, n, maybe_txt, fallback_txt, neg_big, f8, maybe_precision, cmp_big, marker
            )
            VALUES
                (1, 10, 'present', 'fallback-a', -10, 12.34, 1, 10, NULL),
                (2, 20, NULL, 'Alpha', -20, 12.34, NULL, 99, NULL),
                (3, 30, NULL, NULL, -30, 56.78, 2, NULL, NULL)
            """.strip(),
            "ANALYZE {schema}.pushdown_math_conditional_rows",
        ),
        cases=(
            Case("abs", "ABS(neg_big)", "20"),
            Case("round", "ROUND(f8)", "12"),
            Case("trunc", "TRUNC(f8)", "12"),
            Case("coalesce", "COALESCE(maybe_txt, fallback_txt, 'ultimate')", "'Alpha'"),
            Case("nullif", "NULLIF(fallback_txt, maybe_txt)", "'Alpha'"),
        ),
    ),
    Suite(
        name="string_regex_hash",
        comment=(
            "Admitted string, hashing, and decode functions on the exact pair. "
            "Regex predicate operators stay local and are covered by dedicated local-only proofs."
        ),
        base_table="pushdown_string_regex_hash_rows",
        setup_sql=(
            """
            CREATE TABLE {schema}.pushdown_string_regex_hash_rows (
                id INTEGER PRIMARY KEY,
                n INTEGER NOT NULL,
                txt TEXT NOT NULL,
                txt2 TEXT NOT NULL,
                trim_txt TEXT NOT NULL,
                csv_txt TEXT NOT NULL,
                ident_txt TEXT NOT NULL,
                char_code INTEGER NOT NULL,
                null_txt TEXT,
                hex_txt TEXT NOT NULL,
                bytes_val BYTEA NOT NULL,
                marker TEXT
            )
            """.strip(),
            "CREATE INDEX idx_pushdown_string_regex_hash_rows_n ON {schema}.pushdown_string_regex_hash_rows(n)",
            r"""
            INSERT INTO {schema}.pushdown_string_regex_hash_rows (
                id, n, txt, txt2, trim_txt, csv_txt, ident_txt, char_code, null_txt, hex_txt, bytes_val, marker
            )
            VALUES
                (1, 10, 'before value', 'pq', '  before value  ', 'u,v,w', 'plain_name', 66, 'fallback', '7061', '\x7061'::bytea, NULL),
                (2, 20, 'alpha beta', 'xy', '  alpha beta  ', 'aa,bb,cc', 'select', 65, NULL, '6162', '\x6162'::bytea, NULL),
                (3, 30, 'omega zone', 'zz', '  omega zone  ', 'dd,ee,ff', 'mixedCase', 67, 'tail', '7a7a', '\x7a7a'::bytea, NULL)
            """.strip(),
            "ANALYZE {schema}.pushdown_string_regex_hash_rows",
        ),
        cases=(
            Case("length", "length(txt)", "10"),
            Case("char_length", "char_length(txt)", "10"),
            Case("character_length", "character_length(txt)", "10"),
            Case("trim", "trim(trim_txt)", "'alpha beta'"),
            Case("btrim", "btrim(trim_txt)", "'alpha beta'"),
            Case("ltrim", "ltrim(trim_txt)", "'alpha beta  '"),
            Case("rtrim", "rtrim(trim_txt)", "'  alpha beta'"),
            Case("left", "left(txt, 5)", "'alpha'"),
            Case("right", "right(txt, 4)", "'beta'"),
            Case("substring", "substring(txt from 7 for 4)", "'beta'"),
            Case("substr", "substr(txt, 7, 4)", "'beta'"),
            Case("reverse", "reverse(txt2)", "'yx'"),
            Case("ascii", "ascii(txt2)", "120"),
            Case("chr", "chr(char_code)", "'A'"),
            Case("strpos", "strpos(txt, 'b')", "7"),
            Case("strpos_null_needle", "strpos(txt, null_txt)", "NULL"),
            Case("position", "position('b' IN txt)", "7"),
            Case("split_part", "split_part(csv_txt, ',', 2)", "'bb'"),
            Case("md5", "md5(txt2)", "'3e44107170a520582ade522fa73c1d15'"),
            Case("md5_null", "md5(null_txt)", "NULL"),
            Case(
                "sha256",
                "sha256(txt2)",
                "decode('769a4e6d0003189c7e96c5d9b7e810a0d11c3a12832527ec94b0f86d277f51ca', 'hex')",
            ),
            Case("sha256_null", "sha256(null_txt)", "NULL"),
            Case(
                "digest",
                "digest(txt2, 'sha256')",
                "decode('769a4e6d0003189c7e96c5d9b7e810a0d11c3a12832527ec94b0f86d277f51ca', 'hex')",
            ),
            Case("decode", "decode(hex_txt, 'hex')", "decode('6162', 'hex')"),
        ),
    ),
    Suite(
        name="array",
        comment="Admitted array functions on the exact pair. Array containment and overlap operators stay local on the current exact pair.",
        base_table="pushdown_array_rows",
        update_strategy="projected_match_by_id",
        setup_sql=(
            """
            CREATE TABLE {schema}.pushdown_array_rows (
                id INTEGER PRIMARY KEY,
                n INTEGER NOT NULL,
                tags TEXT[] NOT NULL,
                more_tags TEXT[] NOT NULL,
                ints INTEGER[] NOT NULL,
                matrix INTEGER[][] NOT NULL,
                csv_txt TEXT NOT NULL,
                marker TEXT
            )
            """.strip(),
            "CREATE INDEX idx_pushdown_array_rows_n ON {schema}.pushdown_array_rows(n)",
            """
            INSERT INTO {schema}.pushdown_array_rows (id, n, tags, more_tags, ints, matrix, csv_txt, marker)
            VALUES
                (1, 10, ARRAY['alpha', 'beta'], ARRAY['beta', 'gamma'], ARRAY[10, 20], ARRAY[[10, 20], [30, 40]], 'x,y', NULL),
                (2, 20, ARRAY['alpha', 'sql', 'rust'], ARRAY['sql', 'cop'], ARRAY[20, 30, 40], ARRAY[[20, 30], [40, 50]], 'aa,bb,cc', NULL),
                (3, 30, ARRAY['omega', 'zone'], ARRAY['zone'], ARRAY[30, 40], ARRAY[[30, 40], [50, 60]], 'dd,ee', NULL)
            """.strip(),
            "ANALYZE {schema}.pushdown_array_rows",
        ),
        cases=(
            Case("array_length", "array_length(tags, 1)", "3"),
            Case("array_upper", "array_upper(tags, 1)", "3"),
            Case("array_lower", "array_lower(tags, 1)", "1"),
            Case("array_length_dim2", "array_length(matrix, 2)", "2"),
            Case("array_upper_dim2", "array_upper(matrix, 2)", "2"),
            Case("array_lower_dim2", "array_lower(matrix, 2)", "1"),
            Case("cardinality", "cardinality(tags)", "3"),
            Case("cardinality_matrix", "cardinality(matrix)", "4"),
            Case("array_position", "array_position(tags, 'sql')", "2"),
            Case("array_cat", "array_cat(tags, ARRAY['cop'])", "ARRAY['alpha', 'sql', 'rust', 'cop']"),
            Case("array_cat_null_left", "array_cat(NULL::TEXT[], tags)", "ARRAY['alpha', 'sql', 'rust']"),
            Case("array_cat_null_right", "array_cat(tags, NULL::TEXT[])", "ARRAY['alpha', 'sql', 'rust']"),
            Case("array_append", "array_append(tags, 'cop')", "ARRAY['alpha', 'sql', 'rust', 'cop']"),
            Case("array_prepend", "array_prepend('cop', tags)", "ARRAY['cop', 'alpha', 'sql', 'rust']"),
            Case("array_remove", "array_remove(tags, 'sql')", "ARRAY['alpha', 'rust']"),
            Case("string_to_array", "string_to_array(csv_txt, ',')", "ARRAY['aa', 'bb', 'cc']"),
        ),
    ),
)


def render_manifest() -> dict[str, object]:
    return {
        "point_access": POINT_ACCESS,
        "select_filter": SELECT_FILTER,
        "suites": [suite.to_dict() for suite in SUITES],
    }


def main() -> int:
    MANIFEST_PATH.write_text(json.dumps(render_manifest(), indent=2) + "\n")
    print(f"wrote {MANIFEST_PATH.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
