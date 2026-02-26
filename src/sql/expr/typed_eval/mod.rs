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

mod arithmetic;
mod helpers;

#[cfg(test)]
mod tests;

use crate::model::{Row, Value};
use crate::sql::analyzer::types::*;
use crate::sql::expr::operators::compare_values;
use crate::sql::expr::typed_fold::is_fold_candidate;
use crate::sql::query_context::QueryContext;
use crate::sql::types::cast;
use anyhow::{anyhow, Result};

use arithmetic::{eval_binary, eval_unary};
use helpers::{
    eval_array_index, eval_function_call, eval_timezone, to_sqlparser_json_op, value_to_text,
};

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

/// Evaluate a constant TypedExpr as non-negative usize (LIMIT/OFFSET helper).
///
/// `null_as_zero` controls how `NULL` is handled:
/// - `true`: `NULL` is treated as `0` (legacy executor behavior).
/// - `false`: `NULL` is rejected as non-constant/non-integer.
pub(crate) fn eval_const_usize(expr: &TypedExpr, null_as_zero: bool) -> Result<usize> {
    match &expr.kind {
        TypedExprKind::Constant(Value::Int32(n)) => {
            if *n < 0 {
                Err(anyhow!("LIMIT/OFFSET must not be negative"))
            } else {
                Ok(*n as usize)
            }
        }
        TypedExprKind::Constant(Value::Int64(n)) => {
            if *n < 0 {
                Err(anyhow!("LIMIT/OFFSET must not be negative"))
            } else {
                Ok(*n as usize)
            }
        }
        TypedExprKind::Constant(Value::Null) if null_as_zero => Ok(0),
        TypedExprKind::Cast { expr: inner, .. } => eval_const_usize(inner, null_as_zero),
        TypedExprKind::MinMax { args, is_greatest } => {
            let mut best: Option<usize> = None;
            for arg in args {
                let val = eval_const_usize(arg, false)?;
                best = Some(match best {
                    None => val,
                    Some(cur) => {
                        if *is_greatest {
                            cur.max(val)
                        } else {
                            cur.min(val)
                        }
                    }
                });
            }
            match best {
                Some(v) => Ok(v),
                None if null_as_zero => Ok(0),
                None => Err(anyhow!(
                    "Expected constant integer for LIMIT/OFFSET, got: NULL"
                )),
            }
        }
        _ if null_as_zero => Err(anyhow!("LIMIT/OFFSET must be a constant integer")),
        _ => Err(anyhow!(
            "Expected constant integer for LIMIT/OFFSET, got: {:?}",
            expr.kind
        )),
    }
}

fn eval_typed_expr_inner(expr: &TypedExpr, row: &Row, qctx: &QueryContext) -> Result<Value> {
    match &expr.kind {
        // ── Leaf nodes ──────────────────────────────────────
        TypedExprKind::Constant(v) => Ok(v.clone()),
        TypedExprKind::Default => Ok(Value::Null),

        TypedExprKind::Parameter { index } => {
            match qctx.params.get(*index) {
                Some(Some(v)) => Ok(v.clone()),
                Some(None) => Ok(Value::Null), // Bound NULL
                None => Err(anyhow!(
                    "parameter ${} not bound (bind message supplies {} parameters, \
                     but prepared statement requires at least {})",
                    index + 1,
                    qctx.params.len(),
                    index + 1,
                )),
            }
        }

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

        TypedExprKind::IsDistinctFrom {
            left,
            right,
            negated,
        } => {
            let l = eval_typed_expr(left, row, qctx)?;
            let r = eval_typed_expr(right, row, qctx)?;
            // IS NOT DISTINCT FROM: NULL=NULL→true, one-NULL→false, else a=b
            let not_distinct = match (&l, &r) {
                (Value::Null, Value::Null) => true,
                (Value::Null, _) | (_, Value::Null) => false,
                _ => compare_values(&l, &r)? == 0,
            };
            // negated=true means IS NOT DISTINCT FROM (returns not_distinct)
            // negated=false means IS DISTINCT FROM (returns !not_distinct)
            Ok(Value::Boolean(if *negated {
                not_distinct
            } else {
                !not_distinct
            }))
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
            // ANY/ALL over empty array is vacuously false/true regardless of LHS.
            if list.is_empty() {
                return Ok(Value::Boolean(*negated));
            }
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

        // ScalarArrayCmp: `expr op ANY/ALL(ARRAY[...])`.
        // Evaluates expr exactly once, then applies op to each element.
        TypedExprKind::ScalarArrayCmp {
            expr: inner,
            elems,
            op,
            use_or,
        } => {
            let lhs = eval_typed_expr(inner, row, qctx)?;
            let mut has_null = false;
            if *use_or {
                // ANY semantics: TRUE if any comparison is TRUE
                for elem_expr in elems {
                    let rhs = eval_typed_expr(elem_expr, row, qctx)?;
                    if lhs == Value::Null || rhs == Value::Null {
                        has_null = true;
                        continue;
                    }
                    if scalar_array_cmp_one(&lhs, &rhs, op)? {
                        return Ok(Value::Boolean(true));
                    }
                }
                if has_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Boolean(false))
                }
            } else {
                // ALL semantics: TRUE only if all comparisons are TRUE
                for elem_expr in elems {
                    let rhs = eval_typed_expr(elem_expr, row, qctx)?;
                    if lhs == Value::Null || rhs == Value::Null {
                        has_null = true;
                        continue;
                    }
                    if !scalar_array_cmp_one(&lhs, &rhs, op)? {
                        return Ok(Value::Boolean(false));
                    }
                }
                if has_null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Boolean(true))
                }
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
            // PostgreSQL folds row-independent constant arguments in COALESCE
            // before execution. This can raise errors (e.g. `1/0`) even if
            // short-circuiting would skip the branch at runtime, unless a
            // preceding argument is a known non-NULL constant.
            let mut has_proven_non_null_constant = false;
            for e in exprs {
                if has_proven_non_null_constant {
                    break;
                }
                if !is_fold_candidate(e) {
                    continue;
                }
                let v = eval_typed_expr(e, row, qctx)?;
                if v != Value::Null {
                    has_proven_non_null_constant = true;
                }
            }

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

            // Preserve ROW(...) structural semantics for JSON conversion helpers.
            if args.len() == 1
                && (func.name.eq_ignore_ascii_case("TO_JSONB")
                    || func.name.eq_ignore_ascii_case("ROW_TO_JSON"))
                && matches!(args[0].kind, TypedExprKind::Row(_))
            {
                if let TypedExprKind::Row(items) = &args[0].kind {
                    let mut obj = serde_json::Map::new();
                    for (i, item) in items.iter().enumerate() {
                        let v = eval_typed_expr(item, row, qctx)?;
                        obj.insert(
                            format!("f{}", i + 1),
                            crate::sql::expr::functions::json::value_to_json(&v),
                        );
                    }
                    if func.name.eq_ignore_ascii_case("TO_JSONB") {
                        return Ok(Value::Jsonb(serde_json::Value::Object(obj).to_string()));
                    }
                    return Ok(Value::Json(serde_json::Value::Object(obj).to_string()));
                }
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
        | TypedExprKind::TupleInSubquery { .. }
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

        // ── Collation ───────────────────────────────────────
        TypedExprKind::Collate { expr: inner, .. } => {
            // For now, just evaluate the inner expression.
            // The collation metadata is used during comparison operations.
            eval_typed_expr(inner, row, qctx)
        }
    }
}

/// Evaluate a single comparison for ScalarArrayCmp.
fn scalar_array_cmp_one(lhs: &Value, rhs: &Value, op: &BinaryOp) -> Result<bool> {
    let cmp = compare_values(lhs, rhs)?;
    Ok(match op {
        BinaryOp::Eq => cmp == 0,
        BinaryOp::NotEq => cmp != 0,
        BinaryOp::Lt => cmp < 0,
        BinaryOp::LtEq => cmp <= 0,
        BinaryOp::Gt => cmp > 0,
        BinaryOp::GtEq => cmp >= 0,
        _ => return Err(anyhow!("unsupported operator in ScalarArrayCmp: {:?}", op)),
    })
}
