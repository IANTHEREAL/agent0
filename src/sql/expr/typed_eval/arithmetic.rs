//! Binary and unary operator evaluation for the typed expression evaluator.
//!
//! Contains `eval_binary` (with short-circuit AND/OR), `eval_unary`,
//! bitwise operations, shift operations, and the BinaryOp → sqlparser mapping.

use crate::sql::analyzer::types::*;
use crate::sql::error::SqlError;
use crate::sql::expr::operators::eval_binary_op;
use crate::types::{Row, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::BinaryOperator;

use super::eval_typed_expr;
use crate::sql::query_context::QueryContext;

/// Evaluate a binary operation with short-circuit for AND/OR.
pub(super) fn eval_binary(
    op: &BinaryOp,
    left: &TypedExpr,
    right: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    // Short-circuit AND/OR (critical for correctness and performance)
    match op {
        BinaryOp::And => {
            let lv = eval_typed_expr(left, row, qctx)?;
            match &lv {
                Value::Boolean(false) => return Ok(Value::Boolean(false)),
                Value::Boolean(true) | Value::Null => {
                    let rv = eval_typed_expr(right, row, qctx)?;
                    return eval_binary_op(lv, &BinaryOperator::And, rv);
                }
                _ => return Err(anyhow!("AND requires boolean operands")),
            }
        }
        BinaryOp::Or => {
            let lv = eval_typed_expr(left, row, qctx)?;
            match &lv {
                Value::Boolean(true) => return Ok(Value::Boolean(true)),
                Value::Boolean(false) | Value::Null => {
                    let rv = eval_typed_expr(right, row, qctx)?;
                    return eval_binary_op(lv, &BinaryOperator::Or, rv);
                }
                _ => return Err(anyhow!("OR requires boolean operands")),
            }
        }
        _ => {}
    }

    // Eager evaluation for all other operators
    let lv = eval_typed_expr(left, row, qctx)?;
    let rv = eval_typed_expr(right, row, qctx)?;

    // For comparison operations on text, check if collation is specified
    if matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
    ) {
        use crate::sql::expr::collation_aware::{
            compare_with_collation_from_expr, extract_collation,
        };

        // Check if either operand has a collation
        if extract_collation(left).is_some() || extract_collation(right).is_some() {
            // NULL guard: any comparison with NULL yields NULL (SQL three-valued logic)
            if matches!(lv, Value::Null) || matches!(rv, Value::Null) {
                return Ok(Value::Null);
            }
            // Use collation-aware comparison
            let cmp_result = compare_with_collation_from_expr(&lv, &rv, left, right)?;
            let result = match op {
                BinaryOp::Eq => cmp_result == 0,
                BinaryOp::NotEq => cmp_result != 0,
                BinaryOp::Lt => cmp_result < 0,
                BinaryOp::LtEq => cmp_result <= 0,
                BinaryOp::Gt => cmp_result > 0,
                BinaryOp::GtEq => cmp_result >= 0,
                _ => unreachable!(),
            };
            return Ok(Value::Boolean(result));
        }
    }

    // Operators that map directly to eval_binary_op via sqlparser's BinaryOperator
    if let Some(sqlparser_op) = to_sqlparser_binary_op(op) {
        return eval_binary_op(lv, &sqlparser_op, rv);
    }

    // Operators that need special handling
    match op {
        BinaryOp::Exp => {
            // Exponentiation: delegate to POWER function from registry
            let registry = crate::sql::expr::functions::get_registry();
            match registry.get("POWER") {
                Some(f) => f(vec![lv, rv]),
                None => Err(anyhow!("POWER function not found in registry")),
            }
        }

        // Bitwise operators
        BinaryOp::BitwiseAnd => eval_bitwise_op(lv, rv, "AND", |a, b| a & b),
        BinaryOp::BitwiseOr => eval_bitwise_op(lv, rv, "OR", |a, b| a | b),
        BinaryOp::BitwiseXor => eval_bitwise_op(lv, rv, "XOR", |a, b| a ^ b),
        BinaryOp::ShiftLeft => eval_shift_op(lv, rv, "<<", true),
        BinaryOp::ShiftRight => eval_shift_op(lv, rv, ">>", false),

        // JSON/array containment: @> / <@
        BinaryOp::ArrayContains | BinaryOp::JsonContains => {
            crate::sql::expr::eval_json_access(lv, &sqlparser::ast::JsonOperator::AtArrow, rv)
        }
        BinaryOp::ArrayContainedBy | BinaryOp::JsonContainedBy => {
            crate::sql::expr::eval_json_access(lv, &sqlparser::ast::JsonOperator::ArrowAt, rv)
        }

        // JSON existence: ? / ?| / ?&
        BinaryOp::JsonExists => eval_binary_op(lv, &BinaryOperator::Custom("?".into()), rv),
        BinaryOp::JsonExistsAny => eval_binary_op(
            lv,
            &BinaryOperator::PGCustomBinaryOperator(vec!["?|".into()]),
            rv,
        ),
        BinaryOp::JsonExistsAll => eval_binary_op(
            lv,
            &BinaryOperator::PGCustomBinaryOperator(vec!["?&".into()]),
            rv,
        ),

        // Full-text search: @@
        BinaryOp::TsMatch => eval_binary_op(
            lv,
            &BinaryOperator::PGCustomBinaryOperator(vec!["@@".into()]),
            rv,
        ),

        // Custom operator escape hatch
        BinaryOp::Custom(s) => eval_binary_op(lv, &BinaryOperator::Custom(s.clone()), rv),

        // AND/OR handled above
        BinaryOp::And | BinaryOp::Or => unreachable!(),

        // All other ops should be covered by to_sqlparser_binary_op
        _ => Err(SqlError::Unsupported(format!("unsupported binary operator: {}", op)).into()),
    }
}

/// Map analyzer BinaryOp → sqlparser BinaryOperator for operators handled by eval_binary_op.
/// Returns None for operators that need special handling.
pub(super) fn to_sqlparser_binary_op(op: &BinaryOp) -> Option<BinaryOperator> {
    match op {
        // Arithmetic
        BinaryOp::Add => Some(BinaryOperator::Plus),
        BinaryOp::Sub => Some(BinaryOperator::Minus),
        BinaryOp::Mul => Some(BinaryOperator::Multiply),
        BinaryOp::Div => Some(BinaryOperator::Divide),
        BinaryOp::Mod => Some(BinaryOperator::Modulo),

        // Comparison
        BinaryOp::Eq => Some(BinaryOperator::Eq),
        BinaryOp::NotEq => Some(BinaryOperator::NotEq),
        BinaryOp::Lt => Some(BinaryOperator::Lt),
        BinaryOp::LtEq => Some(BinaryOperator::LtEq),
        BinaryOp::Gt => Some(BinaryOperator::Gt),
        BinaryOp::GtEq => Some(BinaryOperator::GtEq),

        // String
        BinaryOp::Concat => Some(BinaryOperator::StringConcat),

        // Regex
        BinaryOp::RegexMatch => Some(BinaryOperator::PGRegexMatch),
        BinaryOp::RegexIMatch => Some(BinaryOperator::PGRegexIMatch),
        BinaryOp::RegexNotMatch => Some(BinaryOperator::PGRegexNotMatch),
        BinaryOp::RegexNotIMatch => Some(BinaryOperator::PGRegexNotIMatch),

        // Array overlap
        BinaryOp::ArrayOverlap => Some(BinaryOperator::PGOverlap),

        // Everything else needs special handling
        _ => None,
    }
}

pub(super) fn eval_unary(op: &UnaryOp, val: Value) -> Result<Value> {
    if val == Value::Null {
        return Ok(Value::Null);
    }

    match op {
        UnaryOp::Not => match val {
            Value::Boolean(b) => Ok(Value::Boolean(!b)),
            _ => Err(anyhow!("NOT requires boolean operand")),
        },
        UnaryOp::Minus => match val {
            Value::Int32(i) => i.checked_neg().map(Value::Int32).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "integer out of range".into(),
                }
                .into()
            }),
            Value::Int64(i) => i.checked_neg().map(Value::Int64).ok_or_else(|| {
                SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into()
            }),
            Value::Float64(f) => Ok(Value::Float64(-f)),
            Value::Numeric(d) => Ok(Value::Numeric(-d)),
            _ => Err(anyhow!(
                "cannot negate value of type {}",
                val.type_display_name()
            )),
        },
        UnaryOp::Plus => match val {
            Value::Int32(_) | Value::Int64(_) | Value::Float64(_) | Value::Numeric(_) => Ok(val),
            _ => Err(anyhow!(
                "unary + not supported for type {}",
                val.type_display_name()
            )),
        },
        UnaryOp::BitwiseNot => match val {
            Value::Int32(i) => Ok(Value::Int32(!i)),
            Value::Int64(i) => Ok(Value::Int64(!i)),
            _ => Err(anyhow!(
                "bitwise NOT not supported for type {}",
                val.type_display_name()
            )),
        },
    }
}

/// Evaluate a bitwise binary operation (AND, OR, XOR).
///
/// Preserves the input type: if both operands are Int32, returns Int32;
/// otherwise promotes to Int64 (matching PostgreSQL behavior).
pub(super) fn eval_bitwise_op(
    left: Value,
    right: Value,
    op_name: &str,
    op_fn: fn(i64, i64) -> i64,
) -> Result<Value> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let both_i32 = matches!((&left, &right), (Value::Int32(_), Value::Int32(_)));
    let l = value_to_i64(&left).ok_or_else(|| {
        anyhow!(
            "bitwise {} not supported for type {}",
            op_name,
            left.type_display_name()
        )
    })?;
    let r = value_to_i64(&right).ok_or_else(|| {
        anyhow!(
            "bitwise {} not supported for type {}",
            op_name,
            right.type_display_name()
        )
    })?;
    let result = op_fn(l, r);
    if both_i32 {
        Ok(Value::Int32(result as i32))
    } else {
        Ok(Value::Int64(result))
    }
}

/// Evaluate a shift operation (<< or >>).
///
/// Validates the shift amount to prevent panics or masking semantics.
///
/// PostgreSQL treats shift counts as bounded by the underlying integer width
/// (int4: 0..31, int8: 0..63). Out-of-range shift counts should error.
pub(super) fn eval_shift_op(
    left: Value,
    right: Value,
    op_name: &str,
    is_left: bool,
) -> Result<Value> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let shift = value_to_i64(&right).ok_or_else(|| {
        anyhow!(
            "bitwise {} not supported for type {}",
            op_name,
            right.type_display_name()
        )
    })?;

    match left {
        Value::Int32(l) => {
            if shift < 0 || shift >= 32 {
                return Err(SqlError::NumericValueOutOfRange {
                    message: format!("shift count {} out of range for integer", shift),
                }
                .into());
            }
            let s = shift as u32;
            let result = if is_left {
                l.checked_shl(s).unwrap()
            } else {
                l.checked_shr(s).unwrap()
            };
            Ok(Value::Int32(result))
        }
        Value::Int64(l) => {
            if shift < 0 || shift >= 64 {
                return Err(SqlError::NumericValueOutOfRange {
                    message: format!("shift count {} out of range for bigint", shift),
                }
                .into());
            }
            let s = shift as u32;
            let result = if is_left {
                l.checked_shl(s).unwrap()
            } else {
                l.checked_shr(s).unwrap()
            };
            Ok(Value::Int64(result))
        }
        other => Err(anyhow!(
            "bitwise {} not supported for type {}",
            op_name,
            other.type_display_name()
        )),
    }
}

/// Extract i64 from a Value (for bitwise operations).
pub(super) fn value_to_i64(val: &Value) -> Option<i64> {
    match val {
        Value::Int32(i) => Some(*i as i64),
        Value::Int64(i) => Some(*i),
        _ => None,
    }
}
