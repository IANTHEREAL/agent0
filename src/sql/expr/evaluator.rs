use super::context::EvalContext;
use super::operators::{parse_bool_pg, try_coerce_text_to_numeric};
use super::{
    cast_value, compare_values, eval_binary_op, eval_json_access, eval_value, interval_from_number,
    like_match, parse_interval_from_expr, parse_interval_string, parse_timestamp_string,
    parse_timezone_offset_seconds, similar_to_match,
};
use crate::types::{DataType, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{BinaryOperator, Expr, GroupByExpr, Query, SelectItem, SetExpr};

fn is_explicit_null(expr: &Expr) -> bool {
    use sqlparser::ast::Value as SqlValue;
    match expr {
        Expr::Nested(inner) => is_explicit_null(inner),
        Expr::Value(SqlValue::Null) => true,
        _ => false,
    }
}

fn is_string_literal_expr(expr: &Expr) -> bool {
    use sqlparser::ast::Value as SqlValue;
    match expr {
        Expr::Nested(inner) => is_string_literal_expr(inner),
        Expr::Value(
            SqlValue::SingleQuotedString(_)
            | SqlValue::DoubleQuotedString(_)
            | SqlValue::EscapedStringLiteral(_),
        ) => true,
        _ => false,
    }
}

fn infer_expr_type_for_validation<C: EvalContext>(ctx: &C, expr: &Expr) -> DataType {
    if let Some(data_type) = ctx.column_type(expr) {
        return data_type.clone();
    }

    let empty_schema = TableSchema::default();
    let schema = ctx.schema().unwrap_or(&empty_schema);
    crate::sql::types::infer_expr_type(expr, schema)
}

fn ensure_text_or_explicit_null_operand<C: EvalContext>(
    ctx: &C,
    expr: &Expr,
    err_msg: &'static str,
) -> Result<()> {
    if is_explicit_null(expr) {
        return Ok(());
    }

    match infer_expr_type_for_validation(ctx, expr) {
        DataType::Text => Ok(()),
        _ => Err(anyhow!(err_msg)),
    }
}

fn ensure_array_operand<C: EvalContext>(ctx: &C, expr: &Expr, err_msg: &'static str) -> Result<()> {
    match infer_expr_type_for_validation(ctx, expr) {
        DataType::Array(_) => Ok(()),
        _ => Err(anyhow!(err_msg)),
    }
}

fn eval_array_subquery_with_context<C: EvalContext, Q: AsRef<Query>>(
    ctx: &C,
    query: &Q,
) -> Result<Value> {
    let mut query = query.as_ref();

    // SQLAlchemy wraps scalar subqueries in parens, leading to `ARRAY((SELECT ...))`, which parses
    // as nested `SetExpr::Query` nodes. We only unwrap the nested queries when each wrapper is
    // "shape-less" (no WITH/ORDER/LIMIT/OFFSET/FETCH), then apply our existing Dify-focused
    // constraints on the innermost SELECT.
    let select = loop {
        if query.with.is_some()
            || !query.order_by.is_empty()
            || query.limit.is_some()
            || query.offset.is_some()
            || query.fetch.is_some()
        {
            return Err(anyhow!("Unsupported ARRAY(subquery) shape: {:?}", query));
        }

        match &*query.body {
            SetExpr::Query(nested) => {
                query = nested.as_ref();
                continue;
            }
            SetExpr::Select(select) => break select.as_ref(),
            _ => {
                return Err(anyhow!(
                    "Unsupported ARRAY(subquery) body: {:?}",
                    query.body
                ))
            }
        }
    };

    let group_by_is_empty = matches!(
        &select.group_by,
        GroupByExpr::Expressions(exprs) if exprs.is_empty()
    );

    // KISS: support the Dify pattern (tableless correlated select with a single projection).
    if !select.from.is_empty()
        || select.selection.is_some()
        || !group_by_is_empty
        || select.having.is_some()
        || select.distinct.is_some()
        || select.projection.len() != 1
    {
        return Err(anyhow!("Unsupported ARRAY(subquery) shape: {:?}", query));
    }

    let proj_expr = match &select.projection[0] {
        SelectItem::UnnamedExpr(expr) => expr,
        SelectItem::ExprWithAlias { expr, .. } => expr,
        other => {
            return Err(anyhow!(
                "Unsupported ARRAY(subquery) projection item: {:?}",
                other
            ))
        }
    };

    // Our JSON set-returning functions (e.g. jsonb_array_elements_text) are modeled as `Value::Array`.
    // Under `ARRAY(SELECT ...)`, preserve array-valued projections as a single element (e.g.
    // `ARRAY(SELECT ARRAY[1,2])` => `{{1,2}}`), while still treating set-returning projections as a
    // list (e.g. jsonb_array_elements_text => `{...}`).
    match eval_expr_impl(ctx, proj_expr)? {
        Value::Array(arr) => match infer_expr_type_for_validation(ctx, proj_expr) {
            DataType::Array(_) => Ok(Value::Array(vec![Value::Array(arr)])),
            _ => Ok(Value::Array(arr)),
        },
        other => Ok(Value::Array(vec![other])),
    }
}

fn ensure_boolean_or_null_operand<C: EvalContext>(
    ctx: &C,
    expr: &Expr,
    err_msg: &'static str,
) -> Result<()> {
    use sqlparser::ast::UnaryOperator;
    use sqlparser::ast::Value as SqlValue;

    match expr {
        Expr::Nested(inner) => ensure_boolean_or_null_operand(ctx, inner, err_msg),
        Expr::Value(SqlValue::Boolean(_)) | Expr::Value(SqlValue::Null) => Ok(()),

        Expr::Value(
            SqlValue::SingleQuotedString(s)
            | SqlValue::DoubleQuotedString(s)
            | SqlValue::EscapedStringLiteral(s),
        ) => {
            if parse_bool_pg(s).is_some() {
                Ok(())
            } else {
                Err(anyhow!(err_msg))
            }
        }

        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => match ctx.column_type(expr) {
            Some(DataType::Boolean) => Ok(()),
            Some(_) => Err(anyhow!(err_msg)),
            None => {
                let empty_schema = TableSchema::default();
                let schema = ctx.schema().unwrap_or(&empty_schema);
                match crate::sql::types::infer_expr_type(expr, schema) {
                    DataType::Boolean => Ok(()),
                    _ => Err(anyhow!(err_msg)),
                }
            }
        },

        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: inner,
        } => ensure_boolean_or_null_operand(ctx, inner, err_msg),

        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And | BinaryOperator::Or => {
                ensure_boolean_or_null_operand(ctx, left, err_msg)?;
                ensure_boolean_or_null_operand(ctx, right, err_msg)
            }

            BinaryOperator::PGOverlap => {
                ensure_array_operand(ctx, left, "&& operator requires array operands")?;
                ensure_array_operand(ctx, right, "&& operator requires array operands")?;
                Ok(())
            }

            // Operators that always return boolean (or NULL) and do not have deterministic
            // operand type errors (value-dependent errors are still short-circuitable).
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
            | BinaryOperator::PGRegexMatch
            | BinaryOperator::PGRegexIMatch
            | BinaryOperator::PGRegexNotMatch
            | BinaryOperator::PGRegexNotIMatch => Ok(()),

            BinaryOperator::Custom(op) if op == "?" => Ok(()),
            BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "?" => Ok(()),
            BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "@@" => Ok(()),

            _ => Err(anyhow!(err_msg)),
        },

        Expr::JsonAccess {
            operator, right, ..
        } => {
            use sqlparser::ast::JsonOperator;
            match operator {
                JsonOperator::AtArrow | JsonOperator::ArrowAt | JsonOperator::AtAt => Ok(()),
                // See `validate_bool_expr_in_boolean_context` for rationale: sqlparser can
                // attach comparison/IN expressions under the RHS of a JSON access.
                JsonOperator::Arrow
                | JsonOperator::LongArrow
                | JsonOperator::HashArrow
                | JsonOperator::HashLongArrow => match right.as_ref() {
                    Expr::InList { .. } => Ok(()),
                    Expr::BinaryOp { op, .. } => match op {
                        BinaryOperator::And
                        | BinaryOperator::Or
                        | BinaryOperator::Eq
                        | BinaryOperator::NotEq
                        | BinaryOperator::Gt
                        | BinaryOperator::Lt
                        | BinaryOperator::GtEq
                        | BinaryOperator::LtEq
                        | BinaryOperator::PGRegexMatch
                        | BinaryOperator::PGRegexIMatch
                        | BinaryOperator::PGRegexNotMatch
                        | BinaryOperator::PGRegexNotIMatch
                        | BinaryOperator::PGOverlap => Ok(()),
                        BinaryOperator::Custom(op) if op == "?" => Ok(()),
                        BinaryOperator::PGCustomBinaryOperator(op)
                            if op.len() == 1 && op[0] == "?" =>
                        {
                            Ok(())
                        }
                        _ => Err(anyhow!(err_msg)),
                    },
                    _ => Err(anyhow!(err_msg)),
                },
                _ => Err(anyhow!(err_msg)),
            }
        }

        Expr::Like { expr, pattern, .. } => {
            ensure_text_or_explicit_null_operand(ctx, expr, "LIKE requires text operands")?;
            ensure_text_or_explicit_null_operand(ctx, pattern, "LIKE requires text operands")?;
            Ok(())
        }

        Expr::ILike { expr, pattern, .. } => {
            ensure_text_or_explicit_null_operand(ctx, expr, "ILIKE requires text operands")?;
            ensure_text_or_explicit_null_operand(ctx, pattern, "ILIKE requires text operands")?;
            Ok(())
        }

        Expr::SimilarTo { .. }
        | Expr::Between { .. }
        | Expr::InList { .. }
        | Expr::AnyOp { .. }
        | Expr::AllOp { .. }
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsTrue(_)
        | Expr::IsNotTrue(_)
        | Expr::IsFalse(_)
        | Expr::IsNotFalse(_)
        | Expr::IsUnknown(_)
        | Expr::IsNotUnknown(_) => Ok(()),

        Expr::Cast { data_type, .. } => match data_type {
            sqlparser::ast::DataType::Boolean => Ok(()),
            _ => Err(anyhow!(err_msg)),
        },

        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            // Searched CASE (`CASE WHEN ...`) requires boolean conditions. Validate them so
            // short-circuit OR/AND cannot mask deterministic type errors inside WHEN.
            if operand.is_none() {
                for condition in conditions {
                    ensure_boolean_or_null_operand(
                        ctx,
                        condition,
                        "CASE WHEN requires boolean operands",
                    )?;
                }
            }
            for result in results {
                if is_string_literal_expr(result) {
                    return Err(anyhow!(err_msg));
                }
                ensure_boolean_or_null_operand(ctx, result, err_msg)?;
            }
            if let Some(else_expr) = else_result {
                if is_string_literal_expr(else_expr) {
                    return Err(anyhow!(err_msg));
                }
                ensure_boolean_or_null_operand(ctx, else_expr, err_msg)?;
            }
            Ok(())
        }

        // Fallback to type inference for expression forms we don't explicitly classify above.
        other => {
            let empty_schema = TableSchema::default();
            let schema = ctx.schema().unwrap_or(&empty_schema);
            match crate::sql::types::infer_expr_type(other, schema) {
                DataType::Boolean => Ok(()),
                _ => Err(anyhow!(err_msg)),
            }
        }
    }
}

pub fn eval_expr_impl<C: EvalContext>(ctx: &C, expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Value(v) => eval_value(v),
        Expr::Identifier(ident) => {
            if ident.value.to_uppercase() == "DEFAULT" {
                return Ok(Value::Null);
            }
            ctx.resolve_column(&ident.value)
        }
        Expr::CompoundIdentifier(parts) => ctx.resolve_compound_identifier(parts),
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                let left_val = eval_expr_impl(ctx, left)?;
                let left_val = super::coerce_text_literal_to_bool(left, left_val)?;
                match &left_val {
                    Value::Boolean(false) => {
                        ensure_boolean_or_null_operand(
                            ctx,
                            right,
                            "AND requires boolean operands",
                        )?;
                        Ok(Value::Boolean(false))
                    }
                    Value::Boolean(true) | Value::Null => {
                        let right_val = eval_expr_impl(ctx, right)?;
                        let right_val = super::coerce_text_literal_to_bool(right, right_val)?;
                        eval_binary_op(left_val, op, right_val)
                    }
                    _ => Err(anyhow!("AND requires boolean operands")),
                }
            }
            BinaryOperator::Or => {
                let left_val = eval_expr_impl(ctx, left)?;
                let left_val = super::coerce_text_literal_to_bool(left, left_val)?;
                match &left_val {
                    Value::Boolean(true) => {
                        ensure_boolean_or_null_operand(ctx, right, "OR requires boolean operands")?;
                        Ok(Value::Boolean(true))
                    }
                    Value::Boolean(false) | Value::Null => {
                        let right_val = eval_expr_impl(ctx, right)?;
                        let right_val = super::coerce_text_literal_to_bool(right, right_val)?;
                        eval_binary_op(left_val, op, right_val)
                    }
                    _ => Err(anyhow!("OR requires boolean operands")),
                }
            }
            _ => {
                let left_val = eval_expr_impl(ctx, left)?;
                let right_val = eval_expr_impl(ctx, right)?;
                eval_binary_op(left_val, op, right_val)
            }
        },
        Expr::UnaryOp { op, expr } => {
            let val = eval_expr_impl(ctx, expr)?;
            match op {
                sqlparser::ast::UnaryOperator::Minus => match val {
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
                    Value::Text(s) => {
                        let parsed = try_coerce_text_to_numeric(Value::Text(s.clone()));
                        match parsed {
                            Value::Int32(i) => i
                                .checked_neg()
                                .map(Value::Int32)
                                .ok_or_else(|| anyhow!("integer out of range")),
                            Value::Int64(i) => i
                                .checked_neg()
                                .map(Value::Int64)
                                .ok_or_else(|| anyhow!("bigint out of range")),
                            Value::Float64(f) => Ok(Value::Float64(-f)),
                            Value::Text(_) => {
                                Err(anyhow!("invalid input syntax for type numeric: \"{}\"", s))
                            }
                            other => Err(anyhow!("Cannot negate {:?}", other)),
                        }
                    }
                    other => Err(anyhow!("Cannot negate {:?}", other)),
                },
                sqlparser::ast::UnaryOperator::Not => {
                    let val = super::coerce_text_literal_to_bool(expr, val)?;
                    match val {
                        Value::Boolean(b) => Ok(Value::Boolean(!b)),
                        Value::Null => Ok(Value::Null),
                        other => Err(anyhow!("NOT requires boolean, got {:?}", other)),
                    }
                }
                _ => Err(anyhow!("Unsupported unary operator: {:?}", op)),
            }
        }
        Expr::Nested(expr) => eval_expr_impl(ctx, expr),
        Expr::IsNull(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            Ok(Value::Boolean(matches!(val, Value::Null)))
        }
        Expr::IsNotNull(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            Ok(Value::Boolean(!matches!(val, Value::Null)))
        }
        Expr::IsTrue(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            let val = super::coerce_text_literal_to_bool(expr, val)?;
            match val {
                Value::Boolean(b) => Ok(Value::Boolean(b)),
                Value::Null => Ok(Value::Boolean(false)),
                other => Err(anyhow!("IS TRUE requires boolean, got {:?}", other)),
            }
        }
        Expr::IsNotTrue(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            let val = super::coerce_text_literal_to_bool(expr, val)?;
            match val {
                Value::Boolean(true) => Ok(Value::Boolean(false)),
                Value::Boolean(false) | Value::Null => Ok(Value::Boolean(true)),
                other => Err(anyhow!("IS NOT TRUE requires boolean, got {:?}", other)),
            }
        }
        Expr::IsFalse(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            let val = super::coerce_text_literal_to_bool(expr, val)?;
            match val {
                Value::Boolean(b) => Ok(Value::Boolean(!b)),
                Value::Null => Ok(Value::Boolean(false)),
                other => Err(anyhow!("IS FALSE requires boolean, got {:?}", other)),
            }
        }
        Expr::IsNotFalse(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            let val = super::coerce_text_literal_to_bool(expr, val)?;
            match val {
                Value::Boolean(false) => Ok(Value::Boolean(false)),
                Value::Boolean(true) | Value::Null => Ok(Value::Boolean(true)),
                other => Err(anyhow!("IS NOT FALSE requires boolean, got {:?}", other)),
            }
        }
        Expr::IsUnknown(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            Ok(Value::Boolean(matches!(val, Value::Null)))
        }
        Expr::IsNotUnknown(expr) => {
            let val = eval_expr_impl(ctx, expr)?;
            Ok(Value::Boolean(!matches!(val, Value::Null)))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let val = eval_expr_impl(ctx, expr)?;
            if matches!(val, Value::Null) {
                return Ok(Value::Null);
            }
            let mut found = false;
            let mut has_null = false;
            for item in list {
                let item_val = eval_expr_impl(ctx, item)?;
                if matches!(item_val, Value::Null) {
                    has_null = true;
                    continue;
                }
                if compare_values(&val, &item_val).unwrap_or(1) == 0 {
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
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let val = eval_expr_impl(ctx, expr)?;
            let low_val = eval_expr_impl(ctx, low)?;
            let high_val = eval_expr_impl(ctx, high)?;
            if matches!(val, Value::Null)
                || matches!(low_val, Value::Null)
                || matches!(high_val, Value::Null)
            {
                return Ok(Value::Null);
            }
            let ge_low = compare_values(&val, &low_val).unwrap_or(-1) >= 0;
            let le_high = compare_values(&val, &high_val).unwrap_or(1) <= 0;
            let in_range = ge_low && le_high;
            Ok(Value::Boolean(if *negated { !in_range } else { in_range }))
        }
        Expr::ArraySubquery(query) => eval_array_subquery_with_context(ctx, query),
        Expr::Function(func) => eval_function_with_context(ctx, func),
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr_impl(ctx, expr)?;
            let pat = eval_expr_impl(ctx, pattern)?;
            match (&val, &pat) {
                (Value::Text(s), Value::Text(p)) => {
                    let matched = like_match(s, p, *escape_char, false);
                    Ok(Value::Boolean(if *negated { !matched } else { matched }))
                }
                (Value::Null, Value::Null)
                | (Value::Null, Value::Text(_))
                | (Value::Text(_), Value::Null) => Ok(Value::Null),
                _ => Err(anyhow!("LIKE requires text operands")),
            }
        }
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr_impl(ctx, expr)?;
            let pat = eval_expr_impl(ctx, pattern)?;
            match (&val, &pat) {
                (Value::Text(s), Value::Text(p)) => {
                    let matched = like_match(s, p, *escape_char, true);
                    Ok(Value::Boolean(if *negated { !matched } else { matched }))
                }
                (Value::Null, Value::Null)
                | (Value::Null, Value::Text(_))
                | (Value::Text(_), Value::Null) => Ok(Value::Null),
                _ => Err(anyhow!("ILIKE requires text operands")),
            }
        }
        Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let val = eval_expr_impl(ctx, expr)?;
            let pat = eval_expr_impl(ctx, pattern)?;
            let (Value::Text(s), Value::Text(p)) = (&val, &pat) else {
                if matches!(val, Value::Null) || matches!(pat, Value::Null) {
                    return Ok(Value::Null);
                }
                return Ok(Value::Boolean(false));
            };
            let matched = similar_to_match(s, p, *escape_char)?;
            Ok(Value::Boolean(if *negated { !matched } else { matched }))
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                let op_val = eval_expr_impl(ctx, op)?;
                if matches!(op_val, Value::Null) {
                    if let Some(else_expr) = else_result {
                        return eval_expr_impl(ctx, else_expr);
                    }
                    return Ok(Value::Null);
                }
                for (i, cond) in conditions.iter().enumerate() {
                    let cond_val = eval_expr_impl(ctx, cond)?;
                    if matches!(cond_val, Value::Null) {
                        continue;
                    }
                    if compare_values(&op_val, &cond_val).unwrap_or(1) == 0 {
                        return eval_expr_impl(ctx, &results[i]);
                    }
                }
            } else {
                for (i, cond) in conditions.iter().enumerate() {
                    let cond_val = eval_expr_impl(ctx, cond)?;
                    let cond_val = super::coerce_text_literal_to_bool(cond, cond_val)?;
                    let cond_true = match cond_val {
                        Value::Boolean(b) => b,
                        Value::Null => false,
                        other => {
                            return Err(anyhow!(
                                "CASE WHEN requires boolean condition, got {:?}",
                                other
                            ));
                        }
                    };

                    if cond_true {
                        return eval_expr_impl(ctx, &results[i]);
                    }
                }
            }
            if let Some(else_expr) = else_result {
                eval_expr_impl(ctx, else_expr)
            } else {
                Ok(Value::Null)
            }
        }
        Expr::Cast {
            expr, data_type, ..
        } => {
            let val = eval_expr_impl(ctx, expr)?;
            use sqlparser::ast::DataType as SqlType;
            if matches!(
                data_type,
                SqlType::Text | SqlType::Varchar(_) | SqlType::String(_)
            ) {
                if let Value::Timestamp(ts) = val {
                    let is_timestamptz = ctx.is_timestamptz(expr);
                    let mut s =
                        crate::types::timestamp::format_timestamp_millis(ts, is_timestamptz)?;
                    match data_type {
                        SqlType::Varchar(Some(
                            sqlparser::ast::CharacterLength::IntegerLength { length, .. },
                        )) => {
                            let max_len = *length as usize;
                            if s.chars().count() > max_len {
                                s = s.chars().take(max_len).collect();
                            }
                        }
                        SqlType::String(Some(n)) => {
                            let max_len = *n as usize;
                            if s.chars().count() > max_len {
                                s = s.chars().take(max_len).collect();
                            }
                        }
                        _ => {}
                    }
                    return Ok(Value::Text(s));
                }
            }
            cast_value(val, data_type)
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => eval_substring_with_context(ctx, expr, substring_from, substring_for),
        Expr::Trim {
            expr,
            trim_what,
            trim_where,
            ..
        } => {
            let val = eval_expr_impl(ctx, expr)?;
            let Value::Text(s) = val else {
                return Ok(Value::Null);
            };
            let trim_chars: Vec<char> = if let Some(what) = trim_what {
                match eval_expr_impl(ctx, what)? {
                    Value::Text(t) => t.chars().collect(),
                    _ => vec![' '],
                }
            } else {
                vec![' ']
            };
            let result = match trim_where {
                Some(sqlparser::ast::TrimWhereField::Leading) => s
                    .trim_start_matches(|c| trim_chars.contains(&c))
                    .to_string(),
                Some(sqlparser::ast::TrimWhereField::Trailing) => {
                    s.trim_end_matches(|c| trim_chars.contains(&c)).to_string()
                }
                Some(sqlparser::ast::TrimWhereField::Both) | None => {
                    s.trim_matches(|c| trim_chars.contains(&c)).to_string()
                }
            };
            Ok(Value::Text(result))
        }
        Expr::Position { expr, r#in } => {
            let substr = eval_expr_impl(ctx, expr)?;
            let string = eval_expr_impl(ctx, r#in)?;
            let (Value::Text(sub), Value::Text(s)) = (substr, string) else {
                return Ok(Value::Int32(0));
            };
            let pos = s.find(&sub).map(|i| i as i32 + 1).unwrap_or(0);
            Ok(Value::Int32(pos))
        }
        Expr::Extract { field, expr } => eval_extract_with_context(ctx, field, expr),
        Expr::Ceil { expr, .. } => {
            let val = eval_expr_impl(ctx, expr)?;
            match val {
                Value::Float64(n) => Ok(Value::Float64(n.ceil())),
                Value::Int32(n) => Ok(Value::Int32(n)),
                Value::Int64(n) => Ok(Value::Int64(n)),
                Value::Numeric(d) => {
                    use rust_decimal::prelude::ToPrimitive;
                    let f = d.to_f64().ok_or_else(|| {
                        anyhow!("numeric value out of range for double precision")
                    })?;
                    Ok(Value::Float64(f.ceil()))
                }
                _ => Ok(Value::Null),
            }
        }
        Expr::Floor { expr, .. } => {
            let val = eval_expr_impl(ctx, expr)?;
            match val {
                Value::Float64(n) => Ok(Value::Float64(n.floor())),
                Value::Int32(n) => Ok(Value::Int32(n)),
                Value::Int64(n) => Ok(Value::Int64(n)),
                Value::Numeric(d) => {
                    use rust_decimal::prelude::ToPrimitive;
                    let f = d.to_f64().ok_or_else(|| {
                        anyhow!("numeric value out of range for double precision")
                    })?;
                    Ok(Value::Float64(f.floor()))
                }
                _ => Ok(Value::Null),
            }
        }
        Expr::Interval(interval) => {
            let val = eval_expr_impl(ctx, &interval.value)?;
            match val {
                Value::Text(s) => parse_interval_from_expr(&s, interval),
                Value::Int32(n) => interval_from_number(n as i64, interval),
                Value::Int64(n) => interval_from_number(n, interval),
                _ => Err(anyhow!("Invalid interval value")),
            }
        }
        Expr::TypedString { data_type, value } => match data_type {
            sqlparser::ast::DataType::Interval => parse_interval_string(value),
            sqlparser::ast::DataType::Timestamp(_, _) => parse_timestamp_string(value),
            sqlparser::ast::DataType::Date => {
                crate::types::date::parse_date_days(value).map(Value::Date)
            }
            _ => Ok(Value::Text(value.clone())),
        },
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            let ts = eval_expr_impl(ctx, timestamp)?;
            if matches!(ts, Value::Null) {
                return Ok(Value::Null);
            }
            let offset_secs = parse_timezone_offset_seconds(time_zone)?;

            let Value::Timestamp(ts_millis) = ts else {
                return Err(anyhow!("AT TIME ZONE requires timestamp"));
            };

            let offset_ms = i64::from(offset_secs) * 1000;
            if ctx.is_timestamptz(timestamp) {
                Ok(Value::Timestamp(ts_millis + offset_ms))
            } else {
                Ok(Value::Timestamp(ts_millis - offset_ms))
            }
        }
        Expr::JsonAccess {
            left,
            operator,
            right,
        } => eval_json_access_with_context(ctx, left, operator, right),
        Expr::Array(array) => {
            let mut values = Vec::new();
            for elem in &array.elem {
                values.push(eval_expr_impl(ctx, elem)?);
            }
            Ok(Value::Array(values))
        }
        Expr::Tuple(exprs) => {
            let mut values = Vec::with_capacity(exprs.len());
            for e in exprs {
                values.push(eval_expr_impl(ctx, e)?);
            }
            Ok(Value::Array(values))
        }
        Expr::ArrayIndex { obj, indexes } => {
            let arr_val = eval_expr_impl(ctx, obj)?;
            eval_array_index_with_context(ctx, arr_val, indexes)
        }
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => eval_overlay_with_context(ctx, expr, overlay_what, overlay_from, overlay_for),
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => {
            let left_val = eval_expr_impl(ctx, left)?;
            let right_val = eval_expr_impl(ctx, right)?;
            let arr = match right_val {
                Value::Array(arr) => arr,
                _ => return Err(anyhow!("ANY requires an array operand")),
            };
            let mut saw_null = false;
            for elem in arr {
                let cmp = eval_binary_op(left_val.clone(), compare_op, elem)?;
                match cmp {
                    Value::Boolean(true) => return Ok(Value::Boolean(true)),
                    Value::Boolean(false) => {}
                    Value::Null => saw_null = true,
                    other => {
                        return Err(anyhow!("ANY comparison must yield boolean, got {other:?}"))
                    }
                };
            }
            if saw_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(false))
            }
        }
        Expr::AllOp {
            left,
            compare_op,
            right,
        } => {
            let left_val = eval_expr_impl(ctx, left)?;
            let right_val = eval_expr_impl(ctx, right)?;
            let arr = match right_val {
                Value::Array(arr) => arr,
                _ => return Err(anyhow!("ALL requires an array operand")),
            };
            if arr.is_empty() {
                return Ok(Value::Boolean(true));
            }
            let mut saw_null = false;
            for elem in arr {
                let cmp = eval_binary_op(left_val.clone(), compare_op, elem)?;
                match cmp {
                    Value::Boolean(false) => return Ok(Value::Boolean(false)),
                    Value::Boolean(true) => {}
                    Value::Null => saw_null = true,
                    other => {
                        return Err(anyhow!("ALL comparison must yield boolean, got {other:?}"))
                    }
                };
            }
            if saw_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(true))
            }
        }
        _ => Err(anyhow!("Unsupported expression: {:?}", expr)),
    }
}

fn eval_function_with_context<C: EvalContext>(
    ctx: &C,
    func: &sqlparser::ast::Function,
) -> Result<Value> {
    super::eval_function(ctx, func)
}

fn eval_substring_with_context<C: EvalContext>(
    ctx: &C,
    expr: &Expr,
    substring_from: &Option<Box<Expr>>,
    substring_for: &Option<Box<Expr>>,
) -> Result<Value> {
    let val = eval_expr_impl(ctx, expr)?;
    let from_val = if let Some(from_expr) = substring_from {
        Some(eval_expr_impl(ctx, from_expr)?)
    } else {
        None
    };

    match val {
        Value::Text(s) => {
            if let (Some(Value::Text(pattern)), None) = (&from_val, substring_for) {
                let re = regex::Regex::new(pattern)
                    .map_err(|e| anyhow!("Invalid regex pattern in SUBSTRING: {}", e))?;
                if let Some(caps) = re.captures(&s) {
                    if caps.len() > 1 {
                        return Ok(caps
                            .get(1)
                            .map(|m| Value::Text(m.as_str().to_string()))
                            .unwrap_or(Value::Null));
                    }
                    return Ok(caps
                        .get(0)
                        .map(|m| Value::Text(m.as_str().to_string()))
                        .unwrap_or(Value::Null));
                }
                return Ok(Value::Null);
            }

            let start = match &from_val {
                Some(Value::Int32(n)) => (n - 1).max(0) as usize,
                Some(Value::Int64(n)) => (n - 1).max(0) as usize,
                Some(Value::Null) => return Ok(Value::Null),
                Some(_) => 0,
                None => 0,
            };
            let len = if let Some(for_expr) = substring_for {
                match eval_expr_impl(ctx, for_expr)? {
                    Value::Int32(n) => Some(n.max(0) as usize),
                    Value::Int64(n) => Some(n.max(0) as usize),
                    Value::Null => return Ok(Value::Null),
                    _ => None,
                }
            } else {
                None
            };
            let chars: Vec<char> = s.chars().collect();
            let result: String = if let Some(l) = len {
                chars.iter().skip(start).take(l).collect()
            } else {
                chars.iter().skip(start).collect()
            };
            Ok(Value::Text(result))
        }
        Value::Bytes(bytes) => {
            let start = match &from_val {
                Some(Value::Int32(n)) => i64::from(*n),
                Some(Value::Int64(n)) => *n,
                Some(Value::Null) => return Ok(Value::Null),
                Some(_) => return Ok(Value::Null),
                None => 0,
            };
            let count = if let Some(for_expr) = substring_for {
                match eval_expr_impl(ctx, for_expr)? {
                    Value::Int32(n) => Some(i64::from(n.max(0))),
                    Value::Int64(n) => Some(n.max(0)),
                    Value::Null => return Ok(Value::Null),
                    _ => return Ok(Value::Null),
                }
            } else {
                None
            };
            Ok(Value::Bytes(super::super::bytea::substring(
                bytes, start, count,
            )))
        }
        _ => Ok(Value::Null),
    }
}

fn eval_extract_with_context<C: EvalContext>(
    ctx: &C,
    field: &sqlparser::ast::DateTimeField,
    expr: &Expr,
) -> Result<Value> {
    let val = eval_expr_impl(ctx, expr)?;
    let ts = match val {
        Value::Timestamp(t) => t,
        Value::Date(days) => {
            use chrono::NaiveDate;
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
                .ok_or_else(|| anyhow!("Failed to create epoch date"))?;
            let date = epoch + chrono::Duration::days(days as i64);
            date.and_hms_opt(0, 0, 0)
                .ok_or_else(|| anyhow!("Failed to create datetime from date"))?
                .and_utc()
                .timestamp_millis()
        }
        Value::Text(s) => {
            use chrono::NaiveDateTime;
            let dt = if let Ok(dt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S") {
                dt
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S") {
                dt
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f") {
                dt
            } else if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
                d.and_hms_opt(0, 0, 0)
                    .ok_or_else(|| anyhow!("Failed to create datetime from date"))?
            } else {
                return Err(anyhow!("Invalid timestamp format"));
            };
            dt.and_utc().timestamp_millis()
        }
        _ => return Ok(Value::Null),
    };
    use chrono::{Datelike, TimeZone, Timelike, Utc};
    let dt = Utc
        .timestamp_millis_opt(ts)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;
    let result = match field {
        sqlparser::ast::DateTimeField::Year => dt.year() as f64,
        sqlparser::ast::DateTimeField::Month => dt.month() as f64,
        sqlparser::ast::DateTimeField::Day => dt.day() as f64,
        sqlparser::ast::DateTimeField::Hour => dt.hour() as f64,
        sqlparser::ast::DateTimeField::Minute => dt.minute() as f64,
        sqlparser::ast::DateTimeField::Second => dt.second() as f64,
        sqlparser::ast::DateTimeField::Dow => dt.weekday().num_days_from_sunday() as f64,
        sqlparser::ast::DateTimeField::Doy => dt.ordinal() as f64,
        sqlparser::ast::DateTimeField::Week => dt.iso_week().week() as f64,
        sqlparser::ast::DateTimeField::Quarter => ((dt.month() - 1) / 3 + 1) as f64,
        sqlparser::ast::DateTimeField::Epoch => ts as f64 / 1000.0,
        _ => return Err(anyhow!("Unsupported EXTRACT field")),
    };
    Ok(Value::Float64(result))
}

fn eval_json_access_with_context<C: EvalContext>(
    ctx: &C,
    left: &Expr,
    operator: &sqlparser::ast::JsonOperator,
    right: &Expr,
) -> Result<Value> {
    // Handle special cases where right side contains nested expressions
    if let Expr::InList {
        expr: in_expr,
        list,
        negated,
    } = right
    {
        let json_result = eval_json_access_with_context(ctx, left, operator, in_expr)?;
        let mut found = false;
        for item in list {
            let item_val = eval_expr_impl(ctx, item)?;
            if super::compare_values(&json_result, &item_val).unwrap_or(1) == 0 {
                found = true;
                break;
            }
        }
        return Ok(Value::Boolean(if *negated { !found } else { found }));
    }

    if let Expr::BinaryOp {
        left: bin_left,
        op: bin_op,
        right: bin_right,
    } = right
    {
        let json_result = eval_json_access_with_context(ctx, left, operator, bin_left)?;
        let right_val = eval_expr_impl(ctx, bin_right)?;
        return super::eval_binary_op(json_result, bin_op, right_val);
    }

    // Collect chained JSON operations
    let mut ops: Vec<(&Expr, &sqlparser::ast::JsonOperator)> = Vec::new();
    collect_json_ops_for_context(operator, right, &mut ops);

    let mut current = eval_expr_impl(ctx, left)?;
    for (key_expr, op) in ops {
        let key = eval_expr_impl(ctx, key_expr)?;
        current = eval_json_access(current, op, key)?;
    }
    Ok(current)
}

fn collect_json_ops_for_context<'a>(
    operator: &'a sqlparser::ast::JsonOperator,
    right: &'a Expr,
    ops: &mut Vec<(&'a Expr, &'a sqlparser::ast::JsonOperator)>,
) {
    if let Expr::JsonAccess {
        left: inner_left,
        operator: inner_op,
        right: inner_right,
    } = right
    {
        ops.push((inner_left, operator));
        collect_json_ops_for_context(inner_op, inner_right, ops);
    } else {
        ops.push((right, operator));
    }
}

fn eval_array_index_with_context<C: EvalContext>(
    ctx: &C,
    arr_val: Value,
    indexes: &[Expr],
) -> Result<Value> {
    let Value::Array(arr) = arr_val else {
        return Err(anyhow!("Cannot index non-array value"));
    };

    let mut current = Value::Array(arr);
    for idx_expr in indexes {
        let idx_val = eval_expr_impl(ctx, idx_expr)?;
        let idx = match idx_val {
            Value::Int32(i) => i as i64,
            Value::Int64(i) => i,
            _ => return Err(anyhow!("Array index must be an integer")),
        };

        let Value::Array(arr) = current else {
            return Err(anyhow!("Cannot index non-array value"));
        };

        let pg_idx = (idx - 1) as usize;
        current = arr.get(pg_idx).cloned().unwrap_or(Value::Null);
    }
    Ok(current)
}

fn eval_overlay_with_context<C: EvalContext>(
    ctx: &C,
    expr: &Expr,
    overlay_what: &Expr,
    overlay_from: &Expr,
    overlay_for: &Option<Box<Expr>>,
) -> Result<Value> {
    let base = eval_expr_impl(ctx, expr)?;
    let what = eval_expr_impl(ctx, overlay_what)?;
    let from = eval_expr_impl(ctx, overlay_from)?;
    let for_len = match overlay_for {
        Some(e) => Some(eval_expr_impl(ctx, e)?),
        None => None,
    };
    let start = match from {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) => n,
        Value::Null => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let replace_len = match for_len {
        Some(Value::Int32(n)) => Some(i64::from(n.max(0))),
        Some(Value::Int64(n)) => Some(n.max(0)),
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return Ok(Value::Null),
        None => None,
    };

    match (base, what) {
        (Value::Text(base), Value::Text(what)) => {
            if start <= 0 {
                return Ok(Value::Text(base));
            }
            let replace_len = match replace_len {
                Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
                None => what.chars().count(),
            };

            let base_chars: Vec<char> = base.chars().collect();
            let start_idx = usize::try_from(start - 1).unwrap_or(usize::MAX);
            let prefix: String = base_chars.iter().take(start_idx).collect();
            let suffix_start = start_idx.saturating_add(replace_len);
            let suffix: String = base_chars.iter().skip(suffix_start).collect();
            Ok(Value::Text(format!("{prefix}{what}{suffix}")))
        }
        (Value::Bytes(base), Value::Bytes(what)) => Ok(Value::Bytes(super::super::bytea::overlay(
            base,
            &what,
            start,
            replace_len,
        ))),
        _ => Ok(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::super::eval_expr;
    use sqlparser::ast::{BinaryOperator, Expr, Value as SqlValue};

    #[test]
    fn test_and_short_circuit_errors_on_case_text_branches() {
        let make_case = || Expr::Case {
            operand: None,
            conditions: vec![Expr::Value(SqlValue::Boolean(true))],
            results: vec![Expr::Value(SqlValue::SingleQuotedString(
                "true".to_string(),
            ))],
            else_result: Some(Box::new(Expr::Value(SqlValue::SingleQuotedString(
                "false".to_string(),
            )))),
        };

        let false_and = Expr::BinaryOp {
            left: Box::new(Expr::Value(SqlValue::Boolean(false))),
            op: BinaryOperator::And,
            right: Box::new(make_case()),
        };
        let true_and = Expr::BinaryOp {
            left: Box::new(Expr::Value(SqlValue::Boolean(true))),
            op: BinaryOperator::And,
            right: Box::new(make_case()),
        };

        let false_err = eval_expr(&false_and, None, None).unwrap_err().to_string();
        assert_eq!(false_err, "AND requires boolean operands");

        let true_err = eval_expr(&true_and, None, None).unwrap_err().to_string();
        assert_eq!(true_err, "AND requires boolean operands");
    }
}
