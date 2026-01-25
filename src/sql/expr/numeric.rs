use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub enum NumericValue {
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Decimal(Decimal),
}

impl NumericValue {
    pub fn from_value(v: &crate::types::Value) -> Option<Self> {
        match v {
            crate::types::Value::Int32(n) => Some(NumericValue::Int32(*n)),
            crate::types::Value::Int64(n) => Some(NumericValue::Int64(*n)),
            crate::types::Value::Float64(n) => Some(NumericValue::Float64(*n)),
            crate::types::Value::Numeric(n) => Some(NumericValue::Decimal(*n)),
            _ => None,
        }
    }

    pub fn into_value(self) -> crate::types::Value {
        match self {
            NumericValue::Int32(n) => crate::types::Value::Int32(n),
            NumericValue::Int64(n) => crate::types::Value::Int64(n),
            NumericValue::Float64(n) => crate::types::Value::Float64(n),
            NumericValue::Decimal(n) => crate::types::Value::Numeric(n),
        }
    }

    fn to_int64(&self) -> i64 {
        match self {
            NumericValue::Int32(n) => *n as i64,
            NumericValue::Int64(n) => *n,
            NumericValue::Float64(n) => *n as i64,
            NumericValue::Decimal(n) => n.to_i64().unwrap_or(0),
        }
    }

    fn to_float64(&self) -> Result<f64> {
        match self {
            NumericValue::Int32(n) => Ok(*n as f64),
            NumericValue::Int64(n) => Ok(*n as f64),
            NumericValue::Float64(n) => Ok(*n),
            NumericValue::Decimal(n) => n
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision")),
        }
    }

    fn to_decimal(&self) -> Decimal {
        match self {
            NumericValue::Int32(n) => Decimal::from(*n),
            NumericValue::Int64(n) => Decimal::from(*n),
            NumericValue::Float64(n) => Decimal::try_from(*n).unwrap_or_default(),
            NumericValue::Decimal(n) => *n,
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
                2 => Ok(NumericValue::Int64(v.to_int64())),
                3 => Ok(NumericValue::Decimal(v.to_decimal())),
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
            Ok(NumericValue::Int32(a.wrapping_add(b)))
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            Ok(NumericValue::Int64(a.wrapping_add(b)))
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a + b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => Ok(NumericValue::Decimal(a + b)),
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_sub(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => {
            Ok(NumericValue::Int32(a.wrapping_sub(b)))
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            Ok(NumericValue::Int64(a.wrapping_sub(b)))
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a - b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => Ok(NumericValue::Decimal(a - b)),
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_mul(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => {
            Ok(NumericValue::Int32(a.wrapping_mul(b)))
        }
        (NumericValue::Int64(a), NumericValue::Int64(b)) => {
            Ok(NumericValue::Int64(a.wrapping_mul(b)))
        }
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a * b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => Ok(NumericValue::Decimal(a * b)),
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_div(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    if right.is_zero() {
        return Err(anyhow!("Division by zero"));
    }

    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => Ok(NumericValue::Int32(a / b)),
        (NumericValue::Int64(a), NumericValue::Int64(b)) => Ok(NumericValue::Int64(a / b)),
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a / b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => Ok(NumericValue::Decimal(a / b)),
        _ => Err(anyhow!("type promotion failed")),
    }
}

pub fn numeric_mod(left: NumericValue, right: NumericValue) -> Result<NumericValue> {
    if right.is_zero() {
        return Err(anyhow!("Modulo by zero"));
    }

    let (l, r) = NumericValue::promote_pair(left, right)?;
    match (l, r) {
        (NumericValue::Int32(a), NumericValue::Int32(b)) => Ok(NumericValue::Int32(a % b)),
        (NumericValue::Int64(a), NumericValue::Int64(b)) => Ok(NumericValue::Int64(a % b)),
        (NumericValue::Float64(a), NumericValue::Float64(b)) => Ok(NumericValue::Float64(a % b)),
        (NumericValue::Decimal(a), NumericValue::Decimal(b)) => Ok(NumericValue::Decimal(a % b)),
        _ => Err(anyhow!("type promotion failed")),
    }
}

#[allow(dead_code)]
pub fn numeric_neg(val: NumericValue) -> NumericValue {
    match val {
        NumericValue::Int32(n) => NumericValue::Int32(-n),
        NumericValue::Int64(n) => NumericValue::Int64(-n),
        NumericValue::Float64(n) => NumericValue::Float64(-n),
        NumericValue::Decimal(n) => NumericValue::Decimal(-n),
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
            NumericValue::promote_pair(NumericValue::Int32(1), NumericValue::Float64(2.0))
                .unwrap();
        assert!(matches!(l, NumericValue::Float64(_)));
        assert!(matches!(r, NumericValue::Float64(2.0)));
    }

    #[test]
    fn test_promote_int_to_decimal() {
        let (l, r) = NumericValue::promote_pair(
            NumericValue::Int32(1),
            NumericValue::Decimal(Decimal::TWO),
        )
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
}
