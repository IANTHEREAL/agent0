//! Expression analysis: single-pass name resolution + type checking.
//!
//! This module implements `Analyzer::analyze_expr`, the heart of the semantic
//! analysis. It walks a `sqlparser::ast::Expr` and produces a `TypedExpr` with:
//! - All names resolved to positional `ColumnRef` nodes
//! - All types checked and annotated
//! - All syntax sugar normalized to canonical forms (FunctionCall)
//! - Implicit casts inserted where needed

mod coercion;
mod functions;
mod literals;
mod operators;

use sqlparser::ast::{self as ast, BinaryOperator, Expr, TrimWhereField};

use crate::model::{DataType, Value};
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::comparison_target_type;
use crate::sql::types::mapping::sql_datatype_to_internal;

use super::error::AnalyzerError;
use super::types::*;
use super::Analyzer;

use coercion::extract_array_literal_elems;

impl<'a> Analyzer<'a> {
    /// Analyze an expression, producing a fully-typed IR node.
    ///
    /// This is the core single-pass analysis: name resolution and type checking
    /// happen simultaneously during one recursive walk (PostgreSQL model).
    pub fn analyze_expr(&mut self, expr: &Expr) -> Result<TypedExpr, AnalyzerError> {
        match expr {
            // -- Leaf nodes --
            Expr::Identifier(ident) => self.analyze_identifier(ident),

            Expr::CompoundIdentifier(parts) => self.analyze_compound_identifier(parts),

            Expr::Value(val) => self.analyze_value(val),

            // -- Parenthesized expression --
            Expr::Nested(inner) => self.analyze_expr(inner),

            // -- Tuple / row constructor syntax --
            Expr::Tuple(items) => {
                let analyzed_items: Vec<TypedExpr> = items
                    .iter()
                    .map(|e| self.analyze_expr(e))
                    .collect::<Result<_, _>>()?;
                Ok(TypedExpr::new(
                    TypedExprKind::Row(analyzed_items),
                    DataType::UserDefined("record".to_string()),
                ))
            }

            // -- Binary operators --
            Expr::BinaryOp { left, op, right } => self.analyze_binary_op(left, op, right),

            // -- Unary operators --
            Expr::UnaryOp { op, expr } => self.analyze_unary_op(op, expr),

            // -- Cast --
            Expr::Cast {
                expr, data_type, ..
            } => {
                let inner = self.analyze_expr(expr)?;
                let target = sql_datatype_to_internal(data_type)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                // Explicit cast resolves parameter type: `$1::int4`
                if let TypedExprKind::Parameter { index } = &inner.kind {
                    let was_unresolved = self.is_unresolved_param(&inner);
                    self.resolve_param_type(*index, &target)?;
                    if was_unresolved {
                        return Ok(TypedExpr::new(
                            TypedExprKind::Parameter { index: *index },
                            target,
                        ));
                    }
                }
                Ok(TypedExpr::new(
                    TypedExprKind::Cast {
                        expr: Box::new(inner),
                        target_type: target.clone(),
                        cast_context: CastContext::Explicit,
                    },
                    target,
                ))
            }

            // -- Typed string literal (DATE '...', TIMESTAMP '...') --
            Expr::TypedString { data_type, value } => {
                let target = sql_datatype_to_internal(data_type)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                let parsed = self.parse_typed_literal(value, &target)?;
                Ok(TypedExpr::new(TypedExprKind::Constant(parsed), target))
            }

            // -- IS tests --
            Expr::IsNull(expr) => self.analyze_is_test(expr, IsTestKind::Null, false),
            Expr::IsNotNull(expr) => self.analyze_is_test(expr, IsTestKind::Null, true),
            Expr::IsTrue(expr) => self.analyze_is_test(expr, IsTestKind::True, false),
            Expr::IsNotTrue(expr) => self.analyze_is_test(expr, IsTestKind::True, true),
            Expr::IsFalse(expr) => self.analyze_is_test(expr, IsTestKind::False, false),
            Expr::IsNotFalse(expr) => self.analyze_is_test(expr, IsTestKind::False, true),
            Expr::IsUnknown(expr) => self.analyze_is_test(expr, IsTestKind::Unknown, false),
            Expr::IsNotUnknown(expr) => self.analyze_is_test(expr, IsTestKind::Unknown, true),

            // -- BETWEEN --
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
                let e = self.coerce_if_needed(e, &common)?;
                let lo = self.coerce_if_needed(lo, &common)?;
                let hi = self.coerce_if_needed(hi, &common)?;
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

            // -- IN list --
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
                let e = self.coerce_if_needed(e, &common)?;
                let analyzed_list = analyzed_list
                    .into_iter()
                    .map(|item| self.coerce_if_needed(item, &common))
                    .collect::<Result<_, _>>()?;
                Ok(TypedExpr::new(
                    TypedExprKind::InList {
                        expr: Box::new(e),
                        list: analyzed_list,
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // -- LIKE / ILIKE --
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

            // -- SIMILAR TO --
            Expr::SimilarTo {
                negated,
                expr,
                pattern,
                escape_char,
            } => {
                let e = self.analyze_expr(expr)?;
                let p = self.analyze_expr(pattern)?;
                let esc = escape_char.map(|c| {
                    Box::new(TypedExpr::new(
                        TypedExprKind::Constant(Value::Text(c.to_string())),
                        DataType::Text,
                    ))
                });
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

            // -- CASE --
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => self.analyze_case(operand, conditions, results, else_result),

            // -- Functions --
            Expr::Function(func) => self.analyze_function(func),

            // -- Subqueries --
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
                // Tuple form: (col1, col2, ...) [NOT] IN (SELECT ...)
                if let Expr::Tuple(tuple_exprs) = expr.as_ref() {
                    let analyzed_exprs: Vec<TypedExpr> = tuple_exprs
                        .iter()
                        .map(|e| self.analyze_expr(e))
                        .collect::<Result<Vec<_>, _>>()?;
                    let analyzed = self.analyze_query(subquery)?;
                    if analyzed.output_schema.len() != analyzed_exprs.len() {
                        return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                            got: analyzed.output_schema.len(),
                        });
                    }
                    return Ok(TypedExpr::new(
                        TypedExprKind::TupleInSubquery {
                            exprs: analyzed_exprs,
                            subquery: Box::new(analyzed),
                            negated: *negated,
                        },
                        DataType::Boolean,
                    ));
                }

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

            // -- Array subquery: ARRAY(SELECT ...) --
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

            // -- Array --
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
                    .collect::<Result<_, _>>()?;

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
                                left: other.to_string().to_lowercase(),
                                right: idx.data_type.to_string().to_lowercase(),
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

            // -- JSON access --
            Expr::JsonAccess {
                left,
                operator,
                right,
            } => {
                // Chained JSON access can arrive as:
                //   JsonAccess(left, op, JsonAccess(path1, op2, path2))
                // Reassociate to preserve JSON semantics:
                //   JsonAccess(JsonAccess(left, op, path1), op2, path2)
                if let Expr::JsonAccess {
                    left: chained_left,
                    operator: chained_op,
                    right: chained_right,
                } = right.as_ref()
                {
                    let left_json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: chained_left.clone(),
                    };
                    let reassociated = Expr::JsonAccess {
                        left: Box::new(left_json_access),
                        operator: *chained_op,
                        right: chained_right.clone(),
                    };
                    return self.analyze_expr(&reassociated);
                }

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
                    // Chained JSON access can arrive as:
                    //   JsonAccess(left, op, BinaryOp(path1, json_op, path2))
                    // Reassociate to preserve JSON access semantics:
                    //   JsonAccess(JsonAccess(left, op, path1), json_op, path2)
                    if let Some(chained_json_op) = Self::binary_op_to_json_access_op(bin_op) {
                        let left_json_access = Expr::JsonAccess {
                            left: left.clone(),
                            operator: *operator,
                            right: bin_left.clone(),
                        };
                        let reassociated = Expr::JsonAccess {
                            left: Box::new(left_json_access),
                            operator: chained_json_op,
                            right: bin_right.clone(),
                        };
                        return self.analyze_expr(&reassociated);
                    }

                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
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
                        operator: *operator,
                        right: in_expr.clone(),
                    };
                    let outer = Expr::InList {
                        expr: Box::new(json_access),
                        list: list.clone(),
                        negated: *negated,
                    };
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsNull(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsNull(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsNotNull(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsNotNull(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsTrue(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsTrue(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsNotTrue(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsNotTrue(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsFalse(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsFalse(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsNotFalse(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsNotFalse(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsUnknown(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsUnknown(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }
                if let Expr::IsNotUnknown(inner) = right.as_ref() {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: *operator,
                        right: inner.clone(),
                    };
                    let outer = Expr::IsNotUnknown(Box::new(json_access));
                    return self.analyze_expr(&outer);
                }

                let l = self.analyze_expr(left)?;
                let r = self.analyze_expr(right)?;
                match operator {
                    // Access operators -> JsonAccess IR node
                    ast::JsonOperator::Arrow
                    | ast::JsonOperator::LongArrow
                    | ast::JsonOperator::HashArrow
                    | ast::JsonOperator::HashLongArrow
                    | ast::JsonOperator::HashMinus => {
                        // Validate left operand is JSON/JSONB (PG compat)
                        match &l.data_type {
                            DataType::Json | DataType::Jsonb => {}
                            other => {
                                let op_str = match operator {
                                    ast::JsonOperator::Arrow => "->",
                                    ast::JsonOperator::LongArrow => "->>",
                                    ast::JsonOperator::HashArrow => "#>",
                                    ast::JsonOperator::HashLongArrow => "#>>",
                                    ast::JsonOperator::HashMinus => "#-",
                                    _ => "json_op",
                                };
                                // PG reports bare string literals as type "unknown".
                                let right_type =
                                    if matches!(&r.kind, TypedExprKind::Constant(Value::Text(_))) {
                                        "unknown".to_string()
                                    } else {
                                        r.data_type.to_string().to_lowercase()
                                    };
                                return Err(AnalyzerError::OperatorTypeMismatch {
                                    operator: op_str.to_string(),
                                    left: other.to_string().to_lowercase(),
                                    right: right_type,
                                });
                            }
                        }
                        let json_op = match operator {
                            ast::JsonOperator::Arrow => JsonAccessOp::Arrow,
                            ast::JsonOperator::LongArrow => JsonAccessOp::LongArrow,
                            ast::JsonOperator::HashArrow => JsonAccessOp::HashArrow,
                            ast::JsonOperator::HashLongArrow => JsonAccessOp::HashLongArrow,
                            ast::JsonOperator::HashMinus => JsonAccessOp::HashMinus,
                            _ => unreachable!(),
                        };
                        let dt = match json_op {
                            JsonAccessOp::Arrow
                            | JsonAccessOp::HashArrow
                            | JsonAccessOp::HashMinus => DataType::Jsonb,
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
                    // Containment operators -> BinaryOp IR node (returns Boolean)
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
                    // @@ (full-text search match) -- sqlparser 0.40 routes this
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

            // -- Syntax sugar -> normalized to FunctionCall --
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
                // POSITION(substr IN str) -> STRPOS(str, substr)
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

            // -- Interval literal --
            Expr::Interval(interval) => {
                let iv = self.parse_interval(&interval.value)?;
                Ok(TypedExpr::new(
                    TypedExprKind::Constant(Value::Interval(iv)),
                    DataType::Interval,
                ))
            }

            // -- ArrayAgg -> AggregateCall --
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

            // -- ANY / ALL --
            Expr::AnyOp {
                left,
                compare_op,
                right,
            } => {
                if let Expr::Subquery(subquery) = right.as_ref() {
                    return self.analyze_any_all_subquery(left, compare_op, subquery, false);
                }
                if let Expr::ArraySubquery(subquery) = right.as_ref() {
                    return self.analyze_any_all_subquery(left, compare_op, subquery, false);
                }

                let left_expr = self.analyze_expr(left)?;
                let right_expr = self.analyze_expr(right)?;

                // Optimize: `x = ANY(ARRAY[a, b, c])` -> `x IN (a, b, c)`
                if matches!(compare_op, BinaryOperator::Eq) {
                    if let Some(elems) = extract_array_literal_elems(&right_expr) {
                        // Empty array: type-check LHS against the array element type,
                        // then return InList so the LHS is still evaluated at runtime
                        // (catches errors like 1/0). Result is FALSE (= ANY of empty set).
                        if elems.is_empty() {
                            let elem_type = match &right_expr.data_type {
                                DataType::Array(inner) => inner.as_ref().clone(),
                                _ => left_expr.data_type.clone(),
                            };
                            // Use comparison semantics (not unify_types which has a
                            // Text universal fallback). Reject text↔non-text mismatches
                            // since no comparison operator exists (PG parity).
                            let lhs_text = is_text_like(&left_expr.data_type);
                            let rhs_text = is_text_like(&elem_type);
                            let common = if lhs_text != rhs_text {
                                None
                            } else {
                                comparison_target_type(&left_expr.data_type, &elem_type)
                            };
                            let common =
                                common.ok_or_else(|| AnalyzerError::TypesCannotBeMatched {
                                    types: vec![
                                        left_expr.data_type.clone(),
                                        right_expr.data_type.clone(),
                                    ],
                                    context: "ANY".to_string(),
                                })?;
                            let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                            return Ok(TypedExpr::new(
                                TypedExprKind::InList {
                                    expr: Box::new(left_coerced),
                                    list: vec![],
                                    negated: false,
                                },
                                DataType::Boolean,
                            ));
                        }
                        let mut in_refs: Vec<&TypedExpr> = vec![&left_expr];
                        let elem_refs: Vec<&TypedExpr> = elems.iter().collect();
                        in_refs.extend(elem_refs);
                        let common = self.unify_expr_types(&in_refs, "ANY")?;
                        let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                        let list = elems
                            .into_iter()
                            .map(|e| self.coerce_if_needed(e, &common))
                            .collect::<Result<_, _>>()?;
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

                // `x <> ANY(ARRAY[a, b, c])` -> ScalarArrayCmp (OR of inequalities).
                // NOT the same as NOT IN which uses AND semantics.
                // ScalarArrayCmp evaluates LHS exactly once and handles empty arrays
                // correctly (LHS is still evaluated for side effects).
                if matches!(compare_op, BinaryOperator::NotEq) {
                    if let Some(elems) = extract_array_literal_elems(&right_expr) {
                        // Empty array: derive element type from right_expr.data_type
                        // to enforce type compatibility. ScalarArrayCmp with empty
                        // elems already evaluates LHS and returns FALSE at runtime.
                        if elems.is_empty() {
                            let elem_type = match &right_expr.data_type {
                                DataType::Array(inner) => inner.as_ref().clone(),
                                _ => left_expr.data_type.clone(),
                            };
                            // Use comparison semantics (not unify_types which has a
                            // Text universal fallback). Reject text↔non-text mismatches
                            // since no comparison operator exists (PG parity).
                            let lhs_text = is_text_like(&left_expr.data_type);
                            let rhs_text = is_text_like(&elem_type);
                            let common = if lhs_text != rhs_text {
                                None
                            } else {
                                comparison_target_type(&left_expr.data_type, &elem_type)
                            };
                            let common =
                                common.ok_or_else(|| AnalyzerError::TypesCannotBeMatched {
                                    types: vec![
                                        left_expr.data_type.clone(),
                                        right_expr.data_type.clone(),
                                    ],
                                    context: "ANY".to_string(),
                                })?;
                            let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                            let op = self.any_all_compare_op(compare_op)?;
                            return Ok(TypedExpr::new(
                                TypedExprKind::ScalarArrayCmp {
                                    expr: Box::new(left_coerced),
                                    elems: vec![],
                                    op,
                                    use_or: true,
                                },
                                DataType::Boolean,
                            ));
                        }
                        let mut in_refs: Vec<&TypedExpr> = vec![&left_expr];
                        let elem_refs: Vec<&TypedExpr> = elems.iter().collect();
                        in_refs.extend(elem_refs);
                        let common = self.unify_expr_types(&in_refs, "ANY")?;
                        let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                        let coerced_elems = elems
                            .into_iter()
                            .map(|e| self.coerce_if_needed(e, &common))
                            .collect::<Result<_, _>>()?;
                        let op = self.any_all_compare_op(compare_op)?;
                        return Ok(TypedExpr::new(
                            TypedExprKind::ScalarArrayCmp {
                                expr: Box::new(left_coerced),
                                elems: coerced_elems,
                                op,
                                use_or: true,
                            },
                            DataType::Boolean,
                        ));
                    }
                }

                // General case: `x = ANY(array_col)` where array_col is a column reference
                // or other non-literal array expression.
                // Convert to: ARRAY_POSITION(array_col, x) IS NOT NULL
                if matches!(compare_op, BinaryOperator::Eq)
                    && (matches!(right_expr.data_type, DataType::Array(_))
                        || matches!(&right_expr.data_type, DataType::UserDefined(s) if s == "int2vector"))
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

                // Infer array type for unresolved parameters in = ANY() context.
                // Prisma schema engine sends `WHERE col = ANY($1)` with OID=0;
                // PostgreSQL infers $1 as array(col_type) from context.
                if matches!(compare_op, BinaryOperator::Eq) {
                    if let TypedExprKind::Parameter { index } = &right_expr.kind {
                        if self.is_unresolved_param(&right_expr) {
                            let array_type = DataType::Array(Box::new(left_expr.data_type.clone()));
                            self.resolve_param_type(*index, &array_type)?;
                            let right_fixed = TypedExpr::new(
                                TypedExprKind::Parameter { index: *index },
                                array_type,
                            );
                            let array_pos = self.make_function_call(
                                "ARRAY_POSITION",
                                vec![right_fixed, left_expr],
                            )?;
                            return Ok(TypedExpr::new(
                                TypedExprKind::IsTest {
                                    expr: Box::new(array_pos),
                                    test: IsTestKind::Null,
                                    negated: true,
                                },
                                DataType::Boolean,
                            ));
                        }
                    }
                }

                Err(AnalyzerError::Unsupported(format!(
                    "ANY: right operand type {:?}, compare_op {:?}",
                    right_expr.data_type, compare_op,
                )))
            }

            // ALL: `x = ALL(ARRAY[a, b, c])` -> x = a AND x = b AND x = c
            // ALL with column reference: not yet supported.
            Expr::AllOp {
                left,
                compare_op,
                right,
            } => {
                if let Expr::Subquery(subquery) = right.as_ref() {
                    return self.analyze_any_all_subquery(left, compare_op, subquery, true);
                }
                if let Expr::ArraySubquery(subquery) = right.as_ref() {
                    return self.analyze_any_all_subquery(left, compare_op, subquery, true);
                }

                let left_expr = self.analyze_expr(left)?;
                let right_expr = self.analyze_expr(right)?;

                // `x = ALL(ARRAY[a, b, c])` -> x = a AND x = b AND x = c
                if let Some(elems) = extract_array_literal_elems(&right_expr) {
                    if elems.is_empty() {
                        // ALL of empty array is TRUE by SQL standard
                        return Ok(TypedExpr::new(
                            TypedExprKind::Constant(Value::Boolean(true)),
                            DataType::Boolean,
                        ));
                    }
                    let op = self.any_all_compare_op(compare_op)?;
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

            // -- COLLATE --
            Expr::Collate { expr, collation } => {
                let analyzed_expr = self.analyze_expr(expr)?;
                // Validate that the expression is a text type
                match &analyzed_expr.data_type {
                    DataType::Text | DataType::Varchar(_) => {}
                    other => {
                        return Err(AnalyzerError::Unsupported(format!(
                            "COLLATE can only be applied to text types, got {}",
                            other
                        )));
                    }
                }
                // ObjectName is a Vec<Ident>, get the last part as collation name
                let collation_name = if collation.0.len() == 1 {
                    crate::sql::names::normalize_ident(&collation.0[0])
                } else {
                    collation.to_string()
                };
                // COLLATE "default" = use the database default collation, which is
                // the engine's default compare_text_pg path. Skip the Collate node.
                if collation_name.to_lowercase() == "default" {
                    return Ok(analyzed_expr);
                }

                // Resolve the collation at analysis time (catalog first, then registry)
                let resolved = self
                    .resolve_collation(&collation_name)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                // COLLATE preserves the input expression's type (P1-7 fix)
                let result_type = analyzed_expr.data_type.clone();
                Ok(TypedExpr::new(
                    TypedExprKind::Collate {
                        expr: Box::new(analyzed_expr),
                        collation: collation_name,
                        resolved,
                    },
                    result_type,
                ))
            }

            // -- IS [NOT] DISTINCT FROM --
            Expr::IsDistinctFrom(left, right) => {
                let l = self.analyze_expr(left)?;
                let r = self.analyze_expr(right)?;
                Ok(TypedExpr::new(
                    TypedExprKind::IsDistinctFrom {
                        left: Box::new(l),
                        right: Box::new(r),
                        negated: false,
                    },
                    DataType::Boolean,
                ))
            }
            Expr::IsNotDistinctFrom(left, right) => {
                let l = self.analyze_expr(left)?;
                let r = self.analyze_expr(right)?;
                Ok(TypedExpr::new(
                    TypedExprKind::IsDistinctFrom {
                        left: Box::new(l),
                        right: Box::new(r),
                        negated: true,
                    },
                    DataType::Boolean,
                ))
            }

            // -- Catch-all for unsupported expressions --
            other => Err(AnalyzerError::Unsupported(format!(
                "expression type not yet supported: {:?}",
                std::mem::discriminant(other),
            ))),
        }
    }
}

/// Returns `true` for text-like types that form a single comparison category.
/// Used to reject text↔non-text comparisons in empty-array ANY/= ANY paths
/// (PostgreSQL has no cross-category comparison operators for these).
fn is_text_like(dt: &DataType) -> bool {
    matches!(dt, DataType::Text | DataType::Varchar(_) | DataType::Name)
}
