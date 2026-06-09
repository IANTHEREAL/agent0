use crate::model::{DataType, Value};
use crate::sql::error::SqlError;
use crate::sql::expr::numeric::{self, NumericValue};
use anyhow::{anyhow, Result};
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::HashMap;

use super::SqlFn;

/// Round a Decimal to `n` significant digits (matching PostgreSQL's numeric
/// precision for transcendental functions like ln, exp, sqrt).
fn round_to_significant_digits(d: Decimal, n: u32) -> Decimal {
    if d.is_zero() {
        return d;
    }
    let abs_str = d.abs().to_string();
    // Count leading zeros (non-significant digits) in decimal part
    let dot_pos = abs_str.find('.');
    let significant_int_digits = match dot_pos {
        Some(pos) => {
            let int_part = &abs_str[..pos];
            if int_part == "0" {
                // Value < 1: count leading zeros after decimal point
                let leading_zeros =
                    abs_str[pos + 1..].chars().take_while(|c| *c == '0').count() as u32;
                // dp = n + leading_zeros (all significant digits are after the zeros)
                return d.round_dp_with_strategy(
                    n + leading_zeros,
                    RoundingStrategy::MidpointNearestEven,
                );
            }
            int_part.len() as u32
        }
        None => abs_str.len() as u32,
    };
    let dp = n.saturating_sub(significant_int_digits);
    d.round_dp_with_strategy(dp, RoundingStrategy::MidpointNearestEven)
}

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("ABS", abs);
    map.insert("CEIL", ceil);
    map.insert("CEILING", ceil);
    map.insert("FLOOR", floor);
    map.insert("ROUND", round);
    map.insert("TRUNC", trunc);
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
    map.insert("ASIN", asin);
    map.insert("ACOS", acos);
    map.insert("ATAN", atan);
    map.insert("ATAN2", atan2);
    map.insert("PI", pi);
    map.insert("WIDTH_BUCKET", width_bucket);
    map.insert("RANDOM", random);
}

fn numeric_value_out_of_range_error(message: impl Into<String>) -> anyhow::Error {
    SqlError::NumericValueOutOfRange {
        message: message.into(),
    }
    .into()
}

fn float_underflow_error() -> anyhow::Error {
    numeric_value_out_of_range_error("value out of range: underflow")
}

fn invalid_argument_for_logarithm_error(message: impl Into<String>) -> anyhow::Error {
    SqlError::InvalidArgumentForLogarithm {
        message: message.into(),
    }
    .into()
}

fn invalid_argument_for_power_function_error(message: impl Into<String>) -> anyhow::Error {
    SqlError::InvalidArgumentForPowerFunction {
        message: message.into(),
    }
    .into()
}

fn invalid_argument_for_width_bucket_error(message: impl Into<String>) -> anyhow::Error {
    SqlError::InvalidArgumentForWidthBucket {
        message: message.into(),
    }
    .into()
}

fn checked_float_exp(value: f64) -> Result<f64> {
    let result = value.exp();
    if value.is_finite() && result.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    if value.is_finite() && result == 0.0 {
        return Err(float_underflow_error());
    }
    Ok(result)
}

fn checked_float_power(base: f64, exp: f64) -> Result<f64> {
    if base == 0.0 && exp < 0.0 {
        return Err(invalid_argument_for_power_function_error(
            "zero raised to a negative power is undefined",
        ));
    }
    if !base.is_nan() && exp.is_finite() && base < 0.0 && exp.fract() != 0.0 {
        return Err(invalid_argument_for_power_function_error(
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }

    let result = base.powf(exp);
    if base.is_finite() && exp.is_finite() && result.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    if base.is_finite() && exp.is_finite() && result == 0.0 && base != 0.0 {
        return Err(float_underflow_error());
    }
    Ok(result)
}

fn checked_float_sqrt(value: f64) -> Result<f64> {
    if value < 0.0 {
        return Err(invalid_argument_for_power_function_error(
            "cannot take square root of a negative number",
        ));
    }
    let result = value.sqrt();
    if result.is_infinite() && !value.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    if result == 0.0 && value != 0.0 {
        return Err(float_underflow_error());
    }
    Ok(result)
}

fn checked_float_log(value: f64, f: impl FnOnce(f64) -> f64) -> Result<f64> {
    if value == 0.0 {
        return Err(invalid_argument_for_logarithm_error(
            "cannot take logarithm of zero",
        ));
    }
    if value < 0.0 {
        return Err(invalid_argument_for_logarithm_error(
            "cannot take logarithm of a negative number",
        ));
    }
    let result = f(value);
    if result.is_infinite() && !value.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    if result == 0.0 && value != 1.0 {
        return Err(float_underflow_error());
    }
    Ok(result)
}

fn checked_float_cbrt(value: f64) -> Result<f64> {
    let result = if value == 0.0 {
        0.0
    } else {
        value.signum() * ((value.abs().ln() / 3.0).exp())
    };
    if result.is_infinite() && !value.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    if result == 0.0 && value != 0.0 {
        return Err(float_underflow_error());
    }
    Ok(result)
}

fn checked_float_trig(
    value: f64,
    f: impl FnOnce(f64) -> f64,
    check_inf_result: bool,
) -> Result<f64> {
    if value.is_nan() {
        return Ok(f64::NAN);
    }
    let result = f(value);
    if value.is_infinite() {
        return Err(numeric_value_out_of_range_error("input is out of range"));
    }
    if check_inf_result && result.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    Ok(result)
}

fn checked_float_degrees(value: f64) -> Result<f64> {
    let result = value.to_degrees();
    if result.is_infinite() && !value.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    if result == 0.0 && value != 0.0 {
        return Err(float_underflow_error());
    }
    Ok(result)
}

fn checked_float_radians(value: f64) -> Result<f64> {
    let result = value.to_radians();
    if result.is_infinite() && !value.is_infinite() {
        return Err(numeric_value_out_of_range_error(
            "value overflows numeric format",
        ));
    }
    if result == 0.0 && value != 0.0 {
        return Err(float_underflow_error());
    }
    Ok(result)
}

fn trig_inverse_input_out_of_range(n: f64) -> bool {
    !n.is_nan() && !(-1.0..=1.0).contains(&n)
}

pub fn abs(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Int32(n)) => n
            .checked_abs()
            .map(Value::Int32)
            .ok_or_else(|| numeric_value_out_of_range_error("integer out of range")),
        Some(Value::Int64(n)) => n
            .checked_abs()
            .map(Value::Int64)
            .ok_or_else(|| numeric_value_out_of_range_error("bigint out of range")),
        Some(Value::Float64(n)) => Ok(Value::Float64(n.abs())),
        Some(Value::Numeric(d)) => Ok(Value::Numeric(d.abs())),
        _ => Ok(Value::Null),
    }
}

pub fn ceil(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.ceil())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).ceil())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).ceil())),
        Some(Value::Numeric(d)) => Ok(Value::Numeric(d.ceil())),
        _ => Ok(Value::Null),
    }
}

pub fn floor(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.floor())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).floor())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).floor())),
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

fn round_float8_pg(n: f64) -> f64 {
    // PostgreSQL float8 round follows the platform rint/nearbyint tie-to-even rule.
    n.round_ties_even()
}

pub fn round(args: Vec<Value>) -> Result<Value> {
    let has_precision = args.len() >= 2;
    let mut iter = args.into_iter();
    let val = iter.next();
    let input_type = val
        .as_ref()
        .and_then(Value::data_type)
        .unwrap_or(DataType::Unknown)
        .pg_display_name();
    let precision = match iter.next() {
        None => 0,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Int32(n)) => n,
        Some(value) => {
            let precision_type = value
                .data_type()
                .unwrap_or(DataType::Unknown)
                .pg_display_name();
            return Err(SqlError::FunctionNotFound(format!(
                "round({input_type}, {precision_type})"
            ))
            .into());
        }
    };
    match val {
        Some(Value::Float64(_)) if has_precision => {
            Err(SqlError::FunctionNotFound("round(double precision, integer)".into()).into())
        }
        Some(Value::Float64(n)) => Ok(Value::Float64(round_float8_pg(n))),
        Some(Value::Numeric(d)) => Ok(Value::Numeric(round_numeric_with_precision(d, precision)?)),
        Some(Value::Int32(n)) if has_precision => Ok(Value::Numeric(round_numeric_with_precision(
            Decimal::from(n),
            precision,
        )?)),
        Some(Value::Int64(n)) if has_precision => Ok(Value::Numeric(round_numeric_with_precision(
            Decimal::from(n),
            precision,
        )?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(round_float8_pg(n as f64))),
        Some(Value::Int64(n)) => Ok(Value::Float64(round_float8_pg(n as f64))),
        _ => Ok(Value::Null),
    }
}

pub fn trunc(args: Vec<Value>) -> Result<Value> {
    let has_precision = args.len() >= 2;
    let mut iter = args.into_iter();
    let val = iter.next();
    let input_type = val
        .as_ref()
        .and_then(Value::data_type)
        .unwrap_or(DataType::Unknown)
        .pg_display_name();
    let precision = match iter.next() {
        None => 0,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Int32(n)) => n,
        Some(value) => {
            let precision_type = value
                .data_type()
                .unwrap_or(DataType::Unknown)
                .pg_display_name();
            return Err(SqlError::FunctionNotFound(format!(
                "trunc({input_type}, {precision_type})"
            ))
            .into());
        }
    };
    match val {
        Some(Value::Float64(_)) if has_precision => {
            Err(SqlError::FunctionNotFound("trunc(double precision, integer)".into()).into())
        }
        Some(Value::Float64(n)) => {
            let factor = 10_f64.powi(precision);
            Ok(Value::Float64((n * factor).trunc() / factor))
        }
        Some(Value::Numeric(d)) => Ok(Value::Numeric(trunc_numeric_with_precision(d, precision)?)),
        Some(Value::Int32(n)) if has_precision => Ok(Value::Numeric(trunc_numeric_with_precision(
            Decimal::from(n),
            precision,
        )?)),
        Some(Value::Int64(n)) if has_precision => Ok(Value::Numeric(trunc_numeric_with_precision(
            Decimal::from(n),
            precision,
        )?)),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).trunc())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).trunc())),
        _ => Ok(Value::Null),
    }
}

pub fn sqrt(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::MathematicalOps;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_sqrt(n)?)),
        Some(Value::Int32(n)) => {
            if n < 0 {
                return Err(invalid_argument_for_power_function_error(
                    "cannot take square root of a negative number",
                ));
            }
            Ok(Value::Float64((n as f64).sqrt()))
        }
        Some(Value::Int64(n)) => {
            if n < 0 {
                return Err(invalid_argument_for_power_function_error(
                    "cannot take square root of a negative number",
                ));
            }
            Ok(Value::Float64((n as f64).sqrt()))
        }
        Some(Value::Numeric(d)) => {
            let result = d.sqrt().ok_or_else(|| {
                invalid_argument_for_power_function_error(
                    "cannot take square root of a negative number",
                )
            })?;
            Ok(Value::Numeric(round_to_significant_digits(result, 16)))
        }
        _ => Ok(Value::Null),
    }
}

pub fn cbrt(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_cbrt(n)?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(checked_float_cbrt(n as f64)?)),
        Some(Value::Int64(n)) => Ok(Value::Float64(checked_float_cbrt(n as f64)?)),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(checked_float_cbrt(n)?))
        }
        _ => Ok(Value::Null),
    }
}

pub fn power(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    use rust_decimal::MathematicalOps;

    let mut iter = args.into_iter();
    let base = iter.next().unwrap_or(Value::Null);
    let exp = iter.next().unwrap_or(Value::Null);

    if matches!(base, Value::Null) || matches!(exp, Value::Null) {
        return Ok(Value::Null);
    }

    let has_float = matches!(base, Value::Float64(_)) || matches!(exp, Value::Float64(_));
    let has_numeric = matches!(base, Value::Numeric(_)) || matches!(exp, Value::Numeric(_));

    if has_numeric && !has_float {
        let base = match base {
            Value::Int32(n) => Decimal::from(n),
            Value::Int64(n) => Decimal::from(n),
            Value::Numeric(d) => d,
            _ => return Ok(Value::Null),
        };
        let exp = match exp {
            Value::Int32(n) => Decimal::from(n),
            Value::Int64(n) => Decimal::from(n),
            Value::Numeric(d) => d,
            _ => return Ok(Value::Null),
        };
        if base.is_zero() && exp.is_sign_negative() {
            return Err(invalid_argument_for_power_function_error(
                "zero raised to a negative power is undefined",
            ));
        }
        if base.is_sign_negative() && !exp.fract().is_zero() {
            return Err(invalid_argument_for_power_function_error(
                "a negative number raised to a non-integer power yields a complex result",
            ));
        }
        let result = base
            .checked_powd(exp)
            .ok_or_else(|| numeric_value_out_of_range_error("value overflows numeric format"))?;
        return Ok(Value::Numeric(round_to_significant_digits(result, 16)));
    }

    let base = match base {
        Value::Float64(n) => n,
        Value::Int32(n) => n as f64,
        Value::Int64(n) => n as f64,
        Value::Numeric(d) => d
            .to_f64()
            .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?,
        _ => return Ok(Value::Null),
    };
    let exp = match exp {
        Value::Float64(n) => n,
        Value::Int32(n) => n as f64,
        Value::Int64(n) => n as f64,
        Value::Numeric(d) => d
            .to_f64()
            .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?,
        _ => return Ok(Value::Null),
    };
    Ok(Value::Float64(checked_float_power(base, exp)?))
}

pub fn exp(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::MathematicalOps;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_exp(n)?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(checked_float_exp(n as f64)?)),
        Some(Value::Int64(n)) => Ok(Value::Float64(checked_float_exp(n as f64)?)),
        Some(Value::Numeric(d)) => {
            let result = d.checked_exp().ok_or_else(|| {
                numeric_value_out_of_range_error("value overflows numeric format")
            })?;
            Ok(Value::Numeric(round_to_significant_digits(result, 16)))
        }
        _ => Ok(Value::Null),
    }
}

pub fn ln(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::MathematicalOps;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_log(n, f64::ln)?)),
        Some(Value::Int32(n)) => {
            if n == 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of zero",
                ));
            }
            if n < 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of a negative number",
                ));
            }
            Ok(Value::Float64((n as f64).ln()))
        }
        Some(Value::Int64(n)) => {
            if n == 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of zero",
                ));
            }
            if n < 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of a negative number",
                ));
            }
            Ok(Value::Float64((n as f64).ln()))
        }
        Some(Value::Numeric(d)) => {
            if d.is_zero() {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of zero",
                ));
            }
            if d.is_sign_negative() {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of a negative number",
                ));
            }
            let result = d.checked_ln().ok_or_else(|| {
                numeric_value_out_of_range_error("value overflows numeric format")
            })?;
            Ok(Value::Numeric(round_to_significant_digits(result, 16)))
        }
        _ => Ok(Value::Null),
    }
}

pub fn log10(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::Decimal;
    use rust_decimal::MathematicalOps;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_log(n, f64::log10)?)),
        Some(Value::Int32(n)) => {
            if n == 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of zero",
                ));
            }
            if n < 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of a negative number",
                ));
            }
            Ok(Value::Float64((n as f64).log10()))
        }
        Some(Value::Int64(n)) => {
            if n == 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of zero",
                ));
            }
            if n < 0 {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of a negative number",
                ));
            }
            Ok(Value::Float64((n as f64).log10()))
        }
        Some(Value::Numeric(d)) => {
            // log10(x) = ln(x) / ln(10)
            if d.is_zero() {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of zero",
                ));
            }
            if d.is_sign_negative() {
                return Err(invalid_argument_for_logarithm_error(
                    "cannot take logarithm of a negative number",
                ));
            }
            let ln_val = d.checked_ln().ok_or_else(|| {
                numeric_value_out_of_range_error("value overflows numeric format")
            })?;
            let ln_10 = Decimal::TEN.ln(); // constant, always safe
            let result = crate::sql::expr::numeric::checked_decimal_div(ln_val, ln_10)?;
            Ok(Value::Numeric(round_to_significant_digits(result, 16)))
        }
        _ => Ok(Value::Null),
    }
}

pub fn sign(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Int32(n)) => Ok(Value::Float64(if n > 0 {
            1.0
        } else if n < 0 {
            -1.0
        } else {
            0.0
        })),
        Some(Value::Int64(n)) => Ok(Value::Float64(if n > 0 {
            1.0
        } else if n < 0 {
            -1.0
        } else {
            0.0
        })),
        Some(Value::Float64(n)) => Ok(Value::Float64(if n > 0.0 {
            1.0
        } else if n < 0.0 {
            -1.0
        } else {
            0.0
        })),
        Some(Value::Numeric(d)) => Ok(Value::Numeric(if d > Decimal::ZERO {
            Decimal::ONE
        } else if d < Decimal::ZERO {
            Decimal::new(-1, 0)
        } else {
            Decimal::ZERO
        })),
        _ => Ok(Value::Null),
    }
}

pub fn modulo(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("MOD requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let left = iter.next().expect("checked arity");
    let right = iter.next().expect("checked arity");

    if matches!((&left, &right), (Value::Null, _) | (_, Value::Null)) {
        return Ok(Value::Null);
    }
    if matches!(
        (&left, &right),
        (Value::Float64(_), _) | (_, Value::Float64(_))
    ) {
        return Err(SqlError::Unsupported("Unsupported types for modulo".into()).into());
    }

    let (Some(left), Some(right)) = (
        NumericValue::from_value(&left),
        NumericValue::from_value(&right),
    ) else {
        return Err(SqlError::Unsupported("Unsupported types for modulo".into()).into());
    };
    Ok(numeric::numeric_mod(left, right)?.into_value())
}

pub fn degrees(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_degrees(n)?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(checked_float_degrees(n as f64)?)),
        Some(Value::Int64(n)) => Ok(Value::Float64(checked_float_degrees(n as f64)?)),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(checked_float_degrees(n)?))
        }
        _ => Ok(Value::Null),
    }
}

pub fn radians(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_radians(n)?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(checked_float_radians(n as f64)?)),
        Some(Value::Int64(n)) => Ok(Value::Float64(checked_float_radians(n as f64)?)),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(checked_float_radians(n)?))
        }
        _ => Ok(Value::Null),
    }
}

pub fn sin(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_trig(n, f64::sin, true)?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(checked_float_trig(
            n as f64,
            f64::sin,
            true,
        )?)),
        Some(Value::Int64(n)) => Ok(Value::Float64(checked_float_trig(
            n as f64,
            f64::sin,
            true,
        )?)),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(checked_float_trig(n, f64::sin, true)?))
        }
        _ => Ok(Value::Null),
    }
}

pub fn cos(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_trig(n, f64::cos, true)?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(checked_float_trig(
            n as f64,
            f64::cos,
            true,
        )?)),
        Some(Value::Int64(n)) => Ok(Value::Float64(checked_float_trig(
            n as f64,
            f64::cos,
            true,
        )?)),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(checked_float_trig(n, f64::cos, true)?))
        }
        _ => Ok(Value::Null),
    }
}

pub fn tan(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(checked_float_trig(n, f64::tan, false)?)),
        Some(Value::Int32(n)) => Ok(Value::Float64(checked_float_trig(
            n as f64,
            f64::tan,
            false,
        )?)),
        Some(Value::Int64(n)) => Ok(Value::Float64(checked_float_trig(
            n as f64,
            f64::tan,
            false,
        )?)),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(checked_float_trig(n, f64::tan, false)?))
        }
        _ => Ok(Value::Null),
    }
}

pub fn asin(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => {
            if trig_inverse_input_out_of_range(n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64(n.asin()))
        }
        Some(Value::Int32(n)) => {
            if !(-1..=1).contains(&n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64((n as f64).asin()))
        }
        Some(Value::Int64(n)) => {
            if !(-1..=1).contains(&n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64((n as f64).asin()))
        }
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            if trig_inverse_input_out_of_range(n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64(n.asin()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn acos(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => {
            if trig_inverse_input_out_of_range(n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64(n.acos()))
        }
        Some(Value::Int32(n)) => {
            if !(-1..=1).contains(&n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64((n as f64).acos()))
        }
        Some(Value::Int64(n)) => {
            if !(-1..=1).contains(&n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64((n as f64).acos()))
        }
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            if trig_inverse_input_out_of_range(n) {
                return Err(numeric_value_out_of_range_error("input is out of range"));
            }
            Ok(Value::Float64(n.acos()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn atan(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    match args.into_iter().next() {
        Some(Value::Float64(n)) => Ok(Value::Float64(n.atan())),
        Some(Value::Int32(n)) => Ok(Value::Float64((n as f64).atan())),
        Some(Value::Int64(n)) => Ok(Value::Float64((n as f64).atan())),
        Some(Value::Numeric(d)) => {
            let n = d
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?;
            Ok(Value::Float64(n.atan()))
        }
        _ => Ok(Value::Null),
    }
}

pub fn atan2(args: Vec<Value>) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;
    let mut iter = args.into_iter();
    let y = match iter.next() {
        Some(Value::Float64(n)) => Some(n),
        Some(Value::Int32(n)) => Some(n as f64),
        Some(Value::Int64(n)) => Some(n as f64),
        Some(Value::Numeric(d)) => Some(
            d.to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?,
        ),
        _ => None,
    };
    let x = match iter.next() {
        Some(Value::Float64(n)) => Some(n),
        Some(Value::Int32(n)) => Some(n as f64),
        Some(Value::Int64(n)) => Some(n as f64),
        Some(Value::Numeric(d)) => Some(
            d.to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range for double precision"))?,
        ),
        _ => None,
    };
    match (y, x) {
        (Some(y), Some(x)) => Ok(Value::Float64(y.atan2(x))),
        _ => Ok(Value::Null),
    }
}

fn width_bucket_float(operand: Value, low: Value, high: Value, count: Value) -> Result<Value> {
    fn numeric_value_as_f64(value: Value) -> Result<Option<f64>> {
        use rust_decimal::prelude::ToPrimitive;
        match value {
            Value::Null => Ok(None),
            Value::Float64(n) => Ok(Some(n)),
            Value::Int32(n) => Ok(Some(n as f64)),
            Value::Int64(n) => Ok(Some(n as f64)),
            Value::Numeric(d) => d
                .to_f64()
                .map(Some)
                .ok_or_else(|| anyhow!("numeric value out of range for double precision")),
            _ => Ok(None),
        }
    }
    fn integer_value_as_i32(value: Value) -> Result<Option<i32>> {
        match value {
            Value::Null => Ok(None),
            Value::Int32(n) => Ok(Some(n)),
            Value::Int64(n) => {
                let narrowed = i32::try_from(n)
                    .map_err(|_| numeric_value_out_of_range_error("integer out of range"))?;
                Ok(Some(narrowed))
            }
            _ => Ok(None),
        }
    }

    let operand = numeric_value_as_f64(operand)?;
    let low = numeric_value_as_f64(low)?;
    let high = numeric_value_as_f64(high)?;
    let count = integer_value_as_i32(count)?;
    let (Some(operand), Some(low), Some(high), Some(count)) = (operand, low, high, count) else {
        return Ok(Value::Null);
    };

    if operand.is_nan() || low.is_nan() || high.is_nan() {
        return Err(invalid_argument_for_width_bucket_error(
            "operand, lower bound, and upper bound cannot be NaN",
        ));
    }
    if !low.is_finite() || !high.is_finite() {
        return Err(invalid_argument_for_width_bucket_error(
            "lower and upper bounds must be finite",
        ));
    }
    if count <= 0 {
        return Err(invalid_argument_for_width_bucket_error(
            "count must be greater than zero",
        ));
    }
    if low == high {
        return Err(invalid_argument_for_width_bucket_error(
            "lower bound cannot equal upper bound",
        ));
    }

    let count_f64 = f64::from(count);
    let bucket = if low < high {
        if operand < low {
            0_i64
        } else if operand >= high {
            i64::from(count) + 1
        } else {
            let quotient = if (high - low).is_infinite() {
                (operand / 2.0 - low / 2.0) / (high / 2.0 - low / 2.0)
            } else {
                (operand - low) / (high - low)
            };
            let bucket = (count_f64 * quotient).floor() as i64;
            bucket.min(i64::from(count - 1)) + 1
        }
    } else if operand > low {
        0_i64
    } else if operand <= high {
        i64::from(count) + 1
    } else {
        let quotient = if (low - high).is_infinite() {
            (low / 2.0 - operand / 2.0) / (low / 2.0 - high / 2.0)
        } else {
            (low - operand) / (low - high)
        };
        let bucket = (count_f64 * quotient).floor() as i64;
        bucket.min(i64::from(count - 1)) + 1
    };
    let bucket = i32::try_from(bucket)
        .map_err(|_| numeric_value_out_of_range_error("integer out of range"))?;

    Ok(Value::Int32(bucket))
}

fn width_bucket_numeric(operand: Value, low: Value, high: Value, count: Value) -> Result<Value> {
    use rust_decimal::prelude::ToPrimitive;

    fn numeric_value_as_decimal(value: Value) -> Result<Option<Decimal>> {
        match value {
            Value::Null => Ok(None),
            Value::Int32(n) => Ok(Some(Decimal::from(n))),
            Value::Int64(n) => Ok(Some(Decimal::from(n))),
            Value::Numeric(d) => Ok(Some(d)),
            _ => Ok(None),
        }
    }
    fn integer_value_as_i32(value: Value) -> Result<Option<i32>> {
        match value {
            Value::Null => Ok(None),
            Value::Int32(n) => Ok(Some(n)),
            Value::Int64(n) => {
                let narrowed = i32::try_from(n)
                    .map_err(|_| numeric_value_out_of_range_error("integer out of range"))?;
                Ok(Some(narrowed))
            }
            _ => Ok(None),
        }
    }
    fn bucket_to_i32(bucket: Decimal) -> Result<i32> {
        let bucket = bucket
            .to_i64()
            .ok_or_else(|| numeric_value_out_of_range_error("integer out of range"))?;
        i32::try_from(bucket).map_err(|_| numeric_value_out_of_range_error("integer out of range"))
    }

    let operand = numeric_value_as_decimal(operand)?;
    let low = numeric_value_as_decimal(low)?;
    let high = numeric_value_as_decimal(high)?;
    let count = integer_value_as_i32(count)?;
    let (Some(operand), Some(low), Some(high), Some(count)) = (operand, low, high, count) else {
        return Ok(Value::Null);
    };

    if count <= 0 {
        return Err(invalid_argument_for_width_bucket_error(
            "count must be greater than zero",
        ));
    }
    if low == high {
        return Err(invalid_argument_for_width_bucket_error(
            "lower bound cannot equal upper bound",
        ));
    }

    let count_decimal = Decimal::from(count);
    let bucket = if low < high {
        if operand < low {
            Decimal::ZERO
        } else if operand >= high {
            count_decimal + Decimal::ONE
        } else {
            let numerator = operand
                .checked_sub(low)
                .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?;
            let scaled = numerator
                .checked_mul(count_decimal)
                .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?;
            let denominator = high
                .checked_sub(low)
                .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?;
            scaled
                .checked_div(denominator)
                .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?
                .floor()
                + Decimal::ONE
        }
    } else if operand > low {
        Decimal::ZERO
    } else if operand <= high {
        count_decimal + Decimal::ONE
    } else {
        let numerator = low
            .checked_sub(operand)
            .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?;
        let scaled = numerator
            .checked_mul(count_decimal)
            .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?;
        let denominator = low
            .checked_sub(high)
            .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?;
        scaled
            .checked_div(denominator)
            .ok_or_else(|| numeric_value_out_of_range_error("numeric value out of range"))?
            .floor()
            + Decimal::ONE
    };

    Ok(Value::Int32(bucket_to_i32(bucket)?))
}

pub fn width_bucket(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let operand = iter.next().unwrap_or(Value::Null);
    let low = iter.next().unwrap_or(Value::Null);
    let high = iter.next().unwrap_or(Value::Null);
    let count = iter.next().unwrap_or(Value::Null);

    if matches!(operand, Value::Float64(_))
        || matches!(low, Value::Float64(_))
        || matches!(high, Value::Float64(_))
    {
        return width_bucket_float(operand, low, high, count);
    }

    if matches!(operand, Value::Numeric(_))
        || matches!(low, Value::Numeric(_))
        || matches!(high, Value::Numeric(_))
    {
        return width_bucket_numeric(operand, low, high, count);
    }

    width_bucket_float(operand, low, high, count)
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
        let int_min_err = abs(vec![Value::Int32(i32::MIN)]).unwrap_err();
        assert_eq!(int_min_err.to_string(), "integer out of range");
        assert!(matches!(
            int_min_err.downcast_ref::<SqlError>(),
            Some(SqlError::NumericValueOutOfRange { .. })
        ));
        let bigint_min_err = abs(vec![Value::Int64(i64::MIN)]).unwrap_err();
        assert_eq!(bigint_min_err.to_string(), "bigint out of range");
        assert!(matches!(
            bigint_min_err.downcast_ref::<SqlError>(),
            Some(SqlError::NumericValueOutOfRange { .. })
        ));
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
    fn test_round_float8_midpoint_ties_to_even() {
        assert_eq!(
            round(vec![Value::Float64(2.5)]).unwrap(),
            Value::Float64(2.0)
        );
        assert_eq!(
            round(vec![Value::Float64(3.5)]).unwrap(),
            Value::Float64(4.0)
        );
        assert_eq!(
            round(vec![Value::Float64(-2.5)]).unwrap(),
            Value::Float64(-2.0)
        );
        assert_eq!(
            round(vec![Value::Float64(-3.5)]).unwrap(),
            Value::Float64(-4.0)
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
    fn test_round_trunc_pg_precision_signatures() {
        assert_eq!(
            round(vec![Value::Float64(12.34)]).unwrap(),
            Value::Float64(12.0)
        );
        assert_eq!(
            round(vec![Value::Numeric(Decimal::new(1234, 2)), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            trunc(vec![Value::Float64(12.34)]).unwrap(),
            Value::Float64(12.0)
        );
        assert_eq!(
            trunc(vec![Value::Numeric(Decimal::new(1234, 2)), Value::Null]).unwrap(),
            Value::Null
        );
        let round_err = round(vec![Value::Float64(12.34), Value::Int32(1)]).unwrap_err();
        assert!(matches!(
            round_err.downcast_ref::<SqlError>(),
            Some(SqlError::FunctionNotFound(_))
        ));
        assert_eq!(
            round_err.to_string(),
            "function round(double precision, integer) does not exist"
        );

        let trunc_err = trunc(vec![Value::Float64(12.34), Value::Int32(1)]).unwrap_err();
        assert!(matches!(
            trunc_err.downcast_ref::<SqlError>(),
            Some(SqlError::FunctionNotFound(_))
        ));
        assert_eq!(
            trunc_err.to_string(),
            "function trunc(double precision, integer) does not exist"
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
        assert_eq!(
            power(vec![Value::Float64(-2.0), Value::Float64(3.0)]).unwrap(),
            Value::Float64(-8.0)
        );
    }

    #[test]
    fn test_sign() {
        assert_eq!(sign(vec![Value::Int32(-5)]).unwrap(), Value::Float64(-1.0));
        assert_eq!(sign(vec![Value::Int32(5)]).unwrap(), Value::Float64(1.0));
        assert_eq!(sign(vec![Value::Int32(0)]).unwrap(), Value::Float64(0.0));
        assert_eq!(sign(vec![Value::Int64(-5)]).unwrap(), Value::Float64(-1.0));
        assert_eq!(sign(vec![Value::Int64(5)]).unwrap(), Value::Float64(1.0));
        assert_eq!(sign(vec![Value::Int64(0)]).unwrap(), Value::Float64(0.0));
        assert_eq!(
            sign(vec![Value::Float64(-2.5)]).unwrap(),
            Value::Float64(-1.0)
        );
        assert_eq!(
            sign(vec![Value::Float64(2.5)]).unwrap(),
            Value::Float64(1.0)
        );
        assert_eq!(
            sign(vec![Value::Float64(0.0)]).unwrap(),
            Value::Float64(0.0)
        );
        assert_eq!(
            sign(vec![Value::Numeric(Decimal::new(-425, 1))]).unwrap(),
            Value::Numeric(Decimal::new(-1, 0))
        );
        assert_eq!(
            sign(vec![Value::Numeric(Decimal::ZERO)]).unwrap(),
            Value::Numeric(Decimal::ZERO)
        );
        assert_eq!(
            sign(vec![Value::Numeric(Decimal::new(425, 1))]).unwrap(),
            Value::Numeric(Decimal::ONE)
        );
    }

    #[test]
    fn test_mod() {
        assert_eq!(
            modulo(vec![Value::Int32(17), Value::Int32(5)]).unwrap(),
            Value::Int32(2)
        );
        assert_eq!(
            modulo(vec![Value::Int32(i32::MIN), Value::Int32(-1)]).unwrap(),
            Value::Int32(0)
        );
        assert_eq!(
            modulo(vec![Value::Int64(i64::MIN), Value::Int64(-1)]).unwrap(),
            Value::Int64(0)
        );
        assert_eq!(
            modulo(vec![Value::Int32(5), Value::Int64(2)]).unwrap(),
            Value::Int64(1)
        );
        assert_eq!(
            modulo(vec![Value::Int64(5), Value::Int32(2)]).unwrap(),
            Value::Int64(1)
        );
        assert_eq!(
            modulo(vec![
                Value::Numeric(Decimal::new(17, 0)),
                Value::Numeric(Decimal::new(5, 0))
            ])
            .unwrap(),
            Value::Numeric(Decimal::new(2, 0))
        );
        assert!(modulo(vec![Value::Float64(1.0), Value::Float64(0.5)]).is_err());
        assert_eq!(
            modulo(vec![Value::Int64(i64::MIN), Value::Int32(-1)]).unwrap(),
            Value::Int64(0)
        );
        let err = modulo(vec![Value::Int32(17), Value::Int32(0)]).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<SqlError>(),
            Some(SqlError::DivisionByZero)
        ));
        assert_eq!(err.to_string(), "division by zero");
    }

    #[test]
    fn test_trig_inverse_and_atan2() {
        assert_eq!(
            asin(vec![Value::Float64(0.5)]).unwrap(),
            Value::Float64(0.5_f64.asin())
        );
        assert_eq!(
            acos(vec![Value::Float64(0.5)]).unwrap(),
            Value::Float64(0.5_f64.acos())
        );
        assert_eq!(
            atan(vec![Value::Float64(1.0)]).unwrap(),
            Value::Float64(1.0_f64.atan())
        );
        assert_eq!(
            atan2(vec![Value::Float64(1.0), Value::Float64(1.0)]).unwrap(),
            Value::Float64(1.0_f64.atan2(1.0))
        );
    }

    #[test]
    fn test_float_domain_errors_match_pg_contracts() {
        let sqrt_err = sqrt(vec![Value::Float64(-1.0)]).unwrap_err();
        let sqrt_err = sqrt_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            sqrt_err,
            SqlError::InvalidArgumentForPowerFunction { .. }
        ));
        assert_eq!(sqrt_err.sqlstate(), "2201F");

        let ln_err = ln(vec![Value::Float64(-1.0)]).unwrap_err();
        let ln_err = ln_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            ln_err,
            SqlError::InvalidArgumentForLogarithm { .. }
        ));
        assert_eq!(ln_err.sqlstate(), "2201E");

        let ln_zero_err = ln(vec![Value::Float64(0.0)]).unwrap_err();
        let ln_zero_err = ln_zero_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            ln_zero_err,
            SqlError::InvalidArgumentForLogarithm { .. }
        ));
        assert_eq!(ln_zero_err.sqlstate(), "2201E");

        let log10_err = log10(vec![Value::Float64(0.0)]).unwrap_err();
        let log10_err = log10_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            log10_err,
            SqlError::InvalidArgumentForLogarithm { .. }
        ));
        assert_eq!(log10_err.sqlstate(), "2201E");

        let asin_err = asin(vec![Value::Float64(2.0)]).unwrap_err();
        let asin_err = asin_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(asin_err, SqlError::NumericValueOutOfRange { .. }));
        assert_eq!(asin_err.sqlstate(), "22003");

        for value in [f64::INFINITY, f64::NEG_INFINITY] {
            let err = asin(vec![Value::Float64(value)]).unwrap_err();
            let err = err.downcast_ref::<SqlError>().unwrap();
            assert!(matches!(err, SqlError::NumericValueOutOfRange { .. }));
            assert_eq!(err.sqlstate(), "22003");
        }
        assert!(matches!(
            asin(vec![Value::Float64(f64::NAN)]).unwrap(),
            Value::Float64(value) if value.is_nan()
        ));

        let acos_err = acos(vec![Value::Float64(-2.0)]).unwrap_err();
        let acos_err = acos_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(acos_err, SqlError::NumericValueOutOfRange { .. }));
        assert_eq!(acos_err.sqlstate(), "22003");

        for value in [f64::INFINITY, f64::NEG_INFINITY] {
            let err = acos(vec![Value::Float64(value)]).unwrap_err();
            let err = err.downcast_ref::<SqlError>().unwrap();
            assert!(matches!(err, SqlError::NumericValueOutOfRange { .. }));
            assert_eq!(err.sqlstate(), "22003");
        }
        assert!(matches!(
            acos(vec![Value::Float64(f64::NAN)]).unwrap(),
            Value::Float64(value) if value.is_nan()
        ));

        let exp_err = exp(vec![Value::Float64(1000.0)]).unwrap_err();
        let exp_err = exp_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(exp_err, SqlError::NumericValueOutOfRange { .. }));
        assert_eq!(exp_err.sqlstate(), "22003");
        assert_eq!(exp_err.to_string(), "value overflows numeric format");
        assert_eq!(
            exp(vec![Value::Float64(f64::INFINITY)]).unwrap(),
            Value::Float64(f64::INFINITY)
        );
        assert_eq!(
            exp(vec![Value::Float64(f64::NEG_INFINITY)]).unwrap(),
            Value::Float64(0.0)
        );
        let exp_underflow_err = exp(vec![Value::Float64(-100_000.0)]).unwrap_err();
        let exp_underflow_err = exp_underflow_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            exp_underflow_err,
            SqlError::NumericValueOutOfRange { .. }
        ));
        assert_eq!(exp_underflow_err.sqlstate(), "22003");
        assert_eq!(
            exp_underflow_err.to_string(),
            "value out of range: underflow"
        );

        let sin_inf_err = sin(vec![Value::Float64(f64::INFINITY)]).unwrap_err();
        let sin_inf_err = sin_inf_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            sin_inf_err,
            SqlError::NumericValueOutOfRange { .. }
        ));
        assert_eq!(sin_inf_err.sqlstate(), "22003");

        let degrees_overflow_err = degrees(vec![Value::Float64(1e308)]).unwrap_err();
        let degrees_overflow_err = degrees_overflow_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            degrees_overflow_err,
            SqlError::NumericValueOutOfRange { .. }
        ));
        assert_eq!(degrees_overflow_err.sqlstate(), "22003");

        let radians_underflow_err = radians(vec![Value::Float64(5e-324)]).unwrap_err();
        let radians_underflow_err = radians_underflow_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            radians_underflow_err,
            SqlError::NumericValueOutOfRange { .. }
        ));
        assert_eq!(radians_underflow_err.sqlstate(), "22003");

        let power_domain_err = power(vec![Value::Float64(-1.0), Value::Float64(0.5)]).unwrap_err();
        let power_domain_err = power_domain_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            power_domain_err,
            SqlError::InvalidArgumentForPowerFunction { .. }
        ));
        assert_eq!(power_domain_err.sqlstate(), "2201F");
        assert_eq!(
            power_domain_err.to_string(),
            "a negative number raised to a non-integer power yields a complex result"
        );

        let power_negative_infinity_domain_err =
            power(vec![Value::Float64(f64::NEG_INFINITY), Value::Float64(0.5)]).unwrap_err();
        let power_negative_infinity_domain_err = power_negative_infinity_domain_err
            .downcast_ref::<SqlError>()
            .unwrap();
        assert!(matches!(
            power_negative_infinity_domain_err,
            SqlError::InvalidArgumentForPowerFunction { .. }
        ));
        assert_eq!(power_negative_infinity_domain_err.sqlstate(), "2201F");

        let zero_negative_power_err =
            power(vec![Value::Float64(0.0), Value::Float64(-1.0)]).unwrap_err();
        let zero_negative_power_err = zero_negative_power_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            zero_negative_power_err,
            SqlError::InvalidArgumentForPowerFunction { .. }
        ));
        assert_eq!(zero_negative_power_err.sqlstate(), "2201F");
        assert_eq!(
            zero_negative_power_err.to_string(),
            "zero raised to a negative power is undefined"
        );

        let power_overflow_err =
            power(vec![Value::Float64(10.0), Value::Float64(400.0)]).unwrap_err();
        let power_overflow_err = power_overflow_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            power_overflow_err,
            SqlError::NumericValueOutOfRange { .. }
        ));
        assert_eq!(power_overflow_err.sqlstate(), "22003");
        assert_eq!(
            power_overflow_err.to_string(),
            "value overflows numeric format"
        );
        let power_underflow_err =
            power(vec![Value::Float64(10.0), Value::Float64(-100_000.0)]).unwrap_err();
        let power_underflow_err = power_underflow_err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(
            power_underflow_err,
            SqlError::NumericValueOutOfRange { .. }
        ));
        assert_eq!(power_underflow_err.sqlstate(), "22003");
        assert_eq!(
            power(vec![Value::Float64(f64::INFINITY), Value::Float64(2.0)]).unwrap(),
            Value::Float64(f64::INFINITY)
        );
    }

    #[test]
    fn test_width_bucket() {
        assert_eq!(
            width_bucket(vec![
                Value::Float64(12.3456),
                Value::Float64(0.0),
                Value::Float64(20.0),
                Value::Int32(5)
            ])
            .unwrap(),
            Value::Int32(4)
        );
        assert_eq!(
            width_bucket(vec![
                Value::Float64(f64::INFINITY),
                Value::Float64(0.0),
                Value::Float64(10.0),
                Value::Int32(4)
            ])
            .unwrap(),
            Value::Int32(5)
        );
        assert_eq!(
            width_bucket(vec![
                Value::Float64(f64::NEG_INFINITY),
                Value::Float64(0.0),
                Value::Float64(10.0),
                Value::Int32(4)
            ])
            .unwrap(),
            Value::Int32(0)
        );
        assert_eq!(
            width_bucket(vec![
                Value::Float64(f64::INFINITY),
                Value::Float64(10.0),
                Value::Float64(0.0),
                Value::Int32(4)
            ])
            .unwrap(),
            Value::Int32(0)
        );
        assert_eq!(
            width_bucket(vec![
                Value::Float64(f64::NEG_INFINITY),
                Value::Float64(10.0),
                Value::Float64(0.0),
                Value::Int32(4)
            ])
            .unwrap(),
            Value::Int32(5)
        );
        assert_eq!(
            width_bucket(vec![
                Value::Float64(0.0),
                Value::Float64(-1.7e308),
                Value::Float64(1.7e308),
                Value::Int32(10)
            ])
            .unwrap(),
            Value::Int32(6)
        );
        assert_eq!(
            width_bucket(vec![
                Value::Float64(1e308),
                Value::Float64(-1.7e308),
                Value::Float64(1.7e308),
                Value::Int32(10)
            ])
            .unwrap(),
            Value::Int32(8)
        );
    }

    #[test]
    fn test_width_bucket_numeric_preserves_precision() {
        assert_eq!(
            width_bucket(vec![
                Value::Numeric(Decimal::new(49_999_999_999_999_999, 17)),
                Value::Numeric(Decimal::new(0, 0)),
                Value::Numeric(Decimal::new(1, 0)),
                Value::Int32(2),
            ])
            .unwrap(),
            Value::Int32(1)
        );
    }

    #[test]
    fn test_width_bucket_invalid_arguments_match_pg_sqlstates() {
        for (args, expected) in [
            (
                vec![
                    Value::Float64(f64::NAN),
                    Value::Float64(0.0),
                    Value::Float64(10.0),
                    Value::Int32(4),
                ],
                "operand, lower bound, and upper bound cannot be NaN",
            ),
            (
                vec![
                    Value::Float64(5.0),
                    Value::Float64(f64::NEG_INFINITY),
                    Value::Float64(10.0),
                    Value::Int32(4),
                ],
                "lower and upper bounds must be finite",
            ),
            (
                vec![
                    Value::Float64(5.0),
                    Value::Float64(0.0),
                    Value::Float64(10.0),
                    Value::Int32(0),
                ],
                "count must be greater than zero",
            ),
            (
                vec![
                    Value::Float64(5.0),
                    Value::Float64(0.0),
                    Value::Float64(0.0),
                    Value::Int32(4),
                ],
                "lower bound cannot equal upper bound",
            ),
        ] {
            let err = width_bucket(args).unwrap_err();
            let err = err.downcast_ref::<SqlError>().unwrap();
            assert!(matches!(
                err,
                SqlError::InvalidArgumentForWidthBucket { .. }
            ));
            assert_eq!(err.sqlstate(), "2201G");
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn test_width_bucket_bigint_count_overflow_returns_22003() {
        let err = width_bucket(vec![
            Value::Float64(5.0),
            Value::Float64(0.0),
            Value::Float64(10.0),
            Value::Int64(i64::from(i32::MAX) + 1),
        ])
        .unwrap_err();
        let err = err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(err, SqlError::NumericValueOutOfRange { .. }));
        assert_eq!(err.sqlstate(), "22003");
    }

    #[test]
    fn test_width_bucket_result_overflow_returns_22003() {
        let err = width_bucket(vec![
            Value::Float64(10.0),
            Value::Float64(0.0),
            Value::Float64(1.0),
            Value::Int32(i32::MAX),
        ])
        .unwrap_err();
        let err = err.downcast_ref::<SqlError>().unwrap();
        assert!(matches!(err, SqlError::NumericValueOutOfRange { .. }));
        assert_eq!(err.sqlstate(), "22003");
    }

    #[test]
    fn test_pi() {
        assert_eq!(pi(vec![]).unwrap(), Value::Float64(std::f64::consts::PI));
    }
}
