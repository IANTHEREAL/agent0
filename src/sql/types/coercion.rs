//! Type coercion and promotion rules.
//!
//! Two coercion functions exist with intentionally different Text-handling:
//!
//! - **`common_type(a, b)`** — "Text wins": used for UNION/CASE/COALESCE/etc.
//!   where PostgreSQL resolves mixed types to a common supertype.  When one side
//!   is Text/Varchar/Name, the result is Text because every type can be
//!   represented as text.
//!
//! - **`comparison_target_type(a, b)`** — "non-Text wins": used for comparison
//!   operators (`=`, `<`, `>`, etc.) where PostgreSQL coerces the text literal
//!   to the typed side.  For example, `'42' = 42` coerces `'42'` to Int32, not
//!   the integer to text.
//!
//! This difference matches PostgreSQL semantics (see `select_common_type` vs
//! `select_common_typmod` in the PostgreSQL source).

use crate::model::DataType;

pub fn is_oid_alias_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::UserDefined(name)
            if name.eq_ignore_ascii_case("regclass")
                || name.eq_ignore_ascii_case("pg_catalog.regclass")
                || name.eq_ignore_ascii_case("regtype")
                || name.eq_ignore_ascii_case("pg_catalog.regtype")
    )
}

pub fn type_precedence(dt: &DataType) -> i32 {
    match dt {
        DataType::Boolean => 10,
        DataType::Int32 => 20,
        DataType::Oid => 25, // 4-byte OID promotes to Int64, not vice versa
        DataType::Int64 => 30,
        DataType::Float64 => 45,
        DataType::Numeric { .. } => 50,
        DataType::Text => 100,
        DataType::Varchar(_) => 100,
        DataType::Name => 100,
        DataType::Date => 60,
        DataType::Time => 61,
        DataType::Timestamp => 70,
        DataType::TimestampTz => 71,
        DataType::Interval => 80,
        DataType::Uuid => 90,
        DataType::Bytes => 95,
        DataType::Json => 110,
        DataType::Jsonb => 111,
        DataType::Array(_) => 120,
        DataType::Vector(_) => 130,
        DataType::UserDefined(_) => 200,
        DataType::Tsvector => 140,
        DataType::Tsquery => 141,
        DataType::Unknown => 0, // lowest precedence — adapts to the other type
    }
}

pub fn is_numeric(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int32
            | DataType::Int64
            | DataType::Oid
            | DataType::Float64
            | DataType::Numeric { .. }
    )
}

/// Is `from` implicitly coercible to `to` in PostgreSQL's sense (entries
/// in `pg_catalog.pg_cast` with `castcontext = 'i'`)? This is the
/// predicate used by the planner during **function overload resolution**
/// (FunctionRegistry::resolve_return_type) — a declared argument type
/// accepts an actual value whose type implicitly casts to it without an
/// explicit `CAST` / `::` annotation.
///
/// PG distinguishes implicit (any SQL expression), assignment (INSERT
/// target), and explicit (`CAST(...)` only) cast contexts. Overload
/// resolution uses implicit only; cross-category numeric casts like
/// dp ↔ numeric are explicit in PG and deliberately excluded here — so
/// `SIGN(1.5::numeric)` does not match the `sign(double precision)`
/// overload, and vice versa.
///
/// Verified against PG 16.13's `pg_cast` for the numeric category:
///
/// ```text
/// SELECT castsource::regtype, casttarget::regtype, castcontext
///   FROM pg_cast
///  WHERE castsource IN (21,23,20,700,701,1700)
///    AND casttarget IN (21,23,20,700,701,1700)
///    AND castcontext = 'i';
/// -- smallint / int / bigint → smallint / int / bigint / real / dp / numeric
/// -- real → dp
/// -- (no real↔numeric, no dp↔numeric implicit)
/// ```
pub fn is_implicitly_coercible(from: &DataType, to: &DataType) -> bool {
    if from == to {
        return true;
    }
    // Numeric typmod (precision/scale) is not part of the implicit-cast
    // identity — sign(numeric(10,2)) resolves to the sign(numeric)
    // overload regardless of typmod.
    if let (DataType::Numeric { .. }, DataType::Numeric { .. }) = (from, to) {
        return true;
    }
    // Integer-family values widen implicitly into larger numeric-category
    // targets. Do not allow downcasts such as int8 -> int4; PostgreSQL does
    // not use those during function overload resolution.
    if matches!(
        (from, to),
        (DataType::Int32, DataType::Int64)
            | (DataType::Int32, DataType::Oid)
            | (DataType::Int32, DataType::Float64)
            | (DataType::Int32, DataType::Numeric { .. })
            | (DataType::Int64, DataType::Float64)
            | (DataType::Int64, DataType::Numeric { .. })
            | (DataType::Oid, DataType::Int64)
            | (DataType::Oid, DataType::Float64)
            | (DataType::Oid, DataType::Numeric { .. })
    ) {
        return true;
    }
    false
}

pub fn common_type(a: &DataType, b: &DataType) -> Option<DataType> {
    // Both Unknown → resolve to Text (PG defaults UNKNOWNOID to TEXT).
    if matches!(a, DataType::Unknown) && matches!(b, DataType::Unknown) {
        return Some(DataType::Text);
    }

    if a == b {
        return Some(a.clone());
    }

    // Unknown adapts to the other type (PG's UNKNOWNOID semantics).
    if matches!(a, DataType::Unknown) {
        return Some(b.clone());
    }
    if matches!(b, DataType::Unknown) {
        return Some(a.clone());
    }

    // Array(Unknown) adapts recursively.
    if let (DataType::Array(inner_a), DataType::Array(inner_b)) = (a, b) {
        return common_type(inner_a, inner_b).map(|t| DataType::Array(Box::new(t)));
    }

    // Numeric type promotion
    if is_numeric(a) && is_numeric(b) {
        return Some(if type_precedence(a) > type_precedence(b) {
            a.clone()
        } else {
            b.clone()
        });
    }

    // Temporal type rules
    match (a, b) {
        (DataType::Timestamp, DataType::TimestampTz)
        | (DataType::TimestampTz, DataType::Timestamp) => Some(DataType::TimestampTz),
        (DataType::Date, DataType::Timestamp) | (DataType::Timestamp, DataType::Date) => {
            Some(DataType::Timestamp)
        }
        (DataType::Date, DataType::TimestampTz) | (DataType::TimestampTz, DataType::Date) => {
            Some(DataType::TimestampTz)
        }

        // JSON rules
        (DataType::Json, DataType::Jsonb) | (DataType::Jsonb, DataType::Json) => {
            Some(DataType::Jsonb)
        }

        // Text-like as universal fallback
        (DataType::Text, _)
        | (_, DataType::Text)
        | (DataType::Varchar(_), _)
        | (_, DataType::Varchar(_))
        | (DataType::Name, _)
        | (_, DataType::Name) => Some(DataType::Text),

        _ => None,
    }
}

/// Determine the coercion target for comparison operators.
///
/// Unlike `common_type`, comparison prefers the non-text typed side for
/// `Text/Name` mixed comparisons, matching PostgreSQL semantics where
/// `'42' = 42` coerces text to integer.
pub fn comparison_target_type(a: &DataType, b: &DataType) -> Option<DataType> {
    // Both Unknown → resolve to Text (matches common_type guard).
    if matches!(a, DataType::Unknown) && matches!(b, DataType::Unknown) {
        return Some(DataType::Text);
    }
    if a == b {
        return Some(a.clone());
    }

    // Mixed integer/float comparisons keep their original operand types so
    // runtime evaluation can preserve integer precision without lossy `as f64`
    // promotion above the 53-bit mantissa boundary.
    if matches!(
        (a, b),
        (DataType::Int32 | DataType::Int64, DataType::Float64)
            | (DataType::Float64, DataType::Int32 | DataType::Int64)
    ) {
        return None;
    }

    // Unknown adapts to the other type.
    if matches!(a, DataType::Unknown) {
        return Some(b.clone());
    }
    if matches!(b, DataType::Unknown) {
        return Some(a.clone());
    }

    // Both numeric -> higher-precedence numeric wins.
    if is_numeric(a) && is_numeric(b) {
        return common_type(a, b);
    }

    match (a, b) {
        (alias, DataType::Int32 | DataType::Int64 | DataType::Oid) if is_oid_alias_type(alias) => {
            Some(alias.clone())
        }
        (DataType::Int32 | DataType::Int64 | DataType::Oid, alias) if is_oid_alias_type(alias) => {
            Some(alias.clone())
        }
        // Text-like vs typed side -> typed side wins.
        (DataType::Text, other) | (DataType::Name, other)
            if *other != DataType::Text && *other != DataType::Name =>
        {
            Some(other.clone())
        }
        (other, DataType::Text) | (other, DataType::Name)
            if *other != DataType::Text && *other != DataType::Name =>
        {
            Some(other.clone())
        }

        // Temporal promotions.
        (DataType::Date, DataType::Timestamp) | (DataType::Timestamp, DataType::Date) => {
            Some(DataType::Timestamp)
        }
        (DataType::Date, DataType::TimestampTz) | (DataType::TimestampTz, DataType::Date) => {
            Some(DataType::TimestampTz)
        }
        (DataType::Timestamp, DataType::TimestampTz)
        | (DataType::TimestampTz, DataType::Timestamp) => Some(DataType::TimestampTz),

        // JSON cross-type comparisons are unsupported.
        (DataType::Json, DataType::Jsonb)
        | (DataType::Jsonb, DataType::Json)
        | (DataType::Json, _)
        | (_, DataType::Json)
        | (DataType::Jsonb, _)
        | (_, DataType::Jsonb) => None,

        _ => None,
    }
}

/// Check if a source type can be assigned to a target type in assignment context.
///
/// PostgreSQL assignment coercion is more permissive than implicit comparison
/// coercion. Text remains universally assignable via I/O coercion; other types
/// are compatible when they share a common type.
pub fn is_assignment_compatible(from: &DataType, to: &DataType) -> bool {
    // Unknown is coercible to any type (PG's UNKNOWNOID semantics).
    if matches!(from, DataType::Unknown) {
        return true;
    }
    if matches!(from, DataType::Text) || matches!(to, DataType::Text) {
        return true;
    }
    if let (DataType::Array(_), DataType::Array(_)) = (from, to) {
        fn array_base_type(dt: &DataType) -> &DataType {
            match dt {
                DataType::Array(inner) => array_base_type(inner),
                other => other,
            }
        }

        return is_assignment_compatible(array_base_type(from), array_base_type(to));
    }
    if let (DataType::Vector(from_dim), DataType::Vector(to_dim)) = (from, to) {
        return match (*from_dim, *to_dim) {
            // Bare `vector` has no typmod. pgvector allows vector -> vector(n)
            // assignment through its vector(vector, typmod) cast; runtime cast
            // enforces the actual dimensions.
            (0, _) | (_, 0) => true,
            (lhs, rhs) => lhs == rhs,
        };
    }
    common_type(from, to).is_some()
}

pub fn unify_types(types: &[DataType]) -> Option<DataType> {
    if types.is_empty() {
        return None;
    }
    let mut result = types[0].clone();
    for t in &types[1..] {
        result = common_type(&result, t)?;
    }
    Some(result)
}

pub fn binary_op_result_type(op: &str, left: &DataType, right: &DataType) -> Option<DataType> {
    let vector_result_type = |left: &DataType, right: &DataType| match (left, right) {
        (DataType::Vector(ld), DataType::Vector(rd)) if *ld > 0 && *rd > 0 && ld == rd => {
            Some(DataType::Vector(*ld))
        }
        (DataType::Vector(ld), DataType::Vector(_)) if *ld > 0 => Some(DataType::Vector(*ld)),
        (DataType::Vector(_), DataType::Vector(rd)) if *rd > 0 => Some(DataType::Vector(*rd)),
        (DataType::Vector(_), DataType::Vector(_)) => Some(DataType::Vector(0)),
        _ => None,
    };

    match op {
        // Arithmetic operators
        "Plus" | "Minus" | "+" | "-" => {
            // Temporal type special rules
            match (left, right) {
                (DataType::Timestamp, DataType::Interval)
                | (DataType::Interval, DataType::Timestamp) => Some(DataType::Timestamp),
                (DataType::TimestampTz, DataType::Interval)
                | (DataType::Interval, DataType::TimestampTz) => Some(DataType::TimestampTz),
                (DataType::Date, DataType::Interval) | (DataType::Interval, DataType::Date) => {
                    Some(DataType::Timestamp)
                }
                (DataType::Date, DataType::Int32) | (DataType::Int32, DataType::Date) => {
                    Some(DataType::Date)
                }
                (DataType::Date, DataType::Int64) => Some(DataType::Date),
                (DataType::Int64, DataType::Date) if op == "Plus" || op == "+" => {
                    Some(DataType::Date)
                }
                (DataType::Interval, DataType::Interval) => Some(DataType::Interval),
                (DataType::Timestamp, DataType::Timestamp)
                | (DataType::TimestampTz, DataType::TimestampTz)
                | (DataType::Timestamp, DataType::TimestampTz)
                | (DataType::TimestampTz, DataType::Timestamp)
                | (DataType::Timestamp, DataType::Date)
                | (DataType::Date, DataType::Timestamp)
                | (DataType::TimestampTz, DataType::Date)
                | (DataType::Date, DataType::TimestampTz)
                    if op == "Minus" || op == "-" =>
                {
                    Some(DataType::Interval)
                }
                (DataType::Date, DataType::Date) if op == "Minus" || op == "-" => {
                    Some(DataType::Int32)
                }
                (DataType::Jsonb, DataType::Text)
                | (DataType::Jsonb, DataType::Int32)
                | (DataType::Jsonb, DataType::Int64)
                    if op == "Minus" || op == "-" =>
                {
                    Some(DataType::Jsonb)
                }
                (DataType::Jsonb, DataType::Array(inner))
                    if (op == "Minus" || op == "-")
                        && matches!(
                            inner.as_ref(),
                            DataType::Text
                                | DataType::Varchar(_)
                                | DataType::Name
                                | DataType::Unknown
                        ) =>
                {
                    Some(DataType::Jsonb)
                }
                (DataType::Vector(_), DataType::Vector(_)) => vector_result_type(left, right),
                _ if is_numeric(left) && is_numeric(right) => common_type(left, right),
                _ => None,
            }
        }
        "Modulo" | "%" => {
            if matches!(left, DataType::Float64) || matches!(right, DataType::Float64) {
                None
            } else if is_numeric(left) && is_numeric(right) {
                common_type(left, right)
            } else {
                None
            }
        }
        "Multiply" | "Divide" | "PGExp" | "*" | "/" | "^" => {
            if is_numeric(left) && is_numeric(right) {
                common_type(left, right)
            } else if is_numeric(left) && matches!(right, DataType::Vector(_)) {
                Some(right.clone())
            } else if matches!(left, DataType::Vector(_)) && is_numeric(right) {
                Some(left.clone())
            } else if matches!(
                (left, right),
                (DataType::Interval, _) | (_, DataType::Interval)
            ) {
                // interval * number
                Some(DataType::Interval)
            } else {
                None
            }
        }

        // Concatenation (||) overloads (PostgreSQL):
        // - text || text -> text
        // - jsonb || jsonb -> jsonb
        // - anyarray || anyarray / anyarray || anyelement / anyelement || anyarray -> anyarray
        // - tsvector || tsvector -> tsvector
        "StringConcat" | "||" => match (left, right) {
            (DataType::Jsonb, DataType::Jsonb) => Some(DataType::Jsonb),
            (DataType::Tsvector, DataType::Tsvector) => Some(DataType::Tsvector),
            (DataType::Array(inner), DataType::Array(_)) => Some(DataType::Array(inner.clone())),
            (DataType::Array(inner), _) => Some(DataType::Array(inner.clone())),
            (_, DataType::Array(inner)) => Some(DataType::Array(inner.clone())),
            _ => Some(DataType::Text),
        },

        // Comparison operators
        "Eq" | "NotEq" | "Lt" | "LtEq" | "Gt" | "GtEq" | "=" | "!=" | "<>" | "<" | "<=" | ">"
        | ">=" => Some(DataType::Boolean),

        // Logical operators
        "And" | "Or" | "AND" | "OR" => Some(DataType::Boolean),

        // JSON operators
        "Arrow" | "->" => match left {
            DataType::Json => Some(DataType::Json),
            DataType::Jsonb => Some(DataType::Jsonb),
            _ => None,
        },
        "LongArrow" | "->>" => Some(DataType::Text),
        "HashArrow" | "#>" => match left {
            DataType::Json => Some(DataType::Json),
            DataType::Jsonb => Some(DataType::Jsonb),
            _ => None,
        },
        "HashLongArrow" | "#>>" => Some(DataType::Text),
        "HashMinus" | "#-" => match (left, right) {
            (DataType::Jsonb, DataType::Array(_)) => Some(DataType::Jsonb),
            (DataType::Jsonb, DataType::Text) => Some(DataType::Jsonb),
            _ => None,
        },
        "AtArrow" | "ArrowAt" | "@>" | "<@" | "?" | "?|" | "?&" => Some(DataType::Boolean),

        // Regex operators (PostgreSQL-specific, always return boolean)
        "PGRegexMatch" | "PGRegexIMatch" | "PGRegexNotMatch" | "PGRegexNotIMatch" | "~" | "~*"
        | "!~" | "!~*" => Some(DataType::Boolean),

        // Array overlap operator
        // Full-text search match
        "TsMatch" | "@@" => Some(DataType::Boolean),

        // Array overlap
        "PGOverlap" => Some(DataType::Boolean),
        "&&" if matches!(left, DataType::Array(_)) => Some(DataType::Boolean),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── common_type: Text wins ───────────────────────────────────

    #[test]
    fn common_type_text_vs_int32_yields_text() {
        assert_eq!(
            common_type(&DataType::Text, &DataType::Int32),
            Some(DataType::Text)
        );
        assert_eq!(
            common_type(&DataType::Int32, &DataType::Text),
            Some(DataType::Text)
        );
    }

    #[test]
    fn common_type_numeric_promotion() {
        assert_eq!(
            common_type(&DataType::Int32, &DataType::Int64),
            Some(DataType::Int64)
        );
        assert_eq!(
            common_type(&DataType::Int64, &DataType::Float64),
            Some(DataType::Float64)
        );
    }

    #[test]
    fn common_type_numeric_over_float64() {
        // Numeric wins over Float64 (precision preservation, matches PostgreSQL)
        assert_eq!(
            common_type(
                &DataType::Numeric {
                    precision: None,
                    scale: None
                },
                &DataType::Float64
            ),
            Some(DataType::Numeric {
                precision: None,
                scale: None
            })
        );
        assert_eq!(
            common_type(
                &DataType::Float64,
                &DataType::Numeric {
                    precision: None,
                    scale: None
                }
            ),
            Some(DataType::Numeric {
                precision: None,
                scale: None
            })
        );
    }

    #[test]
    fn comparison_numeric_vs_float64_yields_numeric() {
        assert_eq!(
            comparison_target_type(
                &DataType::Numeric {
                    precision: None,
                    scale: None
                },
                &DataType::Float64
            ),
            Some(DataType::Numeric {
                precision: None,
                scale: None
            })
        );
        assert_eq!(
            comparison_target_type(
                &DataType::Float64,
                &DataType::Numeric {
                    precision: None,
                    scale: None
                }
            ),
            Some(DataType::Numeric {
                precision: None,
                scale: None
            })
        );
    }

    #[test]
    fn comparison_int_float_requires_exact_runtime_compare() {
        assert_eq!(
            comparison_target_type(&DataType::Int64, &DataType::Float64),
            None
        );
        assert_eq!(
            comparison_target_type(&DataType::Float64, &DataType::Int64),
            None
        );
        assert_eq!(
            comparison_target_type(&DataType::Int32, &DataType::Float64),
            None
        );
    }

    #[test]
    fn common_type_same_type() {
        assert_eq!(
            common_type(&DataType::Boolean, &DataType::Boolean),
            Some(DataType::Boolean)
        );
    }

    // ── comparison_target_type: non-Text wins ────────────────────

    #[test]
    fn comparison_text_vs_int32_yields_int32() {
        assert_eq!(
            comparison_target_type(&DataType::Text, &DataType::Int32),
            Some(DataType::Int32)
        );
        assert_eq!(
            comparison_target_type(&DataType::Int32, &DataType::Text),
            Some(DataType::Int32)
        );
    }

    #[test]
    fn comparison_text_vs_boolean_yields_boolean() {
        assert_eq!(
            comparison_target_type(&DataType::Text, &DataType::Boolean),
            Some(DataType::Boolean)
        );
    }

    #[test]
    fn comparison_text_vs_text_yields_text() {
        assert_eq!(
            comparison_target_type(&DataType::Text, &DataType::Text),
            Some(DataType::Text)
        );
    }

    #[test]
    fn vector_arithmetic_result_types() {
        assert_eq!(
            binary_op_result_type("+", &DataType::Vector(3), &DataType::Vector(3)),
            Some(DataType::Vector(3))
        );
        assert_eq!(
            binary_op_result_type("-", &DataType::Vector(0), &DataType::Vector(5)),
            Some(DataType::Vector(5))
        );
        assert_eq!(
            binary_op_result_type("*", &DataType::Vector(3), &DataType::Int32),
            Some(DataType::Vector(3))
        );
        assert_eq!(
            binary_op_result_type("*", &DataType::Float64, &DataType::Vector(0)),
            Some(DataType::Vector(0))
        );
    }

    #[test]
    fn comparison_name_vs_int64_yields_int64() {
        assert_eq!(
            comparison_target_type(&DataType::Name, &DataType::Int64),
            Some(DataType::Int64)
        );
    }

    #[test]
    fn comparison_regclass_vs_int64_yields_regclass() {
        let regclass = DataType::UserDefined("pg_catalog.regclass".to_string());
        assert_eq!(
            comparison_target_type(&regclass, &DataType::Int64),
            Some(regclass.clone())
        );
        assert_eq!(
            comparison_target_type(&DataType::Int64, &regclass),
            Some(regclass)
        );
    }

    // ── Boundary: the difference matters ─────────────────────────

    #[test]
    fn common_vs_comparison_text_int_diverge() {
        // common_type: Text wins (for UNION/CASE)
        assert_eq!(
            common_type(&DataType::Text, &DataType::Int32),
            Some(DataType::Text)
        );
        // comparison_target_type: Int32 wins (for = < > operators)
        assert_eq!(
            comparison_target_type(&DataType::Text, &DataType::Int32),
            Some(DataType::Int32)
        );
    }

    // ── Temporal coercion ────────────────────────────────────────

    #[test]
    fn comparison_date_timestamp_yields_timestamp() {
        assert_eq!(
            comparison_target_type(&DataType::Date, &DataType::Timestamp),
            Some(DataType::Timestamp)
        );
    }

    // ── JSON comparisons unsupported ─────────────────────────────

    #[test]
    fn comparison_json_vs_non_text_is_none() {
        // JSON vs non-text types are incomparable.
        assert_eq!(
            comparison_target_type(&DataType::Json, &DataType::Int32),
            None
        );
        assert_eq!(
            comparison_target_type(&DataType::Jsonb, &DataType::Boolean),
            None
        );
    }

    #[test]
    fn comparison_jsonb_vs_text_yields_jsonb() {
        // Text vs Jsonb: the non-text side wins (text literal cast to jsonb).
        assert_eq!(
            comparison_target_type(&DataType::Jsonb, &DataType::Text),
            Some(DataType::Jsonb)
        );
    }

    #[test]
    fn binary_op_result_type_json_access_preserves_input_json_family() {
        assert_eq!(
            binary_op_result_type("->", &DataType::Json, &DataType::Text),
            Some(DataType::Json)
        );
        assert_eq!(
            binary_op_result_type("->", &DataType::Jsonb, &DataType::Text),
            Some(DataType::Jsonb)
        );
        assert_eq!(
            binary_op_result_type("#>", &DataType::Json, &DataType::Text),
            Some(DataType::Json)
        );
        assert_eq!(
            binary_op_result_type("#>", &DataType::Jsonb, &DataType::Text),
            Some(DataType::Jsonb)
        );
    }

    #[test]
    fn assignment_compatibility_allows_bare_vector_to_concrete_vector() {
        assert!(is_assignment_compatible(
            &DataType::Vector(0),
            &DataType::Vector(1024)
        ));
        assert!(is_assignment_compatible(
            &DataType::Vector(1024),
            &DataType::Vector(0)
        ));
        assert!(is_assignment_compatible(
            &DataType::Vector(1024),
            &DataType::Vector(1024)
        ));
    }

    #[test]
    fn assignment_compatibility_ignores_array_dimensions_when_base_types_match() {
        assert!(is_assignment_compatible(
            &DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int32)))),
            &DataType::Array(Box::new(DataType::Int32))
        ));
        assert!(!is_assignment_compatible(
            &DataType::Array(Box::new(DataType::Array(Box::new(DataType::Boolean)))),
            &DataType::Array(Box::new(DataType::Int32))
        ));
    }
}
