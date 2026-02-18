use crate::types::Value;
use anyhow::{anyhow, Result};
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("ABS", abs);
    map.insert("CEIL", ceil);
    map.insert("CEILING", ceil);
    map.insert("FLOOR", floor);
    map.insert("ROUND", round);
    map.insert("TRUNC", trunc);
    map.insert("TRUNCATE", trunc);
    map.insert("SQRT", sqrt);
    map.insert("CBRT", cbrt);
    map.insert("POWER", power);
    map.insert("POW", power);
    map.insert("EXP", exp);
    map.insert("LN", ln);
    map.insert("LOG", log10);
    map.insert("LOG10", log10);
    map.insert("SIGN", sign);
    map.insert("MOD", modulo);
    map.insert("DEGREES", degrees);
    map.insert("RADIANS", radians);
    map.insert("SIN", sin);
    map.insert("COS", cos);
    map.insert("TAN", tan);
    map.insert("PI", pi);
    map.insert("RANDOM", random);
}

pub fn abs(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Int32(n)) => Ok(Value::Int32(n.abs())),
        Some(Value::Int64(n)) => Ok(Value::Int64(n.abs())),
        Some(Value::Float64(n)) => Ok(Value::Float64(n.abs())),
        Some(Value::Numeric(d)) => Ok(Value::Numeric(d.abs())),
        _ => Ok(Value::Null),
    }
}

pub fn ceil(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.ceil())),
        Some(Value::Int32(n)) => Ok(Value::Int32(n)),
        Some(Value::Int64(n)) => Ok(Value::Int64(n)),
        Some(Value::Numeric(d)) => Ok(Value::Numeric(d.ceil())),
        _ => Ok(Value::Null),
    }
}

pub fn floor(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.floor())),
        Some(Value::Int32(n)) => Ok(Value::Int32(n)),
        Some(Value::Int64(n)) => Ok(Value::Int64(n)),
        Some(Value::Numeric(d)) => Ok(Value::Numeric(d.floor())),
        _ => Ok(Value::Null),
    }
}

fn decimal_pow10(exp: u32) -> Result<Decimal> {
    let mut factor = Decimal::ONE;
    for _ in 0..exp {
        factor = factor
            .checked_mul(Decimal::TEN)
            .ok_or_else(|| anyhow!("numeric value out of range"))?;
    }
    Ok(factor)
}

fn round_numeric_with_precision(d: Decimal, precision: i32) -> Result<Decimal> {
    if precision >= 0 {
        return Ok(
            d.round_dp_with_strategy(precision as u32, RoundingStrategy::MidpointAwayFromZero)
        );
    }

    let abs = precision.unsigned_abs();
    if abs > 28 {
        return Ok(Decimal::ZERO);
    }

    let factor = decimal_pow10(abs)?;
    let shifted = d
        .checked_div(factor)
        .ok_or_else(|| anyhow!("numeric value out of range"))?;
    let rounded = shifted.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero);
    rounded
        .checked_mul(factor)
        .ok_or_else(|| anyhow!("numeric value out of range"))
}

fn trunc_numeric_with_precision(d: Decimal, precision: i32) -> Result<Decimal> {
    if precision >= 0 {
        return Ok(d.round_dp_with_strategy(precision as u32, RoundingStrategy::ToZero));
    }

    let abs = precision.unsigned_abs();
    if abs > 28 {
        return Ok(Decimal::ZERO);
    }

    let factor = decimal_pow10(abs)?;
    let shifted = d
        .checked_div(factor)
        .ok_or_else(|| anyhow!("numeric value out of range"))?;
    let truncated = shifted.round_dp_with_strategy(0, RoundingStrategy::ToZero);
    truncated
        .checked_mul(factor)
        .ok_or_else(|| anyhow!("numeric value out of range"))
}

pub fn round(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let val = iter.next();
    let precision = match iter.next() {
        Some(Value::Int32(n)) => n,
        Some(Value::Int64(n)) => n as i32,
        _ => 0,
    };
    match val {
        Some(Value::Float64(n)) => {
            let factor = 10_f64.powi(precision);
            Ok(Value::Float64((n * factor).round() / factor))
        }
        Some(Value::Numeric(d)) => Ok(Value::Numeric(round_numeric_with_precision(d, precision)?)),
        Some(Value::Int32(n)) => Ok(Value::Int32(n)),
        Some(Value::Int64(n)) => Ok(Value::Int64(n)),
        _ => Ok(Value::Null),
    }
}

pub fn trunc(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let val = iter.next();
    let precision = match iter.next() {
        Some(Value::Int32(n)) => n,
        Some(Value::Int64(n)) => n as i32,
        _ => 0,
    };
    match val {
        Some(Value::Float64(n)) => {
            let factor = 10_f64.powi(precision);
            Ok(Value::Float64((n * factor).trunc() / factor))
        }
        Some(Value::Numeric(d)) => Ok(Value::Numeric(trunc_numeric_with_precision(d, precision)?)),
        Some(Value::Int32(n)) => Ok(Value::Int32(n)),
        Some(Value::Int64(n)) => Ok(Value::Int64(n)),
        _ => Ok(Value::Null),
    }
}

pub fn sqrt(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.sqrt())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).sqrt())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).sqrt())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.sqrt()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn cbrt(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.cbrt())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).cbrt())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).cbrt())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.cbrt()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn power(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    let mut iter = args.into_iter();
    let base = match iter.next() {
        Some(Value::Float64(n)) => n,
        Some(Value::Int32(n)) => n as f64,
        Some(Value::Int64(n)) => n as f64,
        Some(Value::Numeric(d)) => d
            .to_f64()
            .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?,
        _ => return Ok(Value::Null),
    };
    let exp = match iter.next() {
        Some(Value::Float64(n)) => n,
        Some(Value::Int32(n)) => n as f64,
        Some(Value::Int64(n)) => n as f64,
        Some(Value::Numeric(d)) => d
            .to_f64()
            .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?,
        _ => return Ok(Value::Null),
    };
    Ok(Value::Float64(base.powf(exp)))
}

pub fn exp(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.exp())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).exp())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).exp())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.exp()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn ln(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.ln())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).ln())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).ln())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.ln()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn log10(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.log10())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).log10())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).log10())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.log10()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn sign(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Int32(n)) => Ok(Value::Int32(if n > 0 {
            1
        } else if n < 0 {
            -1
        } else {
            0
        })),
        Some(Value::Int64(n)) => Ok(Value::Int64(if n > 0 {
            1
        } else if n < 0 {
            -1
        } else {
            0
        })),
        Some(Value::Float64(n)) => Ok(Value::Float64(if n > 0.0 {
            1.0
        } else if n < 0.0 {
            -1.0
        } else {
            0.0
        })),
        Some(Value::Numeric(d)) => Ok(Value::Int32(d.cmp(&Decimal::ZERO) as i32)),
        _ => Ok(Value::Null),
    }
}

pub fn modulo(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let a = iter.next();
    let b = iter.next();
    match (a, b) {
        (Some(Value::Int32(a)), Some(Value::Int32(b))) if b != 0 => Ok(Value::Int32(a % b)),
        (Some(Value::Int64(a)), Some(Value::Int64(b))) if b != 0 => Ok(Value::Int64(a % b)),
        (Some(Value::Float64(a)), Some(Value::Float64(b))) => Ok(Value::Float64(a % b)),
        _ => Ok(Value::Null),
    }
}

pub fn degrees(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.to_degrees())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).to_degrees())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).to_degrees())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.to_degrees()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn radians(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.to_radians())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).to_radians())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).to_radians())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.to_radians()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn sin(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.sin())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).sin())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).sin())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.sin()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn cos(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.cos())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).cos())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).cos())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.cos()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn tan(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.tan())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).tan())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).tan())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.tan()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn pi(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Float64(std::f64::consts::PI))
}

pub fn random(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Float64(rand::random::<f64>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_abs() {
        assert_eq!(abs(vec![Value::Int32(-5)]).unwrap(), Value::Int32(5));
        assert_eq!(
            abs(vec![Value::Float64(-std::f64::consts::PI)]).unwrap(),
            Value::Float64(std::f64::consts::PI)
        );
    }

    #[test]
    fn test_ceil() {
        assert_eq!(
            ceil(vec![Value::Float64(4.2)]).unwrap(),
            Value::Float64(5.0)
        );
        // Numeric input → Numeric output (PostgreSQL semantics)
        assert_eq!(
            ceil(vec![Value::Numeric(Decimal::new(42, 1))]).unwrap(),
            Value::Numeric(Decimal::new(5, 0))
        );
    }

    #[test]
    fn test_floor() {
        assert_eq!(
            floor(vec![Value::Float64(4.8)]).unwrap(),
            Value::Float64(4.0)
        );
        // Numeric input → Numeric output (PostgreSQL semantics)
        assert_eq!(
            floor(vec![Value::Numeric(Decimal::new(48, 1))]).unwrap(),
            Value::Numeric(Decimal::new(4, 0))
        );
    }

    #[test]
    fn test_round_numeric_midpoint_away_from_zero() {
        assert_eq!(
            round(vec![Value::Numeric(Decimal::new(25, 1))]).unwrap(),
            Value::Numeric(Decimal::new(3, 0))
        );
        assert_eq!(
            round(vec![Value::Numeric(Decimal::new(-25, 1))]).unwrap(),
            Value::Numeric(Decimal::new(-3, 0))
        );
        assert_eq!(
            round(vec![Value::Numeric(Decimal::new(125, 2)), Value::Int32(1)]).unwrap(),
            Value::Numeric(Decimal::new(13, 1))
        );
    }

    #[test]
    fn test_round_numeric_negative_precision() {
        assert_eq!(
            round(vec![
                Value::Numeric(Decimal::new(123456, 2)),
                Value::Int32(-1)
            ])
            .unwrap(),
            Value::Numeric(Decimal::new(1230, 0))
        );
        assert_eq!(
            round(vec![
                Value::Numeric(Decimal::new(-123456, 2)),
                Value::Int32(-1)
            ])
            .unwrap(),
            Value::Numeric(Decimal::new(-1230, 0))
        );
    }

    #[test]
    fn test_trunc_numeric_negative_precision() {
        assert_eq!(
            trunc(vec![
                Value::Numeric(Decimal::new(123456, 2)),
                Value::Int32(-1)
            ])
            .unwrap(),
            Value::Numeric(Decimal::new(1230, 0))
        );
        assert_eq!(
            trunc(vec![
                Value::Numeric(Decimal::new(-123456, 2)),
                Value::Int32(-1)
            ])
            .unwrap(),
            Value::Numeric(Decimal::new(-1230, 0))
        );
    }

    #[test]
    fn test_sqrt() {
        assert_eq!(
            sqrt(vec![Value::Float64(16.0)]).unwrap(),
            Value::Float64(4.0)
        );
    }

    #[test]
    fn test_power() {
        assert_eq!(
            power(vec![Value::Float64(2.0), Value::Float64(10.0)]).unwrap(),
            Value::Float64(1024.0)
        );
    }

    #[test]
    fn test_sign() {
        assert_eq!(sign(vec![Value::Int32(-5)]).unwrap(), Value::Int32(-1));
        assert_eq!(sign(vec![Value::Int32(5)]).unwrap(), Value::Int32(1));
        assert_eq!(sign(vec![Value::Int32(0)]).unwrap(), Value::Int32(0));
    }

    #[test]
    fn test_mod() {
        assert_eq!(
            modulo(vec![Value::Int32(17), Value::Int32(5)]).unwrap(),
            Value::Int32(2)
        );
    }

    #[test]
    fn test_pi() {
        assert_eq!(pi(vec![]).unwrap(), Value::Float64(std::f64::consts::PI));
    }
}
