//! Expression analysis: single-pass name resolution + type checking.
//!
//! This module implements `Analyzer::analyze_expr`, the heart of the semantic
//! analysis. It walks a `sqlparser::ast::Expr` and produces a `TypedExpr` with:
//! - All names resolved to positional `ColumnRef` nodes
//! - All types checked and annotated
//! - All syntax sugar normalized to canonical forms (FunctionCall)
//! - Implicit casts inserted where needed

use rust_decimal::Decimal;
use sqlparser::ast::{
    self as ast, BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, TrimWhereField,
    UnaryOperator, WindowType,
};
use std::str::FromStr;

use crate::sql::names::function_name_upper;
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::{
    binary_op_result_type, common_type, comparison_target_type, is_numeric, unify_types,
};
use crate::sql::types::mapping::sql_datatype_to_internal;
use crate::sql::types::registry::global_registry;
use crate::types::{DataType, Value};

use super::error::AnalyzerError;
use super::types::*;
use super::Analyzer;

impl<'a> Analyzer<'a> {
    /// Analyze an expression, producing a fully-typed IR node.
    ///
    /// This is the core single-pass analysis: name resolution and type checking
    /// happen simultaneously during one recursive walk (PostgreSQL model).
    pub fn analyze_expr(&mut self, expr: &Expr) -> Result<TypedExpr, AnalyzerError> {
        match expr {
            // ── Leaf nodes ──────────────────────────────────
            Expr::Identifier(ident) => self.analyze_identifier(&ident.value),

            Expr::CompoundIdentifier(parts) => self.analyze_compound_identifier(parts),

            Expr::Value(val) => self.analyze_value(val),

            // ── Parenthesized expression ────────────────────
            Expr::Nested(inner) => self.analyze_expr(inner),

            // ── Binary operators ────────────────────────────
            Expr::BinaryOp { left, op, right } => self.analyze_binary_op(left, op, right),

            // ── Unary operators ─────────────────────────────
            Expr::UnaryOp { op, expr } => self.analyze_unary_op(op, expr),

            // ── Cast ────────────────────────────────────────
            Expr::Cast {
                expr, data_type, ..
            } => {
                let inner = self.analyze_expr(expr)?;
                let target = sql_datatype_to_internal(data_type)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                Ok(TypedExpr::new(
                    TypedExprKind::Cast {
                        expr: Box::new(inner),
                        target_type: target.clone(),
                        cast_context: CastContext::Explicit,
                    },
                    target,
                ))
            }

            // ── Typed string literal (DATE '...', TIMESTAMP '...') ──
            Expr::TypedString { data_type, value } => {
                let target = sql_datatype_to_internal(data_type)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                let parsed = self.parse_typed_literal(value, &target)?;
                Ok(TypedExpr::new(TypedExprKind::Constant(parsed), target))
            }

            // ── IS tests ────────────────────────────────────
            Expr::IsNull(expr) => self.analyze_is_test(expr, IsTestKind::Null, false),
            Expr::IsNotNull(expr) => self.analyze_is_test(expr, IsTestKind::Null, true),
            Expr::IsTrue(expr) => self.analyze_is_test(expr, IsTestKind::True, false),
            Expr::IsNotTrue(expr) => self.analyze_is_test(expr, IsTestKind::True, true),
            Expr::IsFalse(expr) => self.analyze_is_test(expr, IsTestKind::False, false),
            Expr::IsNotFalse(expr) => self.analyze_is_test(expr, IsTestKind::False, true),
            Expr::IsUnknown(expr) => self.analyze_is_test(expr, IsTestKind::Unknown, false),
            Expr::IsNotUnknown(expr) => self.analyze_is_test(expr, IsTestKind::Unknown, true),

            // ── BETWEEN ─────────────────────────────────────
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let e = self.analyze_expr(expr)?;
                let lo = self.analyze_expr(low)?;
                let hi = self.analyze_expr(high)?;
                // All three operands must be type-compatible (PostgreSQL semantics).
                let common = self.unify_expr_types(&[&e, &lo, &hi], "BETWEEN")?;
                let e = self.coerce_if_needed(e, &common);
                let lo = self.coerce_if_needed(lo, &common);
                let hi = self.coerce_if_needed(hi, &common);
                Ok(TypedExpr::new(
                    TypedExprKind::Between {
                        expr: Box::new(e),
                        low: Box::new(lo),
                        high: Box::new(hi),
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // ── IN list ─────────────────────────────────────
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let e = self.analyze_expr(expr)?;
                let analyzed_list: Vec<TypedExpr> = list
                    .iter()
                    .map(|item| self.analyze_expr(item))
                    .collect::<Result<_, _>>()?;
                // All elements must be type-compatible with the expression.
                let mut in_refs: Vec<&TypedExpr> = vec![&e];
                in_refs.extend(analyzed_list.iter());
                let common = self.unify_expr_types(&in_refs, "IN list")?;
                let e = self.coerce_if_needed(e, &common);
                let analyzed_list = analyzed_list
                    .into_iter()
                    .map(|item| self.coerce_if_needed(item, &common))
                    .collect();
                Ok(TypedExpr::new(
                    TypedExprKind::InList {
                        expr: Box::new(e),
                        list: analyzed_list,
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // ── LIKE / ILIKE ────────────────────────────────
            Expr::Like {
                negated,
                expr,
                pattern,
                escape_char,
            } => self.analyze_like(expr, pattern, escape_char, false, *negated),

            Expr::ILike {
                negated,
                expr,
                pattern,
                escape_char,
            } => self.analyze_like(expr, pattern, escape_char, true, *negated),

            // ── SIMILAR TO ──────────────────────────────────
            Expr::SimilarTo {
                negated,
                expr,
                pattern,
                escape_char,
            } => {
                let e = self.analyze_expr(expr)?;
                let p = self.analyze_expr(pattern)?;
                let esc = match escape_char {
                    Some(c) => Some(Box::new(TypedExpr::new(
                        TypedExprKind::Constant(Value::Text(c.to_string())),
                        DataType::Text,
                    ))),
                    None => None,
                };
                Ok(TypedExpr::new(
                    TypedExprKind::SimilarTo {
                        expr: Box::new(e),
                        pattern: Box::new(p),
                        escape: esc,
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // ── CASE ────────────────────────────────────────
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => self.analyze_case(operand, conditions, results, else_result),

            // ── Functions ───────────────────────────────────
            Expr::Function(func) => self.analyze_function(func),

            // ── Subqueries ──────────────────────────────────
            Expr::Subquery(query) => {
                let analyzed = self.analyze_query(query)?;
                if analyzed.output_schema.len() != 1 {
                    return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                        got: analyzed.output_schema.len(),
                    });
                }
                let dt = analyzed.output_schema[0].1.clone();
                Ok(TypedExpr::new(
                    TypedExprKind::ScalarSubquery(Box::new(analyzed)),
                    dt,
                ))
            }

            Expr::Exists { subquery, negated } => {
                let analyzed = self.analyze_query(subquery)?;
                Ok(TypedExpr::new(
                    TypedExprKind::Exists {
                        subquery: Box::new(analyzed),
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let e = self.analyze_expr(expr)?;
                let analyzed = self.analyze_query(subquery)?;
                if analyzed.output_schema.len() != 1 {
                    return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                        got: analyzed.output_schema.len(),
                    });
                }
                Ok(TypedExpr::new(
                    TypedExprKind::InSubquery {
                        expr: Box::new(e),
                        subquery: Box::new(analyzed),
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // ── Array subquery: ARRAY(SELECT ...) ─────────
            Expr::ArraySubquery(query) => {
                let analyzed = self.analyze_query(query)?;
                if analyzed.output_schema.len() != 1 {
                    return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                        got: analyzed.output_schema.len(),
                    });
                }
                let elem_type = analyzed.output_schema[0].1.clone();
                Ok(TypedExpr::new(
                    TypedExprKind::ArraySubquery(Box::new(analyzed)),
                    DataType::Array(Box::new(elem_type)),
                ))
            }

            // ── Array ───────────────────────────────────────
            Expr::Array(arr) => {
                let elems: Vec<TypedExpr> = arr
                    .elem
                    .iter()
                    .map(|e| self.analyze_expr(e))
                    .collect::<Result<_, _>>()?;

                let elem_type = if elems.is_empty() {
                    // Empty array literal: element type unknown, default to Text
                    // (PostgreSQL: `SELECT ARRAY[]::text[]` requires cast for empty)
                    DataType::Text
                } else {
                    let refs: Vec<&TypedExpr> = elems.iter().collect();
                    self.unify_expr_types(&refs, "ARRAY")?
                };

                // Insert implicit casts for elements that don't match the unified type.
                let elems = elems
                    .into_iter()
                    .map(|e| self.coerce_if_needed(e, &elem_type))
                    .collect();

                Ok(TypedExpr::new(
                    TypedExprKind::ArrayLiteral(elems),
                    DataType::Array(Box::new(elem_type)),
                ))
            }

            Expr::ArrayIndex { obj, indexes } => {
                let arr = self.analyze_expr(obj)?;

                // Multiple dimensions: chain ArrayIndex nodes, unwrapping one
                // Array layer per subscript.
                let mut result = arr;
                for idx_expr in indexes {
                    let idx = self.analyze_expr(idx_expr)?;
                    let elem_type = match &result.data_type {
                        DataType::Array(inner) => inner.as_ref().clone(),
                        other => {
                            return Err(AnalyzerError::OperatorTypeMismatch {
                                operator: "[]".to_string(),
                                left: other.clone(),
                                right: idx.data_type.clone(),
                            });
                        }
                    };
                    result = TypedExpr::new(
                        TypedExprKind::ArrayIndex {
                            array: Box::new(result),
                            index: Box::new(idx),
                        },
                        elem_type,
                    );
                }
                Ok(result)
            }

            // ── JSON access ─────────────────────────────────
            Expr::JsonAccess {
                left,
                operator,
                right,
            } => {
                // sqlparser gives JSON operators lower precedence than comparison
                // operators, so `metadata->>'level' = 'senior'` is parsed as:
                //   JsonAccess(metadata, LongArrow, BinaryOp('level', Eq, 'senior'))
                // We must unwrap the nested operator: analyze the JSON access with
                // only the key, then wrap the result in the outer comparison.
                if let Expr::BinaryOp {
                    left: bin_left,
                    op: bin_op,
                    right: bin_right,
                } = right.as_ref()
                {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: operator.clone(),
                        right: bin_left.clone(),
                    };
                    let outer = Expr::BinaryOp {
                        left: Box::new(json_access),
                        op: bin_op.clone(),
                        right: bin_right.clone(),
                    };
                    return self.analyze_expr(&outer);
                }
                if let Expr::InList {
                    expr: in_expr,
                    list,
                    negated,
                } = right.as_ref()
                {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: operator.clone(),
                        right: in_expr.clone(),
                    };
                    let outer = Expr::InList {
                        expr: Box::new(json_access),
                        list: list.clone(),
                        negated: *negated,
                    };
                    return self.analyze_expr(&outer);
                }

                let l = self.analyze_expr(left)?;
                let r = self.analyze_expr(right)?;
                match operator {
                    // Access operators → JsonAccess IR node
                    ast::JsonOperator::Arrow
                    | ast::JsonOperator::LongArrow
                    | ast::JsonOperator::HashArrow
                    | ast::JsonOperator::HashLongArrow => {
                        let json_op = match operator {
                            ast::JsonOperator::Arrow => JsonAccessOp::Arrow,
                            ast::JsonOperator::LongArrow => JsonAccessOp::LongArrow,
                            ast::JsonOperator::HashArrow => JsonAccessOp::HashArrow,
                            ast::JsonOperator::HashLongArrow => JsonAccessOp::HashLongArrow,
                            _ => unreachable!(),
                        };
                        let dt = match json_op {
                            JsonAccessOp::Arrow | JsonAccessOp::HashArrow => DataType::Jsonb,
                            JsonAccessOp::LongArrow | JsonAccessOp::HashLongArrow => DataType::Text,
                        };
                        Ok(TypedExpr::new(
                            TypedExprKind::JsonAccess {
                                expr: Box::new(l),
                                path: Box::new(r),
                                operator: json_op,
                            },
                            dt,
                        ))
                    }
                    // Containment operators → BinaryOp IR node (returns Boolean)
                    ast::JsonOperator::AtArrow => Ok(TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(l),
                            op: BinaryOp::JsonContains,
                            right: Box::new(r),
                        },
                        DataType::Boolean,
                    )),
                    ast::JsonOperator::ArrowAt => Ok(TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(l),
                            op: BinaryOp::JsonContainedBy,
                            right: Box::new(r),
                        },
                        DataType::Boolean,
                    )),
                    // @@ (full-text search match) — sqlparser 0.40 routes this
                    // through JsonAccess, but it's a boolean operator.
                    ast::JsonOperator::AtAt => Ok(TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(l),
                            op: BinaryOp::TsMatch,
                            right: Box::new(r),
                        },
                        DataType::Boolean,
                    )),
                    other => Err(AnalyzerError::Unsupported(format!(
                        "JSON operator {:?}",
                        other
                    ))),
                }
            }

            // ── Syntax sugar → normalized to FunctionCall ───
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                let mut args = vec![self.analyze_expr(expr)?];
                if let Some(from) = substring_from {
                    args.push(self.analyze_expr(from)?);
                }
                if let Some(len) = substring_for {
                    args.push(self.analyze_expr(len)?);
                }
                self.make_function_call("SUBSTRING", args)
            }

            Expr::Trim {
                expr,
                trim_where,
                trim_what,
                ..
            } => {
                let func_name = match trim_where {
                    Some(TrimWhereField::Leading) => "LTRIM",
                    Some(TrimWhereField::Trailing) => "RTRIM",
                    Some(TrimWhereField::Both) | None => "BTRIM",
                };
                let mut args = vec![self.analyze_expr(expr)?];
                if let Some(what) = trim_what {
                    args.push(self.analyze_expr(what)?);
                }
                self.make_function_call(func_name, args)
            }

            Expr::Position { expr, r#in } => {
                // POSITION(substr IN str) → STRPOS(str, substr)
                let substr = self.analyze_expr(expr)?;
                let string = self.analyze_expr(r#in)?;
                self.make_function_call("STRPOS", vec![string, substr])
            }

            Expr::Extract { field, expr } => {
                let field_str = format!("{}", field);
                let field_const = TypedExpr::new(
                    TypedExprKind::Constant(Value::Text(field_str)),
                    DataType::Text,
                );
                let date_expr = self.analyze_expr(expr)?;
                self.make_function_call("DATE_PART", vec![field_const, date_expr])
            }

            Expr::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                let ts = self.analyze_expr(timestamp)?;
                let tz = TypedExpr::new(
                    TypedExprKind::Constant(Value::Text(time_zone.clone())),
                    DataType::Text,
                );
                // AT TIME ZONE: tz is first arg per PG convention for timezone()
                self.make_function_call("TIMEZONE", vec![tz, ts])
            }

            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                let mut args = vec![
                    self.analyze_expr(expr)?,
                    self.analyze_expr(overlay_what)?,
                    self.analyze_expr(overlay_from)?,
                ];
                if let Some(len) = overlay_for {
                    args.push(self.analyze_expr(len)?);
                }
                self.make_function_call("OVERLAY", args)
            }

            Expr::Ceil { expr, .. } => {
                let arg = self.analyze_expr(expr)?;
                self.make_function_call("CEIL", vec![arg])
            }

            Expr::Floor { expr, .. } => {
                let arg = self.analyze_expr(expr)?;
                self.make_function_call("FLOOR", vec![arg])
            }

            // ── Interval literal ────────────────────────────
            Expr::Interval(interval) => {
                let iv = self.parse_interval(&interval.value)?;
                Ok(TypedExpr::new(
                    TypedExprKind::Constant(Value::Interval(iv)),
                    DataType::Interval,
                ))
            }

            // ── ArrayAgg → AggregateCall ────────────────────
            Expr::ArrayAgg(agg) => {
                let inner = self.analyze_expr(&agg.expr)?;
                let elem_type = inner.data_type.clone();
                let return_type = DataType::Array(Box::new(elem_type));
                let func = ResolvedFunction {
                    name: "ARRAY_AGG".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: return_type.clone(),
                };

                let order_by = if let Some(ob) = &agg.order_by {
                    self.analyze_order_by_exprs(ob, &[])?
                } else {
                    vec![]
                };

                Ok(TypedExpr::new(
                    TypedExprKind::AggregateCall {
                        func,
                        args: vec![inner],
                        distinct: agg.distinct,
                        order_by,
                        filter: None,
                    },
                    return_type,
                ))
            }

            // ── ANY / ALL ─────────────────────────────────────
            Expr::AnyOp {
                left,
                compare_op,
                right,
            } => {
                let left_expr = self.analyze_expr(left)?;
                let right_expr = self.analyze_expr(right)?;

                // Optimize: `x = ANY(ARRAY[a, b, c])` → `x IN (a, b, c)`
                if matches!(compare_op, BinaryOperator::Eq) {
                    if let TypedExprKind::ArrayLiteral(elems) = right_expr.kind {
                        let mut in_refs: Vec<&TypedExpr> = vec![&left_expr];
                        let elem_refs: Vec<&TypedExpr> = elems.iter().collect();
                        in_refs.extend(elem_refs);
                        let common = self.unify_expr_types(&in_refs, "ANY")?;
                        let left_coerced = self.coerce_if_needed(left_expr, &common);
                        let list = elems
                            .into_iter()
                            .map(|e| self.coerce_if_needed(e, &common))
                            .collect();
                        return Ok(TypedExpr::new(
                            TypedExprKind::InList {
                                expr: Box::new(left_coerced),
                                list,
                                negated: false,
                            },
                            DataType::Boolean,
                        ));
                    }
                }

                // Optimize: `x <> ANY(ARRAY[a, b, c])` → `x NOT IN (a, b, c)`
                if matches!(compare_op, BinaryOperator::NotEq) {
                    if let TypedExprKind::ArrayLiteral(elems) = right_expr.kind {
                        let mut in_refs: Vec<&TypedExpr> = vec![&left_expr];
                        let elem_refs: Vec<&TypedExpr> = elems.iter().collect();
                        in_refs.extend(elem_refs);
                        let common = self.unify_expr_types(&in_refs, "ANY")?;
                        let left_coerced = self.coerce_if_needed(left_expr, &common);
                        let list = elems
                            .into_iter()
                            .map(|e| self.coerce_if_needed(e, &common))
                            .collect();
                        return Ok(TypedExpr::new(
                            TypedExprKind::InList {
                                expr: Box::new(left_coerced),
                                list,
                                negated: true,
                            },
                            DataType::Boolean,
                        ));
                    }
                }

                // General case: `x = ANY(array_col)` where array_col is a column reference
                // or other non-literal array expression.
                // Convert to: ARRAY_POSITION(array_col, x) IS NOT NULL
                if matches!(compare_op, BinaryOperator::Eq) {
                    if matches!(right_expr.data_type, DataType::Array(_))
                        || matches!(&right_expr.data_type, DataType::UserDefined(s) if s == "int2vector")
                    {
                        let array_pos =
                            self.make_function_call("ARRAY_POSITION", vec![right_expr, left_expr])?;
                        return Ok(TypedExpr::new(
                            TypedExprKind::IsTest {
                                expr: Box::new(array_pos),
                                test: IsTestKind::Null,
                                negated: true, // IS NOT NULL
                            },
                            DataType::Boolean,
                        ));
                    }
                }

                Err(AnalyzerError::Unsupported(format!(
                    "ANY with non-array operand or non-equality operator: {:?}",
                    compare_op,
                )))
            }

            // ALL: `x = ALL(ARRAY[a, b, c])` → x = a AND x = b AND x = c
            // ALL with column reference: not yet supported.
            Expr::AllOp {
                left,
                compare_op,
                right,
            } => {
                let left_expr = self.analyze_expr(left)?;
                let right_expr = self.analyze_expr(right)?;

                // `x = ALL(ARRAY[a, b, c])` → x = a AND x = b AND x = c
                if let TypedExprKind::ArrayLiteral(elems) = right_expr.kind {
                    if elems.is_empty() {
                        // ALL of empty array is TRUE by SQL standard
                        return Ok(TypedExpr::new(
                            TypedExprKind::Constant(Value::Boolean(true)),
                            DataType::Boolean,
                        ));
                    }
                    let op = match compare_op {
                        BinaryOperator::Eq => BinaryOp::Eq,
                        BinaryOperator::NotEq => BinaryOp::NotEq,
                        BinaryOperator::Lt => BinaryOp::Lt,
                        BinaryOperator::LtEq => BinaryOp::LtEq,
                        BinaryOperator::Gt => BinaryOp::Gt,
                        BinaryOperator::GtEq => BinaryOp::GtEq,
                        other => {
                            return Err(AnalyzerError::Unsupported(format!(
                                "ALL with operator: {:?}",
                                other,
                            )));
                        }
                    };
                    // Build: (left op elem[0]) AND (left op elem[1]) AND ...
                    let comparisons: Vec<TypedExpr> = elems
                        .into_iter()
                        .map(|elem| {
                            TypedExpr::new(
                                TypedExprKind::BinaryOp {
                                    left: Box::new(left_expr.clone()),
                                    op: op.clone(),
                                    right: Box::new(elem),
                                },
                                DataType::Boolean,
                            )
                        })
                        .collect();
                    let mut result = comparisons.into_iter();
                    let first = result.next().unwrap();
                    let combined = result.fold(first, |acc, next| {
                        TypedExpr::new(
                            TypedExprKind::BinaryOp {
                                left: Box::new(acc),
                                op: BinaryOp::And,
                                right: Box::new(next),
                            },
                            DataType::Boolean,
                        )
                    });
                    return Ok(combined);
                }

                Err(AnalyzerError::Unsupported(format!(
                    "ALL with non-array operand: {:?}",
                    compare_op,
                )))
            }

            // ── Catch-all for unsupported expressions ───────
            other => Err(AnalyzerError::Unsupported(format!(
                "expression type not yet supported: {:?}",
                std::mem::discriminant(other),
            ))),
        }
    }

    // ── Helper: identifier resolution ───────────────────────

    fn analyze_identifier(&mut self, name: &str) -> Result<TypedExpr, AnalyzerError> {
        let resolved = self.scopes.resolve_column(name)?;
        Ok(TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: resolved.scope_depth,
                column_index: resolved.column_index,
                column_name: resolved.column_name,
            },
            resolved.data_type,
        ))
    }

    fn analyze_compound_identifier(
        &mut self,
        parts: &[ast::Ident],
    ) -> Result<TypedExpr, AnalyzerError> {
        if parts.is_empty() {
            return Err(AnalyzerError::Internal(
                "empty compound identifier".to_string(),
            ));
        }

        // Two-part: table.column
        if parts.len() == 2 {
            let resolved = self
                .scopes
                .resolve_qualified_column(&parts[0].value, &parts[1].value)?;
            return Ok(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: resolved.scope_depth,
                    column_index: resolved.column_index,
                    column_name: resolved.column_name,
                },
                resolved.data_type,
            ));
        }

        // Three-part: schema.table.column — use last two parts
        if parts.len() >= 3 {
            let n = parts.len();
            let resolved = self
                .scopes
                .resolve_qualified_column(&parts[n - 2].value, &parts[n - 1].value)?;
            return Ok(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: resolved.scope_depth,
                    column_index: resolved.column_index,
                    column_name: resolved.column_name,
                },
                resolved.data_type,
            ));
        }

        // Single part (shouldn't reach here, but handle gracefully)
        self.analyze_identifier(&parts[0].value)
    }

    // ── Helper: value literal analysis ──────────────────────

    fn analyze_value(&self, val: &ast::Value) -> Result<TypedExpr, AnalyzerError> {
        match val {
            ast::Value::Number(n, _) => {
                if n.contains(['e', 'E']) {
                    let f: f64 = n.parse().map_err(|_| AnalyzerError::InvalidLiteral {
                        value: n.clone(),
                        target_type: DataType::Float64,
                        parse_error: "invalid float".to_string(),
                    })?;
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Float64(f)),
                        DataType::Float64,
                    ))
                } else if n.contains('.') {
                    let d = Decimal::from_str(n).map_err(|e| AnalyzerError::InvalidLiteral {
                        value: n.clone(),
                        target_type: DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                        parse_error: e.to_string(),
                    })?;
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Numeric(d)),
                        DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    ))
                } else if let Ok(i) = n.parse::<i32>() {
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Int32(i)),
                        DataType::Int32,
                    ))
                } else if let Ok(i) = n.parse::<i64>() {
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Int64(i)),
                        DataType::Int64,
                    ))
                } else {
                    let d = Decimal::from_str(n).map_err(|e| AnalyzerError::InvalidLiteral {
                        value: n.clone(),
                        target_type: DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                        parse_error: e.to_string(),
                    })?;
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Numeric(d)),
                        DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    ))
                }
            }

            ast::Value::SingleQuotedString(s)
            | ast::Value::DoubleQuotedString(s)
            | ast::Value::EscapedStringLiteral(s) => Ok(TypedExpr::new(
                TypedExprKind::Constant(Value::Text(s.clone())),
                DataType::Text,
            )),

            ast::Value::Boolean(b) => Ok(TypedExpr::new(
                TypedExprKind::Constant(Value::Boolean(*b)),
                DataType::Boolean,
            )),

            ast::Value::Null => {
                // Untyped NULL — default type is Text (PostgreSQL semantics).
                // The Analyzer may override via contextual coercion in binary ops.
                Ok(TypedExpr::null(DataType::Text))
            }

            ast::Value::HexStringLiteral(s) => {
                let bytes = hex::decode(s).map_err(|e| AnalyzerError::InvalidLiteral {
                    value: s.clone(),
                    target_type: DataType::Bytes,
                    parse_error: e.to_string(),
                })?;
                Ok(TypedExpr::new(
                    TypedExprKind::Constant(Value::Bytes(bytes)),
                    DataType::Bytes,
                ))
            }

            other => Err(AnalyzerError::Unsupported(format!(
                "value literal type: {:?}",
                other,
            ))),
        }
    }

    // ── Helper: binary operators ────────────────────────────

    fn analyze_binary_op(
        &mut self,
        left: &Expr,
        op: &ast::BinaryOperator,
        right: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut l = self.analyze_expr(left)?;
        let mut r = self.analyze_expr(right)?;
        let typed_op = self.convert_binary_op(op)?;

        // Contextual NULL typing: if one side is NULL, adopt the other's type
        if l.is_null_constant() && !r.is_null_constant() {
            l = TypedExpr::null(r.data_type.clone());
        } else if r.is_null_constant() && !l.is_null_constant() {
            r = TypedExpr::null(l.data_type.clone());
        }

        // PostgreSQL UNKNOWN literal rule (partial):
        //
        // String literals are untyped (UNKNOWN) in PostgreSQL and can be coerced
        // to match a numeric operator context. In tipg, string literals are
        // initially typed as TEXT, which would otherwise reject `TEXT + INT`.
        //
        // We only apply this for *literal* text constants (not TEXT columns, and
        // not explicitly typed TEXT via `::text`), matching the desired contract:
        //
        //   SELECT '100' + 50  -> OK (coerce literal to INT)
        //   SELECT '100'::text + 50 -> ERROR
        //   SELECT text_col + 50 -> ERROR
        if matches!(
            typed_op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Mod
                | BinaryOp::Exp
        ) {
            if is_numeric(&r.data_type) && matches!(l.kind, TypedExprKind::Constant(Value::Text(_)))
            {
                l = self.coerce_if_needed(l, &r.data_type);
            } else if is_numeric(&l.data_type)
                && matches!(r.kind, TypedExprKind::Constant(Value::Text(_)))
            {
                r = self.coerce_if_needed(r, &l.data_type);
            }
        }

        // Use our BinaryOp Display impl (outputs "+", "-", "=", etc.)
        // which maps directly to the operator symbols in binary_op_result_type.
        let op_display = typed_op.to_string();
        let result_type = binary_op_result_type(&op_display, &l.data_type, &r.data_type)
            .or_else(|| {
                // Bitwise operators return the common numeric type
                match &typed_op {
                    BinaryOp::BitwiseAnd
                    | BinaryOp::BitwiseOr
                    | BinaryOp::BitwiseXor
                    | BinaryOp::ShiftLeft
                    | BinaryOp::ShiftRight => common_type(&l.data_type, &r.data_type),
                    BinaryOp::Custom(_) => common_type(&l.data_type, &r.data_type),
                    _ => None,
                }
            })
            .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                operator: op_display.clone(),
                left: l.data_type.clone(),
                right: r.data_type.clone(),
            })?;

        // Insert implicit casts when operand types differ and a target type exists.
        // Comparisons use comparison_target_type (non-Text side wins) while
        // arithmetic/other operators continue to use common_type.
        if l.data_type != r.data_type {
            let target_type = match typed_op {
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq => comparison_target_type(&l.data_type, &r.data_type),
                _ => common_type(&l.data_type, &r.data_type),
            };

            if let Some(target) = target_type {
                l = self.coerce_if_needed(l, &target);
                r = self.coerce_if_needed(r, &target);
            }
        }

        Ok(TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(l),
                op: typed_op,
                right: Box::new(r),
            },
            result_type,
        ))
    }

    fn convert_binary_op(&self, op: &ast::BinaryOperator) -> Result<BinaryOp, AnalyzerError> {
        use ast::BinaryOperator as SqlOp;
        Ok(match op {
            SqlOp::Plus => BinaryOp::Add,
            SqlOp::Minus => BinaryOp::Sub,
            SqlOp::Multiply => BinaryOp::Mul,
            SqlOp::Divide => BinaryOp::Div,
            SqlOp::Modulo => BinaryOp::Mod,
            SqlOp::Eq => BinaryOp::Eq,
            SqlOp::NotEq => BinaryOp::NotEq,
            SqlOp::Lt => BinaryOp::Lt,
            SqlOp::LtEq => BinaryOp::LtEq,
            SqlOp::Gt => BinaryOp::Gt,
            SqlOp::GtEq => BinaryOp::GtEq,
            SqlOp::And => BinaryOp::And,
            SqlOp::Or => BinaryOp::Or,
            SqlOp::StringConcat => BinaryOp::Concat,
            SqlOp::BitwiseAnd => BinaryOp::BitwiseAnd,
            SqlOp::BitwiseOr => BinaryOp::BitwiseOr,
            SqlOp::BitwiseXor => BinaryOp::BitwiseXor,
            SqlOp::PGBitwiseShiftLeft => BinaryOp::ShiftLeft,
            SqlOp::PGBitwiseShiftRight => BinaryOp::ShiftRight,
            SqlOp::PGRegexMatch => BinaryOp::RegexMatch,
            SqlOp::PGRegexIMatch => BinaryOp::RegexIMatch,
            SqlOp::PGRegexNotMatch => BinaryOp::RegexNotMatch,
            SqlOp::PGRegexNotIMatch => BinaryOp::RegexNotIMatch,
            SqlOp::PGOverlap => BinaryOp::ArrayOverlap,
            SqlOp::PGExp => BinaryOp::Exp,
            SqlOp::PGCustomBinaryOperator(parts) => {
                let op_str: String = parts.iter().map(|p| p.as_str()).collect();
                match op_str.as_str() {
                    "?|" => BinaryOp::JsonExistsAny,
                    "?&" => BinaryOp::JsonExistsAll,
                    "@@" => BinaryOp::TsMatch,
                    "@>" => BinaryOp::ArrayContains,
                    "<@" => BinaryOp::ArrayContainedBy,
                    other => BinaryOp::Custom(other.to_string()),
                }
            }
            other => {
                return Err(AnalyzerError::Unsupported(format!(
                    "binary operator {:?}",
                    other
                )));
            }
        })
    }

    // ── Helper: unary operators ─────────────────────────────

    fn analyze_unary_op(
        &mut self,
        op: &UnaryOperator,
        expr: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        let operand = self.analyze_expr(expr)?;
        let (typed_op, result_type) = match op {
            UnaryOperator::Not => (UnaryOp::Not, DataType::Boolean),
            UnaryOperator::Plus => (UnaryOp::Plus, operand.data_type.clone()),
            UnaryOperator::Minus => (UnaryOp::Minus, operand.data_type.clone()),
            UnaryOperator::PGBitwiseNot => (UnaryOp::BitwiseNot, operand.data_type.clone()),
            _ => {
                return Err(AnalyzerError::Unsupported(format!(
                    "unary operator {:?}",
                    op,
                )));
            }
        };

        Ok(TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: typed_op,
                operand: Box::new(operand),
            },
            result_type,
        ))
    }

    // ── Helper: IS tests ────────────────────────────────────

    fn analyze_is_test(
        &mut self,
        expr: &Expr,
        test: IsTestKind,
        negated: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let inner = self.analyze_expr(expr)?;
        Ok(TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(inner),
                test,
                negated,
            },
            DataType::Boolean,
        ))
    }

    // ── Helper: LIKE ────────────────────────────────────────

    fn analyze_like(
        &mut self,
        expr: &Expr,
        pattern: &Expr,
        escape: &Option<char>,
        case_insensitive: bool,
        negated: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let e = self.analyze_expr(expr)?;
        let p = self.analyze_expr(pattern)?;
        let esc = escape.map(|c| {
            Box::new(TypedExpr::new(
                TypedExprKind::Constant(Value::Text(c.to_string())),
                DataType::Text,
            ))
        });

        Ok(TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(e),
                pattern: Box::new(p),
                escape: esc,
                case_insensitive,
                negated,
            },
            DataType::Boolean,
        ))
    }

    // ── Helper: CASE ────────────────────────────────────────

    fn analyze_case(
        &mut self,
        operand: &Option<Box<Expr>>,
        conditions: &[Expr],
        results: &[Expr],
        else_result: &Option<Box<Expr>>,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut analyzed_operand = match operand {
            Some(e) => Some(Box::new(self.analyze_expr(e)?)),
            None => None,
        };

        let mut when_clauses = Vec::with_capacity(conditions.len());

        for (cond, result) in conditions.iter().zip(results.iter()) {
            let c = self.analyze_expr(cond)?;
            // For searched CASE (no operand), each WHEN condition must be boolean.
            // For simple CASE (with operand), conditions are values compared to
            // the operand, so any type is valid.
            if analyzed_operand.is_none() && c.data_type != DataType::Boolean {
                return Err(AnalyzerError::TypeMismatch {
                    expected: DataType::Boolean,
                    found: c.data_type.clone(),
                    context: "CASE WHEN condition".to_string(),
                });
            }
            let r = self.analyze_expr(result)?;
            when_clauses.push((c, r));
        }

        // Simple CASE: coerce operand and WHEN values to a single comparison target type.
        //
        // PostgreSQL desugars `CASE operand WHEN v THEN ...` into comparisons
        // (`operand = v`) with coercion. The Typed IR must not rely on runtime
        // comparison coercion; insert casts here so executor evaluation only
        // compares type-compatible values.
        if let Some(op) = analyzed_operand.take() {
            let mut target = op.data_type.clone();
            for (when_expr, _) in &when_clauses {
                target =
                    comparison_target_type(&target, &when_expr.data_type).ok_or_else(|| {
                        AnalyzerError::OperatorTypeMismatch {
                            operator: "=".to_string(),
                            left: target.clone(),
                            right: when_expr.data_type.clone(),
                        }
                    })?;
            }

            analyzed_operand = Some(Box::new(self.coerce_if_needed(*op, &target)));
            when_clauses = when_clauses
                .into_iter()
                .map(|(cond, result)| (self.coerce_if_needed(cond, &target), result))
                .collect();
        }

        let analyzed_else = match else_result {
            Some(e) => Some(Box::new(self.analyze_expr(e)?)),
            None => None,
        };

        // Collect result expression refs for NULL-aware type unification.
        let result_refs: Vec<&TypedExpr> = when_clauses
            .iter()
            .map(|(_, r)| r)
            .chain(analyzed_else.iter().map(|e| &**e))
            .collect();
        let result_type = self.unify_expr_types(&result_refs, "CASE")?;

        // Insert implicit casts on result expressions to match the unified type.
        let when_clauses = when_clauses
            .into_iter()
            .map(|(cond, result)| (cond, self.coerce_if_needed(result, &result_type)))
            .collect();
        let analyzed_else =
            analyzed_else.map(|e| Box::new(self.coerce_if_needed(*e, &result_type)));

        Ok(TypedExpr::new(
            TypedExprKind::Case {
                operand: analyzed_operand,
                when_clauses,
                else_result: analyzed_else,
            },
            result_type,
        ))
    }

    // ── Helper: function analysis ───────────────────────────

    fn analyze_function(&mut self, func: &Function) -> Result<TypedExpr, AnalyzerError> {
        let mut func_name = function_name_upper(func);

        // Preserve schema prefix for schema-qualified functions (e.g., cron.schedule).
        // function_name_upper() only takes the last segment of ObjectName, stripping
        // schema qualifiers. We need the full qualified name for dispatch in classify.rs,
        // materialize.rs, and typed_eval.rs.
        if func.name.0.len() > 1 {
            let schema = func.name.0[0].value.to_lowercase();
            if schema == "cron" {
                func_name = format!("cron.{}", func_name);
            }
        }

        // Extract function arguments
        let args = self.extract_function_args(func)?;

        // Analyze argument expressions
        let analyzed_args: Vec<TypedExpr> = args
            .iter()
            .map(|a| self.analyze_expr(a))
            .collect::<Result<_, _>>()?;

        let arg_types: Vec<DataType> = analyzed_args.iter().map(|a| a.data_type.clone()).collect();

        // Intercept conditional expressions that need dedicated IR variants.
        // sqlparser 0.40 parses COALESCE/NULLIF/GREATEST/LEAST as Expr::Function,
        // but they have special short-circuit semantics requiring dedicated IR nodes.
        match func_name.as_str() {
            "COALESCE" => return self.analyze_coalesce(analyzed_args),
            "NULLIF" => return self.analyze_nullif(analyzed_args),
            "GREATEST" => return self.analyze_greatest_least(analyzed_args, true),
            "LEAST" => return self.analyze_greatest_least(analyzed_args, false),
            _ => {}
        }

        // Analyze FILTER clause
        let filter = match &func.filter {
            Some(f) => Some(Box::new(self.analyze_expr(f)?)),
            None => None,
        };

        // Analyze ORDER BY within function
        let order_by = if func.order_by.is_empty() {
            vec![]
        } else {
            self.analyze_order_by_exprs(&func.order_by, &[])?
        };

        // Resolve function from registry
        let registry = global_registry();

        if let Some(sig) = registry.get(&func_name) {
            // Validate argument count
            let arg_count = analyzed_args.len();
            if arg_count < sig.min_args {
                return Err(AnalyzerError::ArgumentCountMismatch {
                    function: func_name,
                    expected_min: sig.min_args,
                    expected_max: sig.max_args,
                    got: arg_count,
                });
            }
            if let Some(max) = sig.max_args {
                if arg_count > max {
                    return Err(AnalyzerError::ArgumentCountMismatch {
                        function: func_name,
                        expected_min: sig.min_args,
                        expected_max: sig.max_args,
                        got: arg_count,
                    });
                }
            }

            // Reject window-only functions used without OVER clause.
            // Functions like ROW_NUMBER(), RANK() are meaningless without a window.
            if sig.is_window && !sig.is_aggregate && func.over.is_none() {
                return Err(AnalyzerError::WindowNotAllowed {
                    function: func_name,
                    context: "called without OVER clause".to_string(),
                });
            }

            // Resolve return type using the signature's ReturnType resolver.
            // None here means the argument types don't match the resolver
            // (e.g., SameAsArg(0) with no args after arg count validation
            // passed — indicates a registry bug). Treat as error, not silent
            // fallback, to surface misconfigurations.
            let return_type = registry
                .resolve_return_type(&func_name, &arg_types)
                .ok_or_else(|| AnalyzerError::FunctionNotFound {
                    name: func_name.clone(),
                    arg_types: arg_types.clone(),
                })?;

            let resolved = ResolvedFunction {
                name: func_name.clone(),
                kind: FunctionKind::Builtin,
                return_type: return_type.clone(),
            };

            // Determine expression kind based on function classification + OVER clause
            if func.over.is_some() {
                // Window functions are only allowed in SELECT, ORDER BY, and HAVING.
                if !self.scopes.current().allow_windows {
                    return Err(AnalyzerError::WindowNotAllowed {
                        function: func_name,
                        context: "WHERE clause or GROUP BY".to_string(),
                    });
                }
                // Window function
                let (partition_by, window_order_by, window_frame) =
                    self.analyze_window_spec(func)?;

                return Ok(TypedExpr::new(
                    TypedExprKind::WindowCall {
                        func: resolved,
                        args: analyzed_args,
                        partition_by,
                        order_by: window_order_by,
                        window_frame,
                    },
                    return_type,
                ));
            }

            if sig.is_aggregate {
                if !self.scopes.current().allow_aggregates {
                    return Err(AnalyzerError::AggregateNotAllowed {
                        function: func_name,
                        context: "WHERE clause or GROUP BY".to_string(),
                    });
                }
                return Ok(TypedExpr::new(
                    TypedExprKind::AggregateCall {
                        func: resolved,
                        args: analyzed_args,
                        distinct: func.distinct,
                        order_by,
                        filter,
                    },
                    return_type,
                ));
            }

            return Ok(TypedExpr::new(
                TypedExprKind::FunctionCall {
                    func: resolved,
                    args: analyzed_args,
                    order_by,
                    filter,
                },
                return_type,
            ));
        }

        // Not in builtin registry — check catalog for UDF
        if let Ok(Some(func_def)) = self.catalog.resolve_function(&func_name, None, &arg_types) {
            let return_type = sql_datatype_to_internal(&sqlparser::ast::DataType::Custom(
                sqlparser::ast::ObjectName(vec![ast::Ident::new(&func_def.return_type)]),
                vec![],
            ))
            .map_err(|e| {
                AnalyzerError::Unsupported(format!(
                    "UDF {}: unsupported return type '{}': {}",
                    func_name, func_def.return_type, e
                ))
            })?;

            let resolved = ResolvedFunction {
                name: func_name,
                kind: FunctionKind::UserDefined { oid: func_def.oid },
                return_type: return_type.clone(),
            };

            return Ok(TypedExpr::new(
                TypedExprKind::FunctionCall {
                    func: resolved,
                    args: analyzed_args,
                    order_by,
                    filter,
                },
                return_type,
            ));
        }

        // Unknown function — treat as opaque call returning Text.
        // The runtime function registry (eval_expr) handles many pg-specific functions
        // that aren't registered in the type registry. Rather than hard-failing at
        // analysis time, we pass through and let runtime evaluation handle dispatch.
        let resolved = ResolvedFunction {
            name: func_name,
            kind: FunctionKind::Builtin,
            return_type: DataType::Text,
        };
        Ok(TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: resolved,
                args: analyzed_args,
                order_by,
                filter,
            },
            DataType::Text,
        ))
    }

    /// Extract argument expressions from a function call, flattening named args.
    fn extract_function_args<'b>(
        &self,
        func: &'b Function,
    ) -> Result<Vec<&'b Expr>, AnalyzerError> {
        let mut exprs = Vec::new();
        for arg in &func.args {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => exprs.push(e),
                FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => exprs.push(e),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {
                    // COUNT(*) — no argument to analyze
                }
                _ => {}
            }
        }
        Ok(exprs)
    }

    /// Create a resolved scalar FunctionCall from a function name and analyzed args.
    ///
    /// Used for syntax sugar normalization (SUBSTRING → FunctionCall, etc.).
    /// These are known builtins, so we can rely on the registry.
    pub(super) fn make_function_call(
        &self,
        name: &str,
        args: Vec<TypedExpr>,
    ) -> Result<TypedExpr, AnalyzerError> {
        let arg_types: Vec<DataType> = args.iter().map(|a| a.data_type.clone()).collect();

        let registry = global_registry();
        let return_type = registry
            .resolve_return_type(name, &arg_types)
            .ok_or_else(|| AnalyzerError::FunctionNotFound {
                name: name.to_string(),
                arg_types: arg_types.clone(),
            })?;

        let func = ResolvedFunction {
            name: name.to_string(),
            kind: FunctionKind::Builtin,
            return_type: return_type.clone(),
        };

        Ok(TypedExpr::new(
            TypedExprKind::FunctionCall {
                func,
                args,
                order_by: vec![],
                filter: None,
            },
            return_type,
        ))
    }

    // ── Helpers: conditional expressions ─────────────────────
    // These produce dedicated IR variants instead of FunctionCall,
    // because they have short-circuit or comparison semantics.

    fn analyze_coalesce(&self, args: Vec<TypedExpr>) -> Result<TypedExpr, AnalyzerError> {
        if args.is_empty() {
            return Err(AnalyzerError::ArgumentCountMismatch {
                function: "COALESCE".to_string(),
                expected_min: 1,
                expected_max: None,
                got: 0,
            });
        }
        let refs: Vec<&TypedExpr> = args.iter().collect();
        let unified = self.unify_expr_types(&refs, "COALESCE")?;
        let args = args
            .into_iter()
            .map(|a| self.coerce_if_needed(a, &unified))
            .collect();
        Ok(TypedExpr::new(TypedExprKind::Coalesce(args), unified))
    }

    fn analyze_nullif(&self, args: Vec<TypedExpr>) -> Result<TypedExpr, AnalyzerError> {
        if args.len() != 2 {
            return Err(AnalyzerError::ArgumentCountMismatch {
                function: "NULLIF".to_string(),
                expected_min: 2,
                expected_max: Some(2),
                got: args.len(),
            });
        }
        let dt = args[0].data_type.clone();
        let mut it = args.into_iter();
        let a = it.next().unwrap();
        let b = it.next().unwrap();
        Ok(TypedExpr::new(
            TypedExprKind::NullIf(Box::new(a), Box::new(b)),
            dt,
        ))
    }

    fn analyze_greatest_least(
        &self,
        args: Vec<TypedExpr>,
        is_greatest: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let name = if is_greatest { "GREATEST" } else { "LEAST" };
        if args.is_empty() {
            return Err(AnalyzerError::ArgumentCountMismatch {
                function: name.to_string(),
                expected_min: 1,
                expected_max: None,
                got: 0,
            });
        }
        let refs: Vec<&TypedExpr> = args.iter().collect();
        let unified = self.unify_expr_types(&refs, name)?;
        let args = args
            .into_iter()
            .map(|a| self.coerce_if_needed(a, &unified))
            .collect();
        Ok(TypedExpr::new(
            TypedExprKind::MinMax { args, is_greatest },
            unified,
        ))
    }

    // ── Helper: window spec ─────────────────────────────────

    fn analyze_window_spec(
        &mut self,
        func: &Function,
    ) -> Result<(Vec<TypedExpr>, Vec<TypedOrderByExpr>, Option<WindowFrame>), AnalyzerError> {
        let window_type = match &func.over {
            Some(w) => w,
            None => return Ok((vec![], vec![], None)),
        };

        match window_type {
            WindowType::WindowSpec(spec) => {
                let partition_by: Vec<TypedExpr> = spec
                    .partition_by
                    .iter()
                    .map(|e| self.analyze_expr(e))
                    .collect::<Result<_, _>>()?;

                let order_by = self.analyze_order_by_exprs(&spec.order_by, &[])?;

                let window_frame = match &spec.window_frame {
                    Some(frame) => Some(self.convert_window_frame(frame)?),
                    None => None,
                };

                Ok((partition_by, order_by, window_frame))
            }
            WindowType::NamedWindow(_) => Err(AnalyzerError::Unsupported(
                "named window references".to_string(),
            )),
        }
    }

    fn convert_window_frame(
        &mut self,
        frame: &ast::WindowFrame,
    ) -> Result<WindowFrame, AnalyzerError> {
        let units = match frame.units {
            ast::WindowFrameUnits::Rows => WindowFrameUnits::Rows,
            ast::WindowFrameUnits::Range => WindowFrameUnits::Range,
            ast::WindowFrameUnits::Groups => WindowFrameUnits::Groups,
        };

        let start = self.convert_window_frame_bound(&frame.start_bound)?;
        let end = match &frame.end_bound {
            Some(b) => Some(self.convert_window_frame_bound(b)?),
            None => None,
        };

        Ok(WindowFrame { units, start, end })
    }

    fn convert_window_frame_bound(
        &mut self,
        bound: &ast::WindowFrameBound,
    ) -> Result<WindowFrameBound, AnalyzerError> {
        Ok(match bound {
            ast::WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
            ast::WindowFrameBound::Preceding(None) => WindowFrameBound::Preceding(None),
            ast::WindowFrameBound::Preceding(Some(e)) => {
                WindowFrameBound::Preceding(Some(Box::new(self.analyze_expr(e)?)))
            }
            ast::WindowFrameBound::Following(None) => WindowFrameBound::Following(None),
            ast::WindowFrameBound::Following(Some(e)) => {
                WindowFrameBound::Following(Some(Box::new(self.analyze_expr(e)?)))
            }
        })
    }

    // ── Helper: implicit cast insertion ─────────────────────

    /// Wrap an expression in an implicit Cast if its type differs from the target.
    ///
    /// Used after `unify_types` to ensure all branches/arguments have uniform
    /// types in the IR — the evaluator never needs runtime coercion.
    ///
    /// NULL constants are retyped directly (no Cast node needed).
    fn coerce_if_needed(&self, expr: TypedExpr, target: &DataType) -> TypedExpr {
        if expr.data_type == *target {
            expr
        } else if expr.is_null_constant() {
            // NULL constants can be retyped directly — no Cast node needed.
            TypedExpr::null(target.clone())
        } else {
            TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: target.clone(),
                    cast_context: CastContext::Implicit,
                },
                target.clone(),
            )
        }
    }

    /// Unify types across a list of expressions, treating NULL constants as
    /// wildcards (they adopt the unified type of the non-NULL expressions).
    ///
    /// If all expressions are NULL, defaults to Text (PostgreSQL semantics).
    /// Takes references to avoid cloning entire expressions just for type inspection.
    fn unify_expr_types(
        &self,
        exprs: &[&TypedExpr],
        context: &str,
    ) -> Result<DataType, AnalyzerError> {
        let non_null_types: Vec<DataType> = exprs
            .iter()
            .filter(|e| !e.is_null_constant())
            .map(|e| e.data_type.clone())
            .collect();

        if non_null_types.is_empty() {
            // All NULLs → default to Text (PostgreSQL: standalone NULL is text)
            return Ok(DataType::Text);
        }

        unify_types(&non_null_types).ok_or_else(|| AnalyzerError::TypesCannotBeMatched {
            types: exprs.iter().map(|e| e.data_type.clone()).collect(),
            context: context.to_string(),
        })
    }

    // ── Helper: ORDER BY analysis ───────────────────────────

    pub(super) fn analyze_order_by_exprs(
        &mut self,
        order_by: &[ast::OrderByExpr],
        projection: &[AnalyzedProjection],
    ) -> Result<Vec<TypedOrderByExpr>, AnalyzerError> {
        order_by
            .iter()
            .map(|ob| {
                // PostgreSQL: ORDER BY can reference output aliases. Check first.
                let expr = if let Expr::Identifier(ident) = &ob.expr {
                    let name_lower = ident.value.to_lowercase();
                    if let Some(proj) = projection
                        .iter()
                        .find(|p| p.output_name.to_lowercase() == name_lower)
                    {
                        proj.expr.clone()
                    } else {
                        self.analyze_expr(&ob.expr)?
                    }
                } else if let Expr::Value(ast::Value::Number(n, _)) = &ob.expr {
                    // ORDER BY <position> (1-based)
                    if let Ok(pos) = n.parse::<usize>() {
                        if pos >= 1 && pos <= projection.len() {
                            projection[pos - 1].expr.clone()
                        } else {
                            self.analyze_expr(&ob.expr)?
                        }
                    } else {
                        self.analyze_expr(&ob.expr)?
                    }
                } else {
                    self.analyze_expr(&ob.expr)?
                };
                Ok(TypedOrderByExpr {
                    expr,
                    asc: ob.asc.unwrap_or(true),
                    nulls_first: ob.nulls_first.unwrap_or(false),
                })
            })
            .collect()
    }
}
