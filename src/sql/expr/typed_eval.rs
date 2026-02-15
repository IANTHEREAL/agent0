//! Typed expression evaluator.
//!
//! `eval_typed_expr` evaluates a `TypedExpr` against a `Row`, producing a `Value`.
//! All name resolution and type checking happened at analysis time — this is the
//! new hot-path evaluator that replaces the legacy `eval_expr`.
//!
//! Lives in `src/sql/expr/` alongside the legacy evaluator because it is
//! **runtime evaluation code**, not static analysis. The analyzer module
//! (`src/sql/analyzer/`) is purely for name resolution and type checking.
//!
//! Context-dependent builtins (NOW, CURRENT_DATE, PG_BACKEND_PID, etc.) read
//! session state from the explicit `QueryContext` parameter.

use crate::sql::analyzer::types::*;
use crate::sql::error::SqlError;
use crate::sql::expr::operators::{compare_values, eval_binary_op};
use crate::sql::query_context::QueryContext;
use crate::sql::types::cast;
use crate::types::{Row, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::BinaryOperator;

/// Evaluate a typed expression against a row.
///
/// All name resolution was done at analysis time — `ColumnRef` uses positional
/// indices. All type checking was done — no runtime type inference needed.
/// This function is sync and never touches the catalog.
///
/// `qctx` provides session state for context-dependent builtins (NOW,
/// CURRENT_DATE, PG_BACKEND_PID, etc.).
///
/// Uses `stacker::maybe_grow` to protect against stack overflow on deeply
/// nested expressions (matching the legacy `eval_expr` pattern).
pub fn eval_typed_expr(expr: &TypedExpr, row: &Row, qctx: &QueryContext) -> Result<Value> {
    stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
        eval_typed_expr_inner(expr, row, qctx)
    })
}

fn eval_typed_expr_inner(expr: &TypedExpr, row: &Row, qctx: &QueryContext) -> Result<Value> {
    match &expr.kind {
        // ── Leaf nodes ──────────────────────────────────────
        TypedExprKind::Constant(v) => Ok(v.clone()),

        TypedExprKind::ColumnRef {
            column_index,
            scope_depth,
            column_name,
        } => {
            if *scope_depth > 0 {
                return Err(anyhow!(
                    "correlated column reference (depth={}) for '{}' must be resolved at executor level",
                    scope_depth, column_name
                ));
            }
            row.values.get(*column_index).cloned().ok_or_else(|| {
                anyhow!(
                    "column index {} out of bounds (row has {} columns) for '{}'",
                    column_index,
                    row.values.len(),
                    column_name,
                )
            })
        }

        // ── Operators ───────────────────────────────────────
        TypedExprKind::BinaryOp { left, op, right } => eval_binary(op, left, right, row, qctx),

        TypedExprKind::UnaryOp { op, operand } => {
            let val = eval_typed_expr(operand, row, qctx)?;
            eval_unary(op, val)
        }

        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => {
            let val = eval_typed_expr(inner, row, qctx)?;
            cast::cast(val, target_type, *cast_context)
        }

        // ── Comparison & Logic ──────────────────────────────
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => {
            let val = eval_typed_expr(inner, row, qctx)?;
            let result = match test {
                IsTestKind::Null => val == Value::Null,
                IsTestKind::True => val == Value::Boolean(true),
                IsTestKind::False => val == Value::Boolean(false),
                // IS UNKNOWN is equivalent to IS NULL for boolean context (SQL standard)
                IsTestKind::Unknown => val == Value::Null,
            };
            Ok(Value::Boolean(if *negated { !result } else { result }))
        }

        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => {
            let val = eval_typed_expr(inner, row, qctx)?;
            let low_val = eval_typed_expr(low, row, qctx)?;
            let high_val = eval_typed_expr(high, row, qctx)?;

            // SQL three-valued logic: any NULL → NULL
            if val == Value::Null || low_val == Value::Null || high_val == Value::Null {
                return Ok(Value::Null);
            }

            let ge_low = compare_values(&val, &low_val)? >= 0;
            let le_high = compare_values(&val, &high_val)? <= 0;
            let result = ge_low && le_high;
            Ok(Value::Boolean(if *negated { !result } else { result }))
        }

        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => {
            let val = eval_typed_expr(inner, row, qctx)?;
            if val == Value::Null {
                return Ok(Value::Null);
            }
            let mut found = false;
            let mut has_null = false;
            for item_expr in list {
                let item_val = eval_typed_expr(item_expr, row, qctx)?;
                if item_val == Value::Null {
                    has_null = true;
                    continue;
                }
                if compare_values(&val, &item_val)? == 0 {
                    found = true;
                    break;
                }
            }
            if found {
                Ok(Value::Boolean(!*negated))
            } else if has_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(*negated))
            }
        }

        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => {
            let val = eval_typed_expr(inner, row, qctx)?;
            let pat_val = eval_typed_expr(pattern, row, qctx)?;

            if val == Value::Null || pat_val == Value::Null {
                return Ok(Value::Null);
            }

            let s = value_to_text(&val);
            let p = value_to_text(&pat_val);
            let esc = match escape {
                Some(esc_expr) => {
                    let esc_val = eval_typed_expr(esc_expr, row, qctx)?;
                    value_to_text(&esc_val).chars().next()
                }
                None => None,
            };
            let matched = crate::sql::expr::like_match(&s, &p, esc, *case_insensitive);
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }

        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => {
            let val = eval_typed_expr(inner, row, qctx)?;
            let pat_val = eval_typed_expr(pattern, row, qctx)?;

            if val == Value::Null || pat_val == Value::Null {
                return Ok(Value::Null);
            }

            let s = value_to_text(&val);
            let p = value_to_text(&pat_val);
            let esc = match escape {
                Some(esc_expr) => {
                    let esc_val = eval_typed_expr(esc_expr, row, qctx)?;
                    value_to_text(&esc_val).chars().next()
                }
                None => None,
            };
            let matched = crate::sql::expr::similar_to_match(&s, &p, esc)?;
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }

        // ── Conditional ─────────────────────────────────────
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            if let Some(operand_expr) = operand {
                // Simple CASE: CASE operand WHEN val THEN result ...
                let operand_val = eval_typed_expr(operand_expr, row, qctx)?;
                for (when_expr, then_expr) in when_clauses {
                    let when_val = eval_typed_expr(when_expr, row, qctx)?;
                    if operand_val != Value::Null
                        && when_val != Value::Null
                        && compare_values(&operand_val, &when_val)? == 0
                    {
                        return eval_typed_expr(then_expr, row, qctx);
                    }
                }
            } else {
                // Searched CASE: CASE WHEN condition THEN result ...
                for (when_expr, then_expr) in when_clauses {
                    let when_val = eval_typed_expr(when_expr, row, qctx)?;
                    if when_val == Value::Boolean(true) {
                        return eval_typed_expr(then_expr, row, qctx);
                    }
                }
            }
            match else_result {
                Some(else_expr) => eval_typed_expr(else_expr, row, qctx),
                None => Ok(Value::Null),
            }
        }

        TypedExprKind::Coalesce(exprs) => {
            for e in exprs {
                let val = eval_typed_expr(e, row, qctx)?;
                if val != Value::Null {
                    return Ok(val);
                }
            }
            Ok(Value::Null)
        }

        TypedExprKind::NullIf(a, b) => {
            let a_val = eval_typed_expr(a, row, qctx)?;
            let b_val = eval_typed_expr(b, row, qctx)?;
            if a_val == Value::Null || b_val == Value::Null {
                Ok(a_val)
            } else if compare_values(&a_val, &b_val)? == 0 {
                Ok(Value::Null)
            } else {
                Ok(a_val)
            }
        }

        TypedExprKind::MinMax { args, is_greatest } => {
            let mut best: Option<Value> = None;
            for arg_expr in args {
                let val = eval_typed_expr(arg_expr, row, qctx)?;
                if val == Value::Null {
                    continue;
                }
                best = Some(match best {
                    None => val,
                    Some(cur) => {
                        let cmp = compare_values(&cur, &val)?;
                        if *is_greatest {
                            if cmp < 0 {
                                val
                            } else {
                                cur
                            }
                        } else if cmp > 0 {
                            val
                        } else {
                            cur
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }

        // ── Functions ───────────────────────────────────────
        TypedExprKind::FunctionCall { func, args, .. } => {
            // TIMEZONE needs the input TypedExpr data_type to decide direction.
            if func.name.eq_ignore_ascii_case("TIMEZONE") && args.len() == 2 {
                return eval_timezone(&args[0], &args[1], row, qctx);
            }

            let arg_vals: Vec<Value> = args
                .iter()
                .map(|a| eval_typed_expr(a, row, qctx))
                .collect::<Result<Vec<_>>>()?;

            eval_function_call(&func.name, arg_vals, qctx)
        }

        // Aggregates and windows are evaluated by operators, not per-row.
        TypedExprKind::AggregateCall { func, .. } => Err(anyhow!(
            "aggregate function '{}' cannot be evaluated per-row; \
             must be handled by aggregate operator",
            func.name
        )),
        TypedExprKind::WindowCall { func, .. } => Err(anyhow!(
            "window function '{}' cannot be evaluated per-row; \
             must be handled by window operator",
            func.name
        )),

        // Subqueries are resolved at executor level before row-level evaluation.
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::ArraySubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::InSubquery { .. }
        | TypedExprKind::AnyAll { .. } => Err(anyhow!(
            "subquery expressions must be resolved at executor level"
        )),

        // ── Array & JSON ────────────────────────────────────
        TypedExprKind::ArrayLiteral(items) => {
            let vals: Vec<Value> = items
                .iter()
                .map(|e| eval_typed_expr(e, row, qctx))
                .collect::<Result<Vec<_>>>()?;
            Ok(Value::Array(vals))
        }

        TypedExprKind::ArrayIndex { array, index } => {
            let arr_val = eval_typed_expr(array, row, qctx)?;
            let idx_val = eval_typed_expr(index, row, qctx)?;
            eval_array_index(arr_val, idx_val)
        }

        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => {
            let left = eval_typed_expr(inner, row, qctx)?;
            let right = eval_typed_expr(path, row, qctx)?;
            let json_op = to_sqlparser_json_op(operator);
            crate::sql::expr::eval_json_access(left, &json_op, right)
        }

        // ── Composite ───────────────────────────────────────
        TypedExprKind::Row(items) => {
            let vals: Vec<Value> = items
                .iter()
                .map(|e| eval_typed_expr(e, row, qctx))
                .collect::<Result<Vec<_>>>()?;
            Ok(Value::Array(vals))
        }
    }
}

// ── Binary operator evaluation ─────────────────────────────────

/// Evaluate a binary operation with short-circuit for AND/OR.
fn eval_binary(
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
fn to_sqlparser_binary_op(op: &BinaryOp) -> Option<BinaryOperator> {
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

// ── Unary operator evaluation ──────────────────────────────────

fn eval_unary(op: &UnaryOp, val: Value) -> Result<Value> {
    if val == Value::Null {
        return Ok(Value::Null);
    }

    match op {
        UnaryOp::Not => match val {
            Value::Boolean(b) => Ok(Value::Boolean(!b)),
            _ => Err(anyhow!("NOT requires boolean operand")),
        },
        UnaryOp::Minus => match val {
            Value::Int32(i) => i
                .checked_neg()
                .map(Value::Int32)
                .ok_or_else(|| anyhow!("integer out of range")),
            Value::Int64(i) => i
                .checked_neg()
                .map(Value::Int64)
                .ok_or_else(|| anyhow!("bigint out of range")),
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

// ── Helper functions ───────────────────────────────────────────

/// Evaluate a bitwise binary operation (AND, OR, XOR).
///
/// Preserves the input type: if both operands are Int32, returns Int32;
/// otherwise promotes to Int64 (matching PostgreSQL behavior).
fn eval_bitwise_op(
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
/// Clamps the shift amount to 0..63 to prevent panics (matching PostgreSQL
/// behavior where excessive shifts produce 0). Preserves input type.
fn eval_shift_op(left: Value, right: Value, op_name: &str, is_left: bool) -> Result<Value> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let is_i32 = matches!(&left, Value::Int32(_));
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

    // Clamp shift amount: negative or >= 64 → 0 (PostgreSQL behavior)
    let result = if r < 0 || r >= 64 {
        0i64
    } else if is_left {
        l.wrapping_shl(r as u32)
    } else {
        l.wrapping_shr(r as u32)
    };

    if is_i32 {
        Ok(Value::Int32(result as i32))
    } else {
        Ok(Value::Int64(result))
    }
}

/// Extract i64 from a Value (for bitwise operations).
fn value_to_i64(val: &Value) -> Option<i64> {
    match val {
        Value::Int32(i) => Some(*i as i64),
        Value::Int64(i) => Some(*i),
        _ => None,
    }
}

/// Extract text representation from a Value (for LIKE/SIMILAR TO).
fn value_to_text(val: &Value) -> String {
    match val {
        Value::Text(s) => s.clone(),
        v => v.to_string(),
    }
}

/// Evaluate array indexing (1-based, PostgreSQL convention).
fn eval_array_index(arr_val: Value, idx_val: Value) -> Result<Value> {
    if arr_val == Value::Null || idx_val == Value::Null {
        return Ok(Value::Null);
    }
    let arr = match arr_val {
        Value::Array(a) => a,
        _ => return Err(anyhow!("subscript requires array operand")),
    };
    let idx = match idx_val {
        Value::Int32(i) => i as i64,
        Value::Int64(i) => i,
        _ => return Err(anyhow!("array index must be integer")),
    };
    // PostgreSQL uses 1-based indexing
    let zero_based = idx - 1;
    if zero_based < 0 || zero_based >= arr.len() as i64 {
        Ok(Value::Null)
    } else {
        Ok(arr[zero_based as usize].clone())
    }
}

/// Dispatch a function call: check context-dependent builtins first, then
/// fall back to the global function registry.
///
/// Context-dependent functions (NOW, CURRENT_DATE, PG_BACKEND_PID, etc.)
/// read timestamps and session info from the explicit `QueryContext`.
fn eval_function_call(name: &str, args: Vec<Value>, qctx: &QueryContext) -> Result<Value> {
    let func_name_upper = name.to_uppercase();

    // Context-dependent builtins that need QueryContext.
    match func_name_upper.as_str() {
        "NOW" | "CURRENT_TIMESTAMP" | "STATEMENT_TIMESTAMP" | "TRANSACTION_TIMESTAMP" => {
            let precision = match args.first() {
                None => 6_u32,
                Some(Value::Int32(p)) => (*p).clamp(0, 6) as u32,
                Some(Value::Int64(p)) => (*p).clamp(0, 6) as u32,
                _ => 6_u32,
            };
            let ts = if func_name_upper == "STATEMENT_TIMESTAMP" {
                qctx.statement_timestamp_ms
            } else {
                qctx.transaction_timestamp_ms
            };
            let ts = crate::types::timestamp::truncate_timestamp_millis(ts, precision);
            return Ok(Value::Timestamp(ts));
        }
        "CURRENT_DATE" => {
            let days =
                crate::types::date::timestamp_millis_to_date_days(qctx.transaction_timestamp_ms)?;
            return Ok(Value::Date(days));
        }
        "PG_BACKEND_PID" => {
            return Ok(Value::Int32(qctx.connection_id));
        }
        "CURRENT_DATABASE" => {
            return Ok(Value::Text(qctx.database_name.as_ref().to_string()));
        }
        "CURRENT_SCHEMA" => return Ok(Value::Text("public".to_string())),
        "CURRENT_USER" | "SESSION_USER" | "USER" => {
            return Ok(Value::Text("postgres".to_string()));
        }
        "VERSION" => {
            return Ok(Value::Text(crate::sql::expr::VERSION_STRING.to_string()));
        }
        "SET_CONFIG" | "PG_CATALOG.SET_CONFIG" => return Ok(Value::Text(String::new())),
        "PG_GET_USERBYID" => return Ok(Value::Text("postgres".to_string())),
        "NEXTVAL" | "CURRVAL" | "SETVAL" => {
            return Err(anyhow!(
                "{} is a sequence function and must be evaluated during execution",
                name
            ));
        }
        "GENERATE_SERIES" => {
            return Err(anyhow!(
                "GENERATE_SERIES is a set-returning function, not supported in this context"
            ));
        }
        _ => {}
    }

    // Standard registry lookup.
    let registry = crate::sql::expr::functions::get_registry();
    match registry.get(func_name_upper.as_str()) {
        Some(f) => f(args),
        None => Err(SqlError::Unsupported(format!("unknown function: {}", name)).into()),
    }
}

/// Evaluate TIMEZONE(zone, timestamp) with type-aware direction.
///
/// PostgreSQL semantics:
/// - TIMESTAMP AT TIME ZONE zone  → TIMESTAMPTZ: interpret as local, convert to UTC
/// - TIMESTAMPTZ AT TIME ZONE zone → TIMESTAMP: convert from UTC to local
fn eval_timezone(
    tz_expr: &TypedExpr,
    ts_expr: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    let tz_val = eval_typed_expr(tz_expr, row, qctx)?;
    let ts_val = eval_typed_expr(ts_expr, row, qctx)?;

    if matches!(tz_val, Value::Null) || matches!(ts_val, Value::Null) {
        return Ok(Value::Null);
    }

    let tz_str = match &tz_val {
        Value::Text(s) => s.as_str(),
        _ => return Err(anyhow!("AT TIME ZONE requires text timezone argument")),
    };
    let offset_secs = crate::sql::timezone::parse_timezone_offset_seconds(tz_str)?;
    let offset_ms = i64::from(offset_secs) * 1000;

    let ts_millis = match ts_val {
        Value::Timestamp(ms) => ms,
        Value::Date(days) => crate::types::date::date_days_to_timestamp_millis(days)?,
        Value::Text(ref s) => match crate::sql::expr::parse_timestamp_string(s) {
            Ok(Value::Timestamp(ms)) => ms,
            _ => return Err(anyhow!("AT TIME ZONE requires timestamp, got text: {}", s)),
        },
        Value::Int64(ms) => ms,
        _ => return Err(anyhow!("AT TIME ZONE requires timestamp, got {:?}", ts_val)),
    };

    use crate::types::DataType;
    if matches!(ts_expr.data_type, DataType::TimestampTz) {
        // TIMESTAMPTZ → TIMESTAMP: convert from UTC to local
        Ok(Value::Timestamp(ts_millis + offset_ms))
    } else {
        // TIMESTAMP → TIMESTAMPTZ: interpret as local, convert to UTC
        Ok(Value::Timestamp(ts_millis - offset_ms))
    }
}

/// Map analyzer JsonAccessOp → sqlparser JsonOperator.
fn to_sqlparser_json_op(op: &JsonAccessOp) -> sqlparser::ast::JsonOperator {
    match op {
        JsonAccessOp::Arrow => sqlparser::ast::JsonOperator::Arrow,
        JsonAccessOp::LongArrow => sqlparser::ast::JsonOperator::LongArrow,
        JsonAccessOp::HashArrow => sqlparser::ast::JsonOperator::HashArrow,
        JsonAccessOp::HashLongArrow => sqlparser::ast::JsonOperator::HashLongArrow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::CastContext;
    use crate::types::DataType;
    use std::sync::Arc;

    fn make_row(vals: Vec<Value>) -> Row {
        Row { values: vals }
    }

    fn empty_row() -> Row {
        Row { values: vec![] }
    }

    fn test_qctx() -> QueryContext {
        QueryContext::new(
            1,                   // connection_id
            Arc::from("testdb"), // database_name
            1_700_000_000_000,   // statement_timestamp_ms
            1_700_000_000_000,   // transaction_timestamp_ms
            Arc::from("UTC"),    // timezone
        )
    }

    fn const_expr(val: Value, dt: DataType) -> TypedExpr {
        TypedExpr::new(TypedExprKind::Constant(val), dt)
    }

    fn col_ref(index: usize, name: &str, dt: DataType) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            dt,
        )
    }

    // ── Constant ────────────────────────────────────────────

    #[test]
    fn test_constant_values() {
        let row = empty_row();
        let qctx = test_qctx();

        let expr = const_expr(Value::Int64(42), DataType::Int64);
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Int64(42)
        );

        let expr = const_expr(Value::Text("hello".into()), DataType::Text);
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Text("hello".into())
        );

        let expr = const_expr(Value::Null, DataType::Text);
        assert_eq!(eval_typed_expr(&expr, &row, &qctx).unwrap(), Value::Null);

        let expr = const_expr(Value::Boolean(true), DataType::Boolean);
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    // ── ColumnRef ───────────────────────────────────────────

    #[test]
    fn test_column_ref() {
        let row = make_row(vec![
            Value::Int64(1),
            Value::Text("foo".into()),
            Value::Boolean(true),
        ]);
        let qctx = test_qctx();

        assert_eq!(
            eval_typed_expr(&col_ref(0, "a", DataType::Int64), &row, &qctx).unwrap(),
            Value::Int64(1)
        );
        assert_eq!(
            eval_typed_expr(&col_ref(1, "b", DataType::Text), &row, &qctx).unwrap(),
            Value::Text("foo".into())
        );
        assert_eq!(
            eval_typed_expr(&col_ref(2, "c", DataType::Boolean), &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_column_ref_out_of_bounds() {
        let row = make_row(vec![Value::Int64(1)]);
        let qctx = test_qctx();
        let expr = col_ref(5, "bad", DataType::Int64);
        assert!(eval_typed_expr(&expr, &row, &qctx).is_err());
    }

    #[test]
    fn test_correlated_column_ref_errors() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 1,
                column_index: 0,
                column_name: "outer_col".into(),
            },
            DataType::Int64,
        );
        assert!(eval_typed_expr(&expr, &row, &qctx).is_err());
    }

    // ── BinaryOp ────────────────────────────────────────────

    #[test]
    fn test_binary_arithmetic() {
        let row = empty_row();
        let qctx = test_qctx();

        let add = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
                op: BinaryOp::Add,
                right: Box::new(const_expr(Value::Int64(20), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&add, &row, &qctx).unwrap(),
            Value::Int64(30)
        );

        let sub = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(30), DataType::Int64)),
                op: BinaryOp::Sub,
                right: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&sub, &row, &qctx).unwrap(),
            Value::Int64(20)
        );
    }

    #[test]
    fn test_binary_comparison() {
        let row = empty_row();
        let qctx = test_qctx();

        let eq = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&eq, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );

        let lt = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(3), DataType::Int64)),
                op: BinaryOp::Lt,
                right: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&lt, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_binary_null_comparison() {
        let row = empty_row();
        let qctx = test_qctx();

        let eq_null = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(const_expr(Value::Null, DataType::Int64)),
            },
            DataType::Boolean,
        );
        assert_eq!(eval_typed_expr(&eq_null, &row, &qctx).unwrap(), Value::Null);
    }

    #[test]
    fn test_and_short_circuit() {
        let row = empty_row();
        let qctx = test_qctx();

        // false AND (error) → false (short-circuit)
        let and_false = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Boolean(false), DataType::Boolean)),
                op: BinaryOp::And,
                // This would error if evaluated (column ref on empty row)
                right: Box::new(col_ref(99, "x", DataType::Boolean)),
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&and_false, &row, &qctx).unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn test_or_short_circuit() {
        let row = empty_row();
        let qctx = test_qctx();

        // true OR (error) → true (short-circuit)
        let or_true = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Boolean(true), DataType::Boolean)),
                op: BinaryOp::Or,
                right: Box::new(col_ref(99, "x", DataType::Boolean)),
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&or_true, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_string_concat() {
        let row = empty_row();
        let qctx = test_qctx();
        let concat = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Text("hello".into()), DataType::Text)),
                op: BinaryOp::Concat,
                right: Box::new(const_expr(Value::Text(" world".into()), DataType::Text)),
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&concat, &row, &qctx).unwrap(),
            Value::Text("hello world".into())
        );
    }

    // ── UnaryOp ─────────────────────────────────────────────

    #[test]
    fn test_unary_not() {
        let row = empty_row();
        let qctx = test_qctx();
        let not = TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(const_expr(Value::Boolean(true), DataType::Boolean)),
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&not, &row, &qctx).unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn test_unary_minus() {
        let row = empty_row();
        let qctx = test_qctx();
        let neg = TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::Minus,
                operand: Box::new(const_expr(Value::Int64(42), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&neg, &row, &qctx).unwrap(),
            Value::Int64(-42)
        );
    }

    #[test]
    fn test_unary_null() {
        let row = empty_row();
        let qctx = test_qctx();
        let neg_null = TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::Minus,
                operand: Box::new(const_expr(Value::Null, DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&neg_null, &row, &qctx).unwrap(),
            Value::Null
        );
    }

    // ── Cast ────────────────────────────────────────────────

    #[test]
    fn test_cast() {
        use crate::sql::types::CastContext;
        let row = empty_row();
        let qctx = test_qctx();

        let cast_expr = TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(const_expr(Value::Int64(42), DataType::Int64)),
                target_type: DataType::Text,
                cast_context: CastContext::Explicit,
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&cast_expr, &row, &qctx).unwrap(),
            Value::Text("42".into())
        );
    }

    // ── IsTest ──────────────────────────────────────────────

    #[test]
    fn test_is_null() {
        let row = empty_row();
        let qctx = test_qctx();

        let is_null = TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(const_expr(Value::Null, DataType::Int64)),
                test: IsTestKind::Null,
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&is_null, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );

        let is_not_null = TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                test: IsTestKind::Null,
                negated: true,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&is_not_null, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_is_true_false() {
        let row = empty_row();
        let qctx = test_qctx();

        let is_true = TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(const_expr(Value::Boolean(true), DataType::Boolean)),
                test: IsTestKind::True,
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&is_true, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );

        let is_false = TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(const_expr(Value::Boolean(false), DataType::Boolean)),
                test: IsTestKind::False,
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&is_false, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    // ── Between ─────────────────────────────────────────────

    #[test]
    fn test_between() {
        let row = empty_row();
        let qctx = test_qctx();

        let between = TypedExpr::new(
            TypedExprKind::Between {
                expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                low: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
                high: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&between, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );

        let not_between = TypedExpr::new(
            TypedExprKind::Between {
                expr: Box::new(const_expr(Value::Int64(15), DataType::Int64)),
                low: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
                high: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&not_between, &row, &qctx).unwrap(),
            Value::Boolean(false)
        );
    }

    // ── InList ──────────────────────────────────────────────

    #[test]
    fn test_in_list() {
        let row = empty_row();
        let qctx = test_qctx();

        let in_list = TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(const_expr(Value::Int64(3), DataType::Int64)),
                list: vec![
                    const_expr(Value::Int64(1), DataType::Int64),
                    const_expr(Value::Int64(2), DataType::Int64),
                    const_expr(Value::Int64(3), DataType::Int64),
                ],
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&in_list, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );

        let not_in_list = TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                list: vec![
                    const_expr(Value::Int64(1), DataType::Int64),
                    const_expr(Value::Int64(2), DataType::Int64),
                ],
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&not_in_list, &row, &qctx).unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn test_in_list_with_null() {
        let row = empty_row();
        let qctx = test_qctx();

        // 5 IN (1, NULL) → NULL (not found, but has NULL)
        let in_null = TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                list: vec![
                    const_expr(Value::Int64(1), DataType::Int64),
                    const_expr(Value::Null, DataType::Int64),
                ],
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(eval_typed_expr(&in_null, &row, &qctx).unwrap(), Value::Null);
    }

    // ── Like ────────────────────────────────────────────────

    #[test]
    fn test_like() {
        let row = empty_row();
        let qctx = test_qctx();

        let like = TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(const_expr(
                    Value::Text("hello world".into()),
                    DataType::Text,
                )),
                pattern: Box::new(const_expr(Value::Text("hello%".into()), DataType::Text)),
                escape: None,
                case_insensitive: false,
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&like, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );

        let ilike = TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(const_expr(
                    Value::Text("Hello World".into()),
                    DataType::Text,
                )),
                pattern: Box::new(const_expr(Value::Text("hello%".into()), DataType::Text)),
                escape: None,
                case_insensitive: true,
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&ilike, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    // ── Case ────────────────────────────────────────────────

    #[test]
    fn test_searched_case() {
        let row = empty_row();
        let qctx = test_qctx();

        let case = TypedExpr::new(
            TypedExprKind::Case {
                operand: None,
                when_clauses: vec![
                    (
                        const_expr(Value::Boolean(false), DataType::Boolean),
                        const_expr(Value::Text("no".into()), DataType::Text),
                    ),
                    (
                        const_expr(Value::Boolean(true), DataType::Boolean),
                        const_expr(Value::Text("yes".into()), DataType::Text),
                    ),
                ],
                else_result: Some(Box::new(const_expr(
                    Value::Text("else".into()),
                    DataType::Text,
                ))),
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&case, &row, &qctx).unwrap(),
            Value::Text("yes".into())
        );
    }

    #[test]
    fn test_simple_case() {
        let row = empty_row();
        let qctx = test_qctx();

        let case = TypedExpr::new(
            TypedExprKind::Case {
                operand: Some(Box::new(const_expr(Value::Int64(2), DataType::Int64))),
                when_clauses: vec![
                    (
                        const_expr(Value::Int64(1), DataType::Int64),
                        const_expr(Value::Text("one".into()), DataType::Text),
                    ),
                    (
                        const_expr(Value::Int64(2), DataType::Int64),
                        const_expr(Value::Text("two".into()), DataType::Text),
                    ),
                ],
                else_result: None,
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&case, &row, &qctx).unwrap(),
            Value::Text("two".into())
        );
    }

    // ── Coalesce ────────────────────────────────────────────

    #[test]
    fn test_coalesce() {
        let row = empty_row();
        let qctx = test_qctx();

        let coalesce = TypedExpr::new(
            TypedExprKind::Coalesce(vec![
                const_expr(Value::Null, DataType::Int64),
                const_expr(Value::Null, DataType::Int64),
                const_expr(Value::Int64(42), DataType::Int64),
                const_expr(Value::Int64(99), DataType::Int64),
            ]),
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&coalesce, &row, &qctx).unwrap(),
            Value::Int64(42)
        );
    }

    // ── NullIf ──────────────────────────────────────────────

    #[test]
    fn test_nullif() {
        let row = empty_row();
        let qctx = test_qctx();

        // NULLIF(5, 5) → NULL
        let nullif_eq = TypedExpr::new(
            TypedExprKind::NullIf(
                Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            ),
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&nullif_eq, &row, &qctx).unwrap(),
            Value::Null
        );

        // NULLIF(5, 3) → 5
        let nullif_ne = TypedExpr::new(
            TypedExprKind::NullIf(
                Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                Box::new(const_expr(Value::Int64(3), DataType::Int64)),
            ),
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&nullif_ne, &row, &qctx).unwrap(),
            Value::Int64(5)
        );
    }

    // ── MinMax ──────────────────────────────────────────────

    #[test]
    fn test_greatest_least() {
        let row = empty_row();
        let qctx = test_qctx();

        let greatest = TypedExpr::new(
            TypedExprKind::MinMax {
                args: vec![
                    const_expr(Value::Int64(3), DataType::Int64),
                    const_expr(Value::Int64(7), DataType::Int64),
                    const_expr(Value::Int64(1), DataType::Int64),
                ],
                is_greatest: true,
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&greatest, &row, &qctx).unwrap(),
            Value::Int64(7)
        );

        let least = TypedExpr::new(
            TypedExprKind::MinMax {
                args: vec![
                    const_expr(Value::Int64(3), DataType::Int64),
                    const_expr(Value::Int64(7), DataType::Int64),
                    const_expr(Value::Int64(1), DataType::Int64),
                ],
                is_greatest: false,
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&least, &row, &qctx).unwrap(),
            Value::Int64(1)
        );
    }

    // ── FunctionCall ────────────────────────────────────────

    #[test]
    fn test_function_call_abs() {
        let row = empty_row();
        let qctx = test_qctx();

        let abs = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "abs".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Float64,
                },
                args: vec![const_expr(Value::Float64(-42.0), DataType::Float64)],
                order_by: vec![],
                filter: None,
            },
            DataType::Float64,
        );
        assert_eq!(
            eval_typed_expr(&abs, &row, &qctx).unwrap(),
            Value::Float64(42.0)
        );
    }

    // ── ArrayLiteral ────────────────────────────────────────

    #[test]
    fn test_array_literal() {
        let row = empty_row();
        let qctx = test_qctx();

        let arr = TypedExpr::new(
            TypedExprKind::ArrayLiteral(vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(2), DataType::Int64),
                const_expr(Value::Int64(3), DataType::Int64),
            ]),
            DataType::Array(Box::new(DataType::Int64)),
        );
        assert_eq!(
            eval_typed_expr(&arr, &row, &qctx).unwrap(),
            Value::Array(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)])
        );
    }

    // ── ArrayIndex ──────────────────────────────────────────

    #[test]
    fn test_array_index() {
        let row = empty_row();
        let qctx = test_qctx();

        // arr[2] (1-based) → second element
        let idx = TypedExpr::new(
            TypedExprKind::ArrayIndex {
                array: Box::new(TypedExpr::new(
                    TypedExprKind::ArrayLiteral(vec![
                        const_expr(Value::Int64(10), DataType::Int64),
                        const_expr(Value::Int64(20), DataType::Int64),
                        const_expr(Value::Int64(30), DataType::Int64),
                    ]),
                    DataType::Array(Box::new(DataType::Int64)),
                )),
                index: Box::new(const_expr(Value::Int64(2), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&idx, &row, &qctx).unwrap(),
            Value::Int64(20)
        );

        // Out of bounds → NULL
        let oob = TypedExpr::new(
            TypedExprKind::ArrayIndex {
                array: Box::new(TypedExpr::new(
                    TypedExprKind::ArrayLiteral(vec![const_expr(
                        Value::Int64(10),
                        DataType::Int64,
                    )]),
                    DataType::Array(Box::new(DataType::Int64)),
                )),
                index: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(eval_typed_expr(&oob, &row, &qctx).unwrap(), Value::Null);
    }

    // ── Row ─────────────────────────────────────────────────

    #[test]
    fn test_row_constructor() {
        let row = empty_row();
        let qctx = test_qctx();

        let row_expr = TypedExpr::new(
            TypedExprKind::Row(vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Text("a".into()), DataType::Text),
            ]),
            DataType::Boolean, // DataType doesn't matter for Row
        );
        assert_eq!(
            eval_typed_expr(&row_expr, &row, &qctx).unwrap(),
            Value::Array(vec![Value::Int64(1), Value::Text("a".into())])
        );
    }

    // ── Aggregate / Window / Subquery errors ────────────────

    #[test]
    fn test_aggregate_errors() {
        let row = empty_row();
        let qctx = test_qctx();
        let agg = TypedExpr::new(
            TypedExprKind::AggregateCall {
                func: ResolvedFunction {
                    name: "count".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                distinct: false,
                order_by: vec![],
                filter: None,
            },
            DataType::Int64,
        );
        assert!(eval_typed_expr(&agg, &row, &qctx).is_err());
    }

    #[test]
    fn test_subquery_errors() {
        let row = empty_row();
        let qctx = test_qctx();
        let exists = TypedExpr::new(
            TypedExprKind::Exists {
                subquery: Box::new(AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![],
                }),
                negated: false,
            },
            DataType::Boolean,
        );
        assert!(eval_typed_expr(&exists, &row, &qctx).is_err());
    }

    // ── Column-based evaluation ─────────────────────────────

    #[test]
    fn test_column_based_filter() {
        // Simulate: SELECT * FROM t WHERE a > 10
        let row = make_row(vec![Value::Int64(15), Value::Text("hello".into())]);
        let qctx = test_qctx();

        let filter = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "a", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&filter, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_complex_expression() {
        // Simulate: CASE WHEN a > 10 THEN 'big' WHEN a > 5 THEN 'medium' ELSE 'small' END
        let row = make_row(vec![Value::Int64(7)]);
        let qctx = test_qctx();

        let case = TypedExpr::new(
            TypedExprKind::Case {
                operand: None,
                when_clauses: vec![
                    (
                        TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(col_ref(0, "a", DataType::Int64)),
                                op: BinaryOp::Gt,
                                right: Box::new(const_expr(Value::Int64(10), DataType::Int64)),
                            },
                            DataType::Boolean,
                        ),
                        const_expr(Value::Text("big".into()), DataType::Text),
                    ),
                    (
                        TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(col_ref(0, "a", DataType::Int64)),
                                op: BinaryOp::Gt,
                                right: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                            },
                            DataType::Boolean,
                        ),
                        const_expr(Value::Text("medium".into()), DataType::Text),
                    ),
                ],
                else_result: Some(Box::new(const_expr(
                    Value::Text("small".into()),
                    DataType::Text,
                ))),
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&case, &row, &qctx).unwrap(),
            Value::Text("medium".into())
        );
    }

    #[test]
    fn test_exp_operator() {
        let row = empty_row();
        let qctx = test_qctx();

        let exp = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Float64(2.0), DataType::Float64)),
                op: BinaryOp::Exp,
                right: Box::new(const_expr(Value::Float64(3.0), DataType::Float64)),
            },
            DataType::Float64,
        );
        assert_eq!(
            eval_typed_expr(&exp, &row, &qctx).unwrap(),
            Value::Float64(8.0)
        );
    }

    #[test]
    fn test_bitwise_and() {
        let row = empty_row();
        let qctx = test_qctx();

        // Int64 & Int64 → Int64
        let bw_and = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(0b1100), DataType::Int64)),
                op: BinaryOp::BitwiseAnd,
                right: Box::new(const_expr(Value::Int64(0b1010), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&bw_and, &row, &qctx).unwrap(),
            Value::Int64(0b1000)
        );
    }

    #[test]
    fn test_bitwise_not() {
        let row = empty_row();
        let qctx = test_qctx();

        let bw_not = TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::BitwiseNot,
                operand: Box::new(const_expr(Value::Int64(0), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&bw_not, &row, &qctx).unwrap(),
            Value::Int64(-1) // !0 = -1 in two's complement
        );
    }

    // ── Shift safety ───────────────────────────────────────

    #[test]
    fn test_shift_left_basic() {
        let row = empty_row();
        let qctx = test_qctx();
        let shl = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
                op: BinaryOp::ShiftLeft,
                right: Box::new(const_expr(Value::Int64(3), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(eval_typed_expr(&shl, &row, &qctx).unwrap(), Value::Int64(8));
    }

    #[test]
    fn test_shift_right_basic() {
        let row = empty_row();
        let qctx = test_qctx();
        let shr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(16), DataType::Int64)),
                op: BinaryOp::ShiftRight,
                right: Box::new(const_expr(Value::Int64(2), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(eval_typed_expr(&shr, &row, &qctx).unwrap(), Value::Int64(4));
    }

    #[test]
    fn test_shift_excessive_amount_returns_zero() {
        let row = empty_row();
        let qctx = test_qctx();

        // Shift left by 64 → 0 (PostgreSQL behavior)
        let shl_64 = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
                op: BinaryOp::ShiftLeft,
                right: Box::new(const_expr(Value::Int64(64), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&shl_64, &row, &qctx).unwrap(),
            Value::Int64(0)
        );

        // Shift right by 100 → 0
        let shr_100 = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(42), DataType::Int64)),
                op: BinaryOp::ShiftRight,
                right: Box::new(const_expr(Value::Int64(100), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&shr_100, &row, &qctx).unwrap(),
            Value::Int64(0)
        );
    }

    #[test]
    fn test_shift_negative_amount_returns_zero() {
        let row = empty_row();
        let qctx = test_qctx();
        let shl_neg = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(42), DataType::Int64)),
                op: BinaryOp::ShiftLeft,
                right: Box::new(const_expr(Value::Int64(-1), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&shl_neg, &row, &qctx).unwrap(),
            Value::Int64(0)
        );
    }

    #[test]
    fn test_shift_null_propagation() {
        let row = empty_row();
        let qctx = test_qctx();
        let shl_null = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
                op: BinaryOp::ShiftLeft,
                right: Box::new(const_expr(Value::Null, DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&shl_null, &row, &qctx).unwrap(),
            Value::Null
        );
    }

    // ── Bitwise type preservation ──────────────────────────

    #[test]
    fn test_bitwise_preserves_int32() {
        let row = empty_row();
        let qctx = test_qctx();

        // Int32 & Int32 → Int32
        let bw = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int32(0xFF), DataType::Int32)),
                op: BinaryOp::BitwiseAnd,
                right: Box::new(const_expr(Value::Int32(0x0F), DataType::Int32)),
            },
            DataType::Int32,
        );
        assert_eq!(
            eval_typed_expr(&bw, &row, &qctx).unwrap(),
            Value::Int32(0x0F)
        );
    }

    #[test]
    fn test_shift_preserves_int32() {
        let row = empty_row();
        let qctx = test_qctx();

        // Int32 << Int32 → Int32
        let shl = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int32(1), DataType::Int32)),
                op: BinaryOp::ShiftLeft,
                right: Box::new(const_expr(Value::Int32(3), DataType::Int32)),
            },
            DataType::Int32,
        );
        assert_eq!(eval_typed_expr(&shl, &row, &qctx).unwrap(), Value::Int32(8));
    }

    #[test]
    fn test_bitwise_int32_int64_promotes_to_int64() {
        let row = empty_row();
        let qctx = test_qctx();

        // Int32 & Int64 → Int64
        let bw = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int32(0xFF), DataType::Int32)),
                op: BinaryOp::BitwiseAnd,
                right: Box::new(const_expr(Value::Int64(0x0F), DataType::Int64)),
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&bw, &row, &qctx).unwrap(),
            Value::Int64(0x0F)
        );
    }

    // ── Context-dependent functions ────────────────────────

    #[test]
    fn test_current_schema() {
        let row = empty_row();
        let qctx = test_qctx();
        let func = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "current_schema".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&func, &row, &qctx).unwrap(),
            Value::Text("public".into())
        );
    }

    #[test]
    fn test_current_user() {
        let row = empty_row();
        let qctx = test_qctx();
        let func = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "current_user".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&func, &row, &qctx).unwrap(),
            Value::Text("postgres".into())
        );
    }

    #[test]
    fn test_version() {
        let row = empty_row();
        let qctx = test_qctx();
        let func = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "version".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Text,
        );
        let result = eval_typed_expr(&func, &row, &qctx).unwrap();
        match result {
            Value::Text(s) => assert!(s.contains("pg-tikv")),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn test_sequence_function_errors() {
        let row = empty_row();
        let qctx = test_qctx();
        let func = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "nextval".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![const_expr(Value::Text("seq1".into()), DataType::Text)],
                order_by: vec![],
                filter: None,
            },
            DataType::Int64,
        );
        assert!(eval_typed_expr(&func, &row, &qctx).is_err());
    }

    // ── Context-dependent timestamp functions ────────────────

    #[test]
    fn test_now_uses_explicit_qctx() {
        let row = empty_row();
        let qctx = QueryContext::new(
            1,
            Arc::from("testdb"),
            1_700_000_000_000, // statement ts
            1_700_000_000_000, // transaction ts
            Arc::from("UTC"),
        );
        let func = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "now".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Timestamp,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Timestamp,
        );
        let result = eval_typed_expr(&func, &row, &qctx).unwrap();
        assert!(matches!(result, Value::Timestamp(_)));
    }

    #[test]
    fn test_pg_backend_pid_uses_explicit_qctx() {
        let row = empty_row();
        let qctx = QueryContext::new(
            42, // connection_id = 42
            Arc::from("testdb"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );
        let func = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "pg_backend_pid".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int32,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Int32,
        );
        assert_eq!(
            eval_typed_expr(&func, &row, &qctx).unwrap(),
            Value::Int32(42)
        );
    }

    #[test]
    fn test_current_database_uses_explicit_qctx() {
        let row = empty_row();
        let qctx = QueryContext::new(
            1,
            Arc::from("mydb"), // database_name = "mydb"
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );
        let func = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "current_database".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&func, &row, &qctx).unwrap(),
            Value::Text("mydb".into())
        );
    }

    // ── Interval arithmetic ───────────────────────────────

    #[test]
    fn test_interval_add_timestamp() {
        let row = empty_row();
        let qctx = test_qctx();
        // Timestamp + Interval(1 hour) → Timestamp
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Timestamp(1_000_000), DataType::Timestamp)),
                op: BinaryOp::Add,
                right: Box::new(const_expr(
                    Value::Interval(crate::types::IntervalValue::new(0, 3_600_000)),
                    DataType::Interval,
                )),
            },
            DataType::Timestamp,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Timestamp(1_000_000 + 3_600_000)
        );
    }

    #[test]
    fn test_interval_sub_timestamp() {
        let row = empty_row();
        let qctx = test_qctx();
        // Timestamp - Interval(1 hour) → Timestamp
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(
                    Value::Timestamp(10_000_000),
                    DataType::Timestamp,
                )),
                op: BinaryOp::Sub,
                right: Box::new(const_expr(
                    Value::Interval(crate::types::IntervalValue::new(0, 3_600_000)),
                    DataType::Interval,
                )),
            },
            DataType::Timestamp,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Timestamp(10_000_000 - 3_600_000)
        );
    }

    #[test]
    fn test_interval_add_intervals() {
        let row = empty_row();
        let qctx = test_qctx();
        // Interval(1h) + Interval(2h) → Interval(3h)
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(
                    Value::Interval(crate::types::IntervalValue::new(0, 3_600_000)),
                    DataType::Interval,
                )),
                op: BinaryOp::Add,
                right: Box::new(const_expr(
                    Value::Interval(crate::types::IntervalValue::new(0, 7_200_000)),
                    DataType::Interval,
                )),
            },
            DataType::Interval,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Interval(crate::types::IntervalValue::new(0, 10_800_000))
        );
    }

    #[test]
    fn test_timestamp_diff() {
        let row = empty_row();
        let qctx = test_qctx();
        // Timestamp - Timestamp → Interval
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(
                    Value::Timestamp(10_000_000),
                    DataType::Timestamp,
                )),
                op: BinaryOp::Sub,
                right: Box::new(const_expr(Value::Timestamp(3_000_000), DataType::Timestamp)),
            },
            DataType::Interval,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Interval(crate::types::IntervalValue::from_millis(7_000_000))
        );
    }

    #[test]
    fn test_date_add_interval() {
        let row = empty_row();
        let qctx = test_qctx();
        // Date + Interval → Timestamp
        // days since epoch 0 = 1970-01-01 → ts=0; interval 1 hour = 3_600_000ms
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Date(0), DataType::Date)),
                op: BinaryOp::Add,
                right: Box::new(const_expr(
                    Value::Interval(crate::types::IntervalValue::new(0, 3_600_000)),
                    DataType::Interval,
                )),
            },
            DataType::Timestamp,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        assert!(matches!(result, Value::Timestamp(_)));
    }

    // ── String functions ──────────────────────────────────

    fn func_call(name: &str, args: Vec<TypedExpr>, ret: DataType) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: name.into(),
                    kind: FunctionKind::Builtin,
                    return_type: ret.clone(),
                },
                args,
                order_by: vec![],
                filter: None,
            },
            ret,
        )
    }

    #[test]
    fn test_trim_function() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = func_call(
            "btrim",
            vec![const_expr(Value::Text("  hello  ".into()), DataType::Text)],
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Text("hello".into())
        );
    }

    #[test]
    fn test_ltrim_rtrim() {
        let row = empty_row();
        let qctx = test_qctx();
        let ltrim = func_call(
            "ltrim",
            vec![const_expr(Value::Text("  hello".into()), DataType::Text)],
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&ltrim, &row, &qctx).unwrap(),
            Value::Text("hello".into())
        );

        let rtrim = func_call(
            "rtrim",
            vec![const_expr(Value::Text("hello  ".into()), DataType::Text)],
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&rtrim, &row, &qctx).unwrap(),
            Value::Text("hello".into())
        );
    }

    #[test]
    fn test_position_function() {
        let row = empty_row();
        let qctx = test_qctx();
        // POSITION('lo' IN 'hello') → 4 (1-based)
        let expr = func_call(
            "strpos",
            vec![
                const_expr(Value::Text("hello".into()), DataType::Text),
                const_expr(Value::Text("lo".into()), DataType::Text),
            ],
            DataType::Int32,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Int32(4)
        );
    }

    #[test]
    fn test_substring_function() {
        let row = empty_row();
        let qctx = test_qctx();
        // SUBSTRING('hello' FROM 2 FOR 3) → 'ell'
        let expr = func_call(
            "substring",
            vec![
                const_expr(Value::Text("hello".into()), DataType::Text),
                const_expr(Value::Int64(2), DataType::Int64),
                const_expr(Value::Int64(3), DataType::Int64),
            ],
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Text("ell".into())
        );
    }

    #[test]
    fn test_upper_lower() {
        let row = empty_row();
        let qctx = test_qctx();
        let upper = func_call(
            "upper",
            vec![const_expr(Value::Text("hello".into()), DataType::Text)],
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&upper, &row, &qctx).unwrap(),
            Value::Text("HELLO".into())
        );

        let lower = func_call(
            "lower",
            vec![const_expr(Value::Text("HELLO".into()), DataType::Text)],
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&lower, &row, &qctx).unwrap(),
            Value::Text("hello".into())
        );
    }

    #[test]
    fn test_replace_function() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = func_call(
            "replace",
            vec![
                const_expr(Value::Text("abc".into()), DataType::Text),
                const_expr(Value::Text("b".into()), DataType::Text),
                const_expr(Value::Text("x".into()), DataType::Text),
            ],
            DataType::Text,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Text("axc".into())
        );
    }

    // ── UUID/Bytea ────────────────────────────────────────

    #[test]
    fn test_uuid_generation() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = func_call("gen_random_uuid", vec![], DataType::Uuid);
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        assert!(matches!(result, Value::Uuid(_)));
    }

    #[test]
    fn test_cast_text_to_uuid() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(const_expr(
                    Value::Text("550e8400-e29b-41d4-a716-446655440000".into()),
                    DataType::Text,
                )),
                target_type: DataType::Uuid,
                cast_context: CastContext::Explicit,
            },
            DataType::Uuid,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        assert!(matches!(result, Value::Uuid(_)));
    }

    #[test]
    fn test_bytea_length() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = func_call(
            "octet_length",
            vec![const_expr(
                Value::Bytes(vec![1, 2, 3, 4, 5]),
                DataType::Bytes,
            )],
            DataType::Int32,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Int32(5)
        );
    }

    // ── Division by zero / NaN ────────────────────────────

    #[test]
    fn test_int_division_by_zero() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(1), DataType::Int64)),
                op: BinaryOp::Div,
                right: Box::new(const_expr(Value::Int64(0), DataType::Int64)),
            },
            DataType::Int64,
        );
        let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("division by zero"),
            "expected 'division by zero' error, got: {}",
            err
        );
    }

    #[test]
    fn test_float_division_by_zero() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Float64(1.0), DataType::Float64)),
                op: BinaryOp::Div,
                right: Box::new(const_expr(Value::Float64(0.0), DataType::Float64)),
            },
            DataType::Float64,
        );
        // PostgreSQL: float8 / 0 → Infinity (not an error)
        let result = eval_typed_expr(&expr, &row, &qctx);
        match result {
            Ok(Value::Float64(f)) => assert!(f.is_infinite(), "expected Infinity, got {}", f),
            Err(e) => {
                // Also acceptable if implementation errors on division by zero
                assert!(e.to_string().to_lowercase().contains("division by zero"));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_numeric_division_by_zero() {
        use rust_decimal::Decimal;
        let row = empty_row();
        let qctx = test_qctx();
        let numeric_dt = DataType::Numeric {
            precision: None,
            scale: None,
        };
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(
                    Value::Numeric(Decimal::new(1, 0)),
                    numeric_dt.clone(),
                )),
                op: BinaryOp::Div,
                right: Box::new(const_expr(
                    Value::Numeric(Decimal::new(0, 0)),
                    numeric_dt.clone(),
                )),
            },
            numeric_dt,
        );
        let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("division by zero"),
            "expected 'division by zero' error, got: {}",
            err
        );
    }

    #[test]
    fn test_modulo_by_zero() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(const_expr(Value::Int64(5), DataType::Int64)),
                op: BinaryOp::Mod,
                right: Box::new(const_expr(Value::Int64(0), DataType::Int64)),
            },
            DataType::Int64,
        );
        let err = eval_typed_expr(&expr, &row, &qctx).unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("by zero"),
            "expected 'by zero' error, got: {}",
            err
        );
    }

    // ── AT TIME ZONE ──────────────────────────────────────

    #[test]
    fn test_at_time_zone_utc() {
        let row = empty_row();
        let qctx = test_qctx();
        // TIMESTAMP AT TIME ZONE 'UTC' → no offset change
        let expr = func_call(
            "timezone",
            vec![
                const_expr(Value::Text("UTC".into()), DataType::Text),
                TypedExpr::new(
                    TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                    DataType::Timestamp,
                ),
            ],
            DataType::Timestamp,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Timestamp(1_700_000_000_000)
        );
    }

    #[test]
    fn test_at_time_zone_named() {
        let row = empty_row();
        let qctx = test_qctx();
        // TIMESTAMPTZ AT TIME ZONE 'America/New_York' → converts from UTC to EST/EDT
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "timezone".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Timestamp,
                },
                args: vec![
                    const_expr(Value::Text("America/New_York".into()), DataType::Text),
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                        DataType::TimestampTz,
                    ),
                ],
                order_by: vec![],
                filter: None,
            },
            DataType::Timestamp,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        // Should shift by EST offset (-5h = -18_000_000ms)
        assert!(matches!(result, Value::Timestamp(_)));
        if let Value::Timestamp(ms) = result {
            assert_ne!(ms, 1_700_000_000_000, "should have applied timezone offset");
        }
    }

    #[test]
    fn test_at_time_zone_offset() {
        let row = empty_row();
        let qctx = test_qctx();
        // TIMESTAMP AT TIME ZONE '+08:00' → interpret as +8h local, convert to UTC
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "timezone".into(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Timestamp,
                },
                args: vec![
                    const_expr(Value::Text("+08:00".into()), DataType::Text),
                    TypedExpr::new(
                        TypedExprKind::Constant(Value::Timestamp(1_700_000_000_000)),
                        DataType::Timestamp,
                    ),
                ],
                order_by: vec![],
                filter: None,
            },
            DataType::Timestamp,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        // TIMESTAMP AT TIME ZONE '+08:00' subtracts 8h offset
        assert_eq!(result, Value::Timestamp(1_700_000_000_000 - 8 * 3_600_000));
    }

    // ── Cast edge cases ───────────────────────────────────

    #[test]
    fn test_cast_float_nan() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(const_expr(Value::Text("NaN".into()), DataType::Text)),
                target_type: DataType::Float64,
                cast_context: CastContext::Explicit,
            },
            DataType::Float64,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        match result {
            Value::Float64(f) => assert!(f.is_nan(), "expected NaN, got {}", f),
            other => panic!("expected Float64, got {:?}", other),
        }
    }

    #[test]
    fn test_cast_float_infinity() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(const_expr(Value::Text("Infinity".into()), DataType::Text)),
                target_type: DataType::Float64,
                cast_context: CastContext::Explicit,
            },
            DataType::Float64,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        match result {
            Value::Float64(f) => {
                assert!(f.is_infinite() && f > 0.0, "expected Infinity, got {}", f)
            }
            other => panic!("expected Float64, got {:?}", other),
        }
    }

    #[test]
    fn test_cast_interval_from_string() {
        let row = empty_row();
        let qctx = test_qctx();
        let expr = TypedExpr::new(
            TypedExprKind::Cast {
                expr: Box::new(const_expr(
                    Value::Text("1 day 2 hours".into()),
                    DataType::Text,
                )),
                target_type: DataType::Interval,
                cast_context: CastContext::Explicit,
            },
            DataType::Interval,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        match result {
            Value::Interval(iv) => {
                // 1 day = 86_400_000ms, 2 hours = 7_200_000ms
                let expected_ms = 86_400_000 + 7_200_000;
                assert_eq!(iv.millis, expected_ms);
            }
            other => panic!("expected Interval, got {:?}", other),
        }
    }

    // ── Misc evaluator paths ──────────────────────────────

    #[test]
    fn test_similar_to() {
        let row = empty_row();
        let qctx = test_qctx();
        // 'hello' SIMILAR TO 'h%o' → true
        let expr = TypedExpr::new(
            TypedExprKind::SimilarTo {
                expr: Box::new(const_expr(Value::Text("hello".into()), DataType::Text)),
                pattern: Box::new(const_expr(Value::Text("h%o".into()), DataType::Text)),
                escape: None,
                negated: false,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );

        // 'hello' NOT SIMILAR TO 'x%' → true
        let not_similar = TypedExpr::new(
            TypedExprKind::SimilarTo {
                expr: Box::new(const_expr(Value::Text("hello".into()), DataType::Text)),
                pattern: Box::new(const_expr(Value::Text("x%".into()), DataType::Text)),
                escape: None,
                negated: true,
            },
            DataType::Boolean,
        );
        assert_eq!(
            eval_typed_expr(&not_similar, &row, &qctx).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_array_literal_nested() {
        let row = empty_row();
        let qctx = test_qctx();
        // ARRAY[1, 2, 3]
        let expr = TypedExpr::new(
            TypedExprKind::ArrayLiteral(vec![
                const_expr(Value::Int64(1), DataType::Int64),
                const_expr(Value::Int64(2), DataType::Int64),
                const_expr(Value::Int64(3), DataType::Int64),
            ]),
            DataType::Array(Box::new(DataType::Int64)),
        );
        assert_eq!(
            eval_typed_expr(&expr, &row, &qctx).unwrap(),
            Value::Array(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)])
        );

        // Nested: ARRAY[ARRAY[1, 2], ARRAY[3, 4]]
        let nested = TypedExpr::new(
            TypedExprKind::ArrayLiteral(vec![
                TypedExpr::new(
                    TypedExprKind::ArrayLiteral(vec![
                        const_expr(Value::Int64(1), DataType::Int64),
                        const_expr(Value::Int64(2), DataType::Int64),
                    ]),
                    DataType::Array(Box::new(DataType::Int64)),
                ),
                TypedExpr::new(
                    TypedExprKind::ArrayLiteral(vec![
                        const_expr(Value::Int64(3), DataType::Int64),
                        const_expr(Value::Int64(4), DataType::Int64),
                    ]),
                    DataType::Array(Box::new(DataType::Int64)),
                ),
            ]),
            DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int64)))),
        );
        let result = eval_typed_expr(&nested, &row, &qctx).unwrap();
        assert_eq!(
            result,
            Value::Array(vec![
                Value::Array(vec![Value::Int64(1), Value::Int64(2)]),
                Value::Array(vec![Value::Int64(3), Value::Int64(4)]),
            ])
        );
    }

    #[test]
    fn test_json_access_arrow() {
        let row = empty_row();
        let qctx = test_qctx();
        // '{"a": 1}' -> 'a' → JSON '1'
        let expr = TypedExpr::new(
            TypedExprKind::JsonAccess {
                expr: Box::new(const_expr(
                    Value::Jsonb(r#"{"a": 1}"#.into()),
                    DataType::Jsonb,
                )),
                path: Box::new(const_expr(Value::Text("a".into()), DataType::Text)),
                operator: JsonAccessOp::Arrow,
            },
            DataType::Jsonb,
        );
        let result = eval_typed_expr(&expr, &row, &qctx).unwrap();
        // -> returns JSON value
        assert!(
            matches!(&result, Value::Jsonb(_) | Value::Json(_) | Value::Text(_)),
            "expected JSON-like result, got {:?}",
            result
        );

        // '{"a": 1}' ->> 'a' → Text '1'
        let text_expr = TypedExpr::new(
            TypedExprKind::JsonAccess {
                expr: Box::new(const_expr(
                    Value::Jsonb(r#"{"a": 1}"#.into()),
                    DataType::Jsonb,
                )),
                path: Box::new(const_expr(Value::Text("a".into()), DataType::Text)),
                operator: JsonAccessOp::LongArrow,
            },
            DataType::Text,
        );
        let result = eval_typed_expr(&text_expr, &row, &qctx).unwrap();
        assert_eq!(result, Value::Text("1".into()));
    }

    #[test]
    fn test_greatest_least_with_nulls() {
        let row = empty_row();
        let qctx = test_qctx();
        // GREATEST(NULL, 3, 1, NULL, 5) → 5
        let greatest = TypedExpr::new(
            TypedExprKind::MinMax {
                args: vec![
                    const_expr(Value::Null, DataType::Int64),
                    const_expr(Value::Int64(3), DataType::Int64),
                    const_expr(Value::Int64(1), DataType::Int64),
                    const_expr(Value::Null, DataType::Int64),
                    const_expr(Value::Int64(5), DataType::Int64),
                ],
                is_greatest: true,
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&greatest, &row, &qctx).unwrap(),
            Value::Int64(5)
        );

        // LEAST(NULL, 3, 1, NULL, 5) → 1
        let least = TypedExpr::new(
            TypedExprKind::MinMax {
                args: vec![
                    const_expr(Value::Null, DataType::Int64),
                    const_expr(Value::Int64(3), DataType::Int64),
                    const_expr(Value::Int64(1), DataType::Int64),
                    const_expr(Value::Null, DataType::Int64),
                    const_expr(Value::Int64(5), DataType::Int64),
                ],
                is_greatest: false,
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&least, &row, &qctx).unwrap(),
            Value::Int64(1)
        );

        // GREATEST(NULL, NULL) → NULL
        let all_null = TypedExpr::new(
            TypedExprKind::MinMax {
                args: vec![
                    const_expr(Value::Null, DataType::Int64),
                    const_expr(Value::Null, DataType::Int64),
                ],
                is_greatest: true,
            },
            DataType::Int64,
        );
        assert_eq!(
            eval_typed_expr(&all_null, &row, &qctx).unwrap(),
            Value::Null
        );
    }
}
