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

pub fn type_precedence(dt: &DataType) -> i32 {
    match dt {
        DataType::Boolean => 10,
        DataType::Int32 => 20,
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
    }
}

pub fn is_numeric(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int32 | DataType::Int64 | DataType::Float64 | DataType::Numeric { .. }
    )
}

pub fn common_type(a: &DataType, b: &DataType) -> Option<DataType> {
    if a == b {
        return Some(a.clone());
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
    if a == b {
        return Some(a.clone());
    }

    // Both numeric -> higher-precedence numeric wins.
    if is_numeric(a) && is_numeric(b) {
        return common_type(a, b);
    }

    match (a, b) {
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
                (DataType::Interval, DataType::Interval) => Some(DataType::Interval),
                (DataType::Timestamp, DataType::Timestamp) if op == "Minus" || op == "-" => {
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
                            DataType::Text | DataType::Varchar(_) | DataType::Name
                        ) =>
                {
                    Some(DataType::Jsonb)
                }
                (DataType::Vector(_), DataType::Vector(_)) => vector_result_type(left, right),
                _ if is_numeric(left) && is_numeric(right) => common_type(left, right),
                _ => None,
            }
        }
        "Multiply" | "Divide" | "Modulo" | "PGExp" | "*" | "/" | "%" | "^" => {
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

        // String concatenation
        "StringConcat" | "||" => Some(DataType::Text),

        // Comparison operators
        "Eq" | "NotEq" | "Lt" | "LtEq" | "Gt" | "GtEq" | "=" | "!=" | "<>" | "<" | "<=" | ">"
        | ">=" => Some(DataType::Boolean),

        // Logical operators
        "And" | "Or" | "AND" | "OR" => Some(DataType::Boolean),

        // JSON operators
        "Arrow" | "->" => Some(DataType::Jsonb),
        "LongArrow" | "->>" => Some(DataType::Text),
        "HashArrow" | "#>" => Some(DataType::Jsonb),
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
}
