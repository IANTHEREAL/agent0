//! `ANY` / `ALL` quantified comparison analysis.

use sqlparser::ast::{BinaryOperator, Expr};

use crate::model::{DataType, Value};
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::unify_types;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

use super::coercion::extract_array_literal_elems;

impl<'a> Analyzer<'a> {
    fn implicit_text_array_coercion_source(expr: &TypedExpr) -> Option<&TypedExpr> {
        match &expr.kind {
            TypedExprKind::Cast {
                expr: inner,
                target_type,
                cast_context: CastContext::Implicit,
            } if Self::is_text_like_type(target_type)
                && !Self::is_text_like_type(&inner.data_type) =>
            {
                Some(inner.as_ref())
            }
            _ => None,
        }
    }

    fn has_explicit_text_like_array_cast(expr: &TypedExpr) -> bool {
        match &expr.kind {
            TypedExprKind::Cast {
                expr: inner,
                target_type,
                cast_context: CastContext::Explicit,
            } => {
                matches!(target_type, DataType::Array(elem) if Self::is_text_like_type(elem))
                    || Self::has_explicit_text_like_array_cast(inner)
            }
            TypedExprKind::Cast { expr: inner, .. } => {
                Self::has_explicit_text_like_array_cast(inner)
            }
            _ => false,
        }
    }

    /// For `ANY/ALL` over non-empty literal arrays, recover RHS element type for
    /// operator resolution from concrete non-text members when array-level
    /// unification widened to text due UNKNOWN literals.
    ///
    /// Example (PostgreSQL): `1 = ANY(ARRAY[1, '2'])` resolves as integer
    /// comparison, while `1 = ANY(ARRAY['1', '2'])` remains integer vs text error.
    fn comparison_target_type_for_any_literal_array(
        &self,
        left_expr: &TypedExpr,
        elems: &[TypedExpr],
        fallback_elem_type: &DataType,
        allow_text_recovery: bool,
    ) -> Option<DataType> {
        let mut rhs_elem_type = fallback_elem_type.clone();

        if allow_text_recovery && !elems.is_empty() && Self::is_text_like_type(fallback_elem_type) {
            let mut concrete_non_text = Vec::new();
            let mut has_non_unknown_text_like = false;

            for elem in elems {
                if self.any_rhs_elem_is_semantically_unknown(elem) {
                    continue;
                }

                if let Some(inner) = Self::implicit_text_array_coercion_source(elem) {
                    concrete_non_text.push(inner.data_type.clone());
                    continue;
                }

                if Self::is_text_like_type(&elem.data_type) {
                    has_non_unknown_text_like = true;
                } else {
                    concrete_non_text.push(elem.data_type.clone());
                }
            }

            if !has_non_unknown_text_like && !concrete_non_text.is_empty() {
                rhs_elem_type = unify_types(&concrete_non_text)?;
            }
        }

        self.comparison_target_type_for_any(left_expr, &rhs_elem_type)
    }

    /// Analyze `x <op> ANY(rhs)` expressions.
    ///
    /// Handles subquery forms, literal array optimizations (`= ANY(ARRAY[...])` to
    /// `IN (...)`), `<> ANY` via `ScalarArrayCmp`, non-literal arrays via
    /// `__DB9_EQ_ANY`, and parameter type inference for Prisma-style `= ANY($1)`.
    pub(super) fn analyze_any_op(
        &mut self,
        left: &Expr,
        compare_op: &BinaryOperator,
        right: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        if let Expr::Subquery(subquery) = right {
            return self.analyze_any_all_subquery(left, compare_op, subquery, false);
        }
        if let Expr::ArraySubquery(subquery) = right {
            return self.analyze_any_all_subquery(left, compare_op, subquery, false);
        }

        let mut left_expr = self.analyze_expr(left)?;
        let mut right_expr = self.analyze_expr(right)?;

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
                    let common = self
                        .comparison_target_type_for_any(&left_expr, &elem_type)
                        .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                            operator: compare_op.to_string(),
                            left: left_expr.data_type.pg_display_name(),
                            right: elem_type.pg_display_name(),
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
                let elem_type = match &right_expr.data_type {
                    DataType::Array(inner) => inner.as_ref().clone(),
                    _ => left_expr.data_type.clone(),
                };
                let allow_text_recovery = !Self::has_explicit_text_like_array_cast(&right_expr);
                let common = self
                    .comparison_target_type_for_any_literal_array(
                        &left_expr,
                        &elems,
                        &elem_type,
                        allow_text_recovery,
                    )
                    .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                        operator: compare_op.to_string(),
                        left: left_expr.data_type.pg_display_name(),
                        right: elem_type.pg_display_name(),
                    })?;
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
                    let common = self
                        .comparison_target_type_for_any(&left_expr, &elem_type)
                        .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                            operator: compare_op.to_string(),
                            left: left_expr.data_type.pg_display_name(),
                            right: elem_type.pg_display_name(),
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
                let elem_type = match &right_expr.data_type {
                    DataType::Array(inner) => inner.as_ref().clone(),
                    _ => left_expr.data_type.clone(),
                };
                let allow_text_recovery = !Self::has_explicit_text_like_array_cast(&right_expr);
                let common = self
                    .comparison_target_type_for_any_literal_array(
                        &left_expr,
                        &elems,
                        &elem_type,
                        allow_text_recovery,
                    )
                    .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                        operator: compare_op.to_string(),
                        left: left_expr.data_type.pg_display_name(),
                        right: elem_type.pg_display_name(),
                    })?;
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
        // or other non-literal array expression. Use a dedicated helper instead
        // of ARRAY_POSITION(... ) IS NOT NULL because ANY uses SQL equality
        // three-valued logic, while array_position uses IS NOT DISTINCT FROM
        // semantics for NULL needles.
        if matches!(compare_op, BinaryOperator::Eq)
            && (matches!(right_expr.data_type, DataType::Array(_))
                || matches!(
                    &right_expr.data_type,
                    DataType::UserDefined(s)
                        if s.eq_ignore_ascii_case("int2vector")
                            || s.eq_ignore_ascii_case("oidvector")
                ))
        {
            // Coerce element types so LHS and array elements are compatible.
            // e.g. uuid_col = ANY(text_array) → cast array to uuid[], matching
            // how analyze_binary_op uses comparison_target_type for plain `=`.
            if let DataType::Array(elem_type) = &right_expr.data_type {
                if **elem_type != left_expr.data_type {
                    use crate::sql::types::coercion::comparison_target_type;
                    if let Some(target) = comparison_target_type(&left_expr.data_type, elem_type) {
                        if **elem_type != target {
                            right_expr = TypedExpr::new(
                                TypedExprKind::Cast {
                                    expr: Box::new(right_expr),
                                    target_type: DataType::Array(Box::new(target.clone())),
                                    cast_context: crate::sql::types::CastContext::Implicit,
                                },
                                DataType::Array(Box::new(target.clone())),
                            );
                        }
                        if left_expr.data_type != target {
                            left_expr = self.coerce_if_needed(left_expr, &target)?;
                        }
                    }
                }
            }

            return self.make_function_call("__DB9_EQ_ANY", vec![right_expr, left_expr]);
        }

        // Infer array type for unresolved parameters in = ANY() context.
        // Prisma schema engine sends `WHERE col = ANY($1)` with OID=0;
        // PostgreSQL infers $1 as array(col_type) from context.
        if matches!(compare_op, BinaryOperator::Eq) {
            if let TypedExprKind::Parameter { index } = &right_expr.kind {
                if self.is_unresolved_param(&right_expr) {
                    let array_type = DataType::Array(Box::new(left_expr.data_type.clone()));
                    self.resolve_param_type(*index, &array_type)?;
                    let right_fixed =
                        TypedExpr::new(TypedExprKind::Parameter { index: *index }, array_type);
                    return self.make_function_call("__DB9_EQ_ANY", vec![right_fixed, left_expr]);
                }
            }
        }

        Err(AnalyzerError::Unsupported(format!(
            "ANY: right operand type {:?}, compare_op {:?}",
            right_expr.data_type, compare_op,
        )))
    }

    /// Analyze `x <op> ALL(rhs)` expressions.
    ///
    /// Handles subquery forms and literal array expansion
    /// (`= ALL(ARRAY[a, b, c])` to `a AND b AND c` chain).
    pub(super) fn analyze_all_op(
        &mut self,
        left: &Expr,
        compare_op: &BinaryOperator,
        right: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        if let Expr::Subquery(subquery) = right {
            return self.analyze_any_all_subquery(left, compare_op, subquery, true);
        }
        if let Expr::ArraySubquery(subquery) = right {
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
            // Coerce each element to be comparison-compatible with the LHS.
            let comparisons: Vec<TypedExpr> = elems
                .into_iter()
                .map(|elem| {
                    let (l, r) = self.ensure_comparison_compatible(left_expr.clone(), elem)?;
                    Ok(TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(l),
                            op: op.clone(),
                            right: Box::new(r),
                        },
                        DataType::Boolean,
                    ))
                })
                .collect::<Result<Vec<_>, AnalyzerError>>()?;
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
}
