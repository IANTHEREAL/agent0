use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use crate::sql::pg_numeric::pg_numeric_div;

// ── Checked Decimal arithmetic ──────────────────────────────────────
//
// `rust_decimal` v1.40 panics on overflow for +, -, *, /, %.
// These helpers use the checked_* methods and return SQLSTATE 22003
// (numeric_value_out_of_range) instead of crashing the server.

#[inline]
pub(crate) fn checked_decimal_add(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b).ok_or_else(|| {
        SqlError::NumericValueOutOfRange {
            message: "numeric field overflow: result of addition exceeds numeric capacity".into(),
        }
        .into()
    })
}

#[inline]
pub(crate) fn checked_decimal_sub(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_sub(b).ok_or_else(|| {
        SqlError::NumericValueOutOfRange {
            message: "numeric field overflow: result of subtraction exceeds numeric capacity"
                .into(),
        }
        .into()
    })
}

#[inline]
pub(crate) fn checked_decimal_mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_mul(b).ok_or_else(|| {
        SqlError::NumericValueOutOfRange {
            message: "numeric field overflow: result of multiplication exceeds numeric capacity"
                .into(),
        }
        .into()
    })
}

#[inline]
pub(crate) fn checked_decimal_div(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_div(b).ok_or_else(|| {
        SqlError::NumericValueOutOfRange {
            message: "numeric field overflow: result of division exceeds numeric capacity".into(),
        }
        .into()
    })
}

#[inline]
pub(crate) fn checked_decimal_rem(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_rem(b).ok_or_else(|| {
        SqlError::NumericValueOutOfRange {
            message: "numeric field overflow: result of modulo exceeds numeric capacity".into(),
        }
        .into()
    })
}

#[derive(Debug, Clone)]
pub enum NumericValue {
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Decimal(Decimal),
}

impl NumericValue {
    pub fn from_value(v: &crate::model::Value) -> Option<Self> {
        match v {
            crate::model::Value::Int32(n) => Some(NumericValue::Int32(*n)),
            crate::model::Value::Int64(n) => Some(NumericValue::Int64(*n)),
            crate::model::Value::Float64(n) => Some(NumericValue::Float64(*n)),
            crate::model::Value::Numeric(n) => Some(NumericValue::Decimal(*n)),
            _ => None,
        }
    }

    pub fn into_value(self) -> crate::model::Value {
        match self {
            NumericValue::Int32(n) => crate::model::Value::Int32(n),
            NumericValue::Int64(n) => crate::model::Value::Int64(n),
            NumericValue::Float64(n) => crate::model::Value::Float64(n),
            NumericValue::Decimal(n) => crate::model::Value::Numeric(n),
        }
    }

    fn to_int64(&self) -> Result<i64> {
        match self {
            NumericValue::Int32(n) => Ok(*n as i64),
            NumericValue::Int64(n) => Ok(*n),
            NumericValue::Float64(n) => Ok(*n as i64),
            NumericValue::Decimal(n) => n.to_i64().ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: format!(
                        "numeric value out of range: {} cannot be converted to bigint",
                        n
                    ),
                }
                .into()
            }),
        }
    }

    fn to_float64(&self) -> Result<f64> {
        match self {
            NumericValue::Int32(n) => Ok(*n as f64),
            NumericValue::Int64(n) => Ok(*n as f64),
            NumericValue::Float64(n) => Ok(*n),
            NumericValue::Decimal(n) => n.to_f64().ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "numeric value out of range for double precision".into(),
                }
                .into()
            }),
        }
    }

    fn to_decimal(&self) -> Result<Decimal> {
        match self {
            NumericValue::Int32(n) => Ok(Decimal::from(*n)),
            NumericValue::Int64(n) => Ok(Decimal::from(*n)),
            NumericValue::Float64(n) => Decimal::try_from(*n).map_err(|_| {
                SqlError::NumericValueOutOfRange {
                    message: format!(
                        "numeric value out of range: {} cannot be converted to numeric",
                        n
                    ),
                }
                .into()
            }),
            NumericValue::Decimal(n) => Ok(*n),
        }
    }

    fn type_priority(&self) -> u8 {
        match self {
            NumericValue::Int32(_) => 1,
            NumericValue::Int64(_) => 2,
            NumericValue::Decimal(_) => 3,
            NumericValue::Float64(_) => 4,
        }
    }

    pub fn promote_pair(left: Self, right: Self) -> Result<(Self, Self)> {
        let left_prio = left.type_priority();
        let right_prio = right.type_priority();

        if left_prio == right_prio {
            return Ok((left, right));
        }

        let target_prio = left_prio.max(right_prio);

        let promote = |v: Self| -> Result<Self> {
            if v.type_priority() == target_prio {
                return Ok(v);
            }
            match target_prio {
                2 => Ok(NumericValue::Int64(v.to_int64()?)),
                3 => Ok(NumericValue::Decimal(v.to_decimal()?)),
                4 => Ok(NumericValue::Float64(v.to_float64()?)),
                _ => Ok(v),
            }
        };

        Ok((promote(left)?, promote(right)?))
    }

    pub fn is_zero(&self) -> bool {
        match self {
            NumericValue::Int32(n) => *n == 0,
            NumericValue::Int64(n) => *n == 0,
            NumericValue::Float64(n) => *n == 0.0,
            NumericValue::Decimal(n) => n.is_zero(),
        }
    }
}

pub fn numeric_add(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => {
            a.checked_add(b).map(NumericValue::Int32).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "integer out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            a.checked_add(b).map(NumericValue::Int64).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a + b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => {
            Ok(NumericValue::Decimal(checked_decimal_add(a, b)?))
        }
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_sub(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => {
            a.checked_sub(b).map(NumericValue::Int32).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "integer out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            a.checked_sub(b).map(NumericValue::Int64).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a - b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => {
            Ok(NumericValue::Decimal(checked_decimal_sub(a, b)?))
        }
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_mul(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => {
            a.checked_mul(b).map(NumericValue::Int32).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "integer out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            a.checked_mul(b).map(NumericValue::Int64).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a * b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => {
            Ok(NumericValue::Decimal(checked_decimal_mul(a, b)?))
        }
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_div(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    if right.is_zero() {
        return Err(SqlError::DivisionByZero.into());
    }

    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => {
            a.checked_div(b).map(NumericValue::Int32).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "integer out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            a.checked_div(b).map(NumericValue::Int64).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            })
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a / b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => {
            Ok(NumericValue::Decimal(pg_numeric_div(a, b)?))
        }
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_mod(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    if right.is_zero() {
        return Err(anyhow!("Modulo by zero"));
    }

    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        // checked_rem returns None for MIN % -1, but PG returns 0.
        (NumericValue::Int32(a), NumericValue::Int32(b)) => {
            Ok(NumericValue::Int32(a.checked_rem(b).unwrap_or(0)))
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            Ok(NumericValue::Int64(a.checked_rem(b).unwrap_or(0)))
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a % b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => {
            Ok(NumericValue::Decimal(checked_decimal_rem(a, b)?))
        }
        _ => Err(anyhow!("type promotion failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_promote_same_type() {
        let (l, r) =
            NumericValue::promote_pair(NumericValue::Int32(1), NumericValue::Int32(2)).unwrap();
        assert!(matches!(l, NumericValue::Int32(1)));
        assert!(matches!(r, NumericValue::Int32(2)));
    }

    #[test]
    fn test_promote_int32_to_int64() {
        let (l, r) =
            NumericValue::promote_pair(NumericValue::Int32(1), NumericValue::Int64(2)).unwrap();
        assert!(matches!(l, NumericValue::Int64(1)));
        assert!(matches!(r, NumericValue::Int64(2)));
    }

    #[test]
    fn test_promote_int_to_float() {
        let (l, r) =
            NumericValue::promote_pair(NumericValue::Int32(1), NumericValue::Float64(2.0)).unwrap();
        assert!(matches!(l, NumericValue::Float64(_)));
        assert!(matches!(r, NumericValue::Float64(2.0)));
    }

    #[test]
    fn test_promote_int_to_decimal() {
        let (l, r) =
            NumericValue::promote_pair(NumericValue::Int32(1), NumericValue::Decimal(Decimal::TWO))
                .unwrap();
        assert!(matches!(l, NumericValue::Decimal(_)));
        assert!(matches!(r, NumericValue::Decimal(_)));
    }

    #[test]
    fn test_promote_decimal_to_float() {
        let (l, r) = NumericValue::promote_pair(
            NumericValue::Decimal(Decimal::ONE),
            NumericValue::Float64(2.0),
        )
        .unwrap();
        assert!(matches!(l, NumericValue::Float64(_)));
        assert!(matches!(r, NumericValue::Float64(2.0)));
    }

    #[test]
    fn test_add() {
        let result = numeric_add(NumericValue::Int32(1), NumericValue::Int32(2)).unwrap();
        assert!(matches!(result, NumericValue::Int32(3)));

        let result = numeric_add(NumericValue::Int32(1), NumericValue::Int64(2)).unwrap();
        assert!(matches!(result, NumericValue::Int64(3)));

        let result = numeric_add(NumericValue::Float64(1.5), NumericValue::Float64(2.5)).unwrap();
        if let NumericValue::Float64(f) = result {
            assert!((f - 4.0).abs() < 0.001);
        } else {
            panic!("expected Float64");
        }
    }

    #[test]
    fn test_div_by_zero() {
        let result = numeric_div(NumericValue::Int32(1), NumericValue::Int32(0));
        assert!(result.is_err());
    }

    #[test]
    fn test_mod() {
        let result = numeric_mod(NumericValue::Int32(7), NumericValue::Int32(3)).unwrap();
        assert!(matches!(result, NumericValue::Int32(1)));
    }

    #[test]
    fn test_mod_by_zero_error_message() {
        let err = numeric_mod(NumericValue::Int32(1), NumericValue::Int32(0))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Modulo by zero");
    }

    #[test]
    fn test_div_decimal_pg_scale_rounding() {
        let result = numeric_div(
            NumericValue::Decimal(Decimal::from(800)),
            NumericValue::Int32(3),
        )
        .unwrap();

        match result {
            NumericValue::Decimal(d) => assert_eq!(d.to_string(), "266.6666666666666667"),
            _ => panic!("expected Decimal"),
        }
    }

    #[test]
    fn test_div_decimal_pg_scale_firstdigit_adjust() {
        let result =
            numeric_div(NumericValue::Decimal(Decimal::ONE), NumericValue::Int32(3)).unwrap();

        match result {
            NumericValue::Decimal(d) => assert_eq!(d.to_string(), "0.33333333333333333333"),
            _ => panic!("expected Decimal"),
        }
    }

    // ── Overflow tests ──────────────────────────────────────────────

    #[test]
    fn test_decimal_add_overflow_returns_error() {
        let max = NumericValue::Decimal(Decimal::MAX);
        let one = NumericValue::Decimal(Decimal::ONE);
        let err = numeric_add(max, one).unwrap_err();
        assert!(err.to_string().contains("numeric field overflow"));
    }

    #[test]
    fn test_decimal_sub_overflow_returns_error() {
        let min = NumericValue::Decimal(Decimal::MIN);
        let one = NumericValue::Decimal(Decimal::ONE);
        let err = numeric_sub(min, one).unwrap_err();
        assert!(err.to_string().contains("numeric field overflow"));
    }

    #[test]
    fn test_decimal_mul_overflow_returns_error() {
        let big = NumericValue::Decimal(Decimal::MAX);
        let two = NumericValue::Decimal(Decimal::TWO);
        let err = numeric_mul(big, two).unwrap_err();
        assert!(err.to_string().contains("numeric field overflow"));
    }

    #[test]
    fn test_decimal_div_overflow_returns_error() {
        let max = NumericValue::Decimal(Decimal::MAX);
        let half = NumericValue::Decimal(Decimal::new(5, 1)); // 0.5
        let err = numeric_div(max, half).unwrap_err();
        assert!(err.to_string().contains("numeric field overflow"));
    }

    #[test]
    fn test_int32_add_overflow_returns_error() {
        let max = NumericValue::Int32(i32::MAX);
        let one = NumericValue::Int32(1);
        let err = numeric_add(max, one).unwrap_err();
        assert!(err.to_string().contains("integer out of range"));
    }

    #[test]
    fn test_int32_sub_overflow_returns_error() {
        let min = NumericValue::Int32(i32::MIN);
        let one = NumericValue::Int32(1);
        let err = numeric_sub(min, one).unwrap_err();
        assert!(err.to_string().contains("integer out of range"));
    }

    #[test]
    fn test_int64_add_overflow_returns_error() {
        let max = NumericValue::Int64(i64::MAX);
        let one = NumericValue::Int64(1);
        let err = numeric_add(max, one).unwrap_err();
        assert!(err.to_string().contains("bigint out of range"));
    }

    #[test]
    fn test_int64_mul_overflow_returns_error() {
        let big = NumericValue::Int64(i64::MAX);
        let two = NumericValue::Int64(2);
        let err = numeric_mul(big, two).unwrap_err();
        assert!(err.to_string().contains("bigint out of range"));
    }

    #[test]
    fn test_checked_helpers_return_sqlstate_22003() {
        use crate::sql::error::SqlError;
        let err = checked_decimal_add(Decimal::MAX, Decimal::ONE).unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("should be SqlError");
        assert_eq!(sql_err.sqlstate(), "22003");
    }
}
