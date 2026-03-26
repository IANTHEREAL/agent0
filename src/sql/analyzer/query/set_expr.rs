//! Set expression analysis: UNION, INTERSECT, EXCEPT.
//!
//! Handles `analyze_set_expr` for each branch of a set operation,
//! schema unification across branches, and implicit coercion wrapping.

use sqlparser::ast::{self as ast, SetExpr};

use crate::model::DataType;
use crate::sql::collation::ResolvedCollation;
use crate::sql::types::coercion::common_type;
use crate::sql::types::CastContext;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    /// Analyze a SetExpr (used for set operations' left/right branches).
    ///
    /// Each branch manages its own scope lifecycle independently.
    pub(super) fn analyze_set_expr(
        &mut self,
        set_expr: &SetExpr,
    ) -> Result<AnalyzedQuery, AnalyzerError> {
        match set_expr {
            SetExpr::Select(select) => self.analyze_select(select),
            SetExpr::Values(values) => self.analyze_values(values),
            SetExpr::Query(query) => self.analyze_query(query),
            SetExpr::SetOperation {
                op,
                set_quantifier,
                left,
                right,
            } => {
                let left_query = self.analyze_set_expr(left)?;
                let right_query = self.analyze_set_expr(right)?;
                let all = matches!(
                    set_quantifier,
                    ast::SetQuantifier::All | ast::SetQuantifier::AllByName
                );
                let set_op_kind = match op {
                    ast::SetOperator::Union => SetOpKind::Union,
                    ast::SetOperator::Intersect => SetOpKind::Intersect,
                    ast::SetOperator::Except => SetOpKind::Except,
                };
                // Validate column counts match and unify types.
                let output_schema =
                    self.unify_set_operation_schemas(&left_query, &right_query, set_op_kind)?;
                let left_query =
                    self.wrap_set_op_arm_with_coercion(left_query, &output_schema, "__setop_l")?;
                let right_query =
                    self.wrap_set_op_arm_with_coercion(right_query, &output_schema, "__setop_r")?;
                Ok(AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::SetOperation {
                        op: set_op_kind,
                        all,
                        left: Box::new(left_query),
                        right: Box::new(right_query),
                    },
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema,
                })
            }
            _ => Err(AnalyzerError::Unsupported(
                "unsupported set expression".to_string(),
            )),
        }
    }

    /// Validate and unify schemas across set operation branches.
    ///
    /// Checks that left and right have the same column count, then unifies
    /// corresponding column types (e.g. Int32 + Int64 -> Int64).
    pub(super) fn unify_set_operation_schemas(
        &self,
        left: &AnalyzedQuery,
        right: &AnalyzedQuery,
        op: SetOpKind,
    ) -> Result<Vec<(String, DataType, Option<ResolvedCollation>)>, AnalyzerError> {
        if left.output_schema.len() != right.output_schema.len() {
            return Err(AnalyzerError::SetOperationColumnMismatch {
                left: left.output_schema.len(),
                right: right.output_schema.len(),
            });
        }

        let op_name = match op {
            SetOpKind::Union => "UNION",
            SetOpKind::Intersect => "INTERSECT",
            SetOpKind::Except => "EXCEPT",
        };

        left.output_schema
            .iter()
            .zip(right.output_schema.iter())
            .map(|((name, left_dt, left_coll), (_, right_dt, _right_coll))| {
                if left_dt == right_dt {
                    let dt = super::resolve_unknown_to_text(left_dt.clone());
                    Ok((name.clone(), dt, left_coll.clone()))
                } else {
                    let unified = common_type(left_dt, right_dt).ok_or_else(|| {
                        AnalyzerError::TypesCannotBeMatched {
                            types: vec![left_dt.clone(), right_dt.clone()],
                            context: op_name.to_string(),
                        }
                    })?;
                    let unified = super::resolve_unknown_to_text(unified);
                    Ok((name.clone(), unified, left_coll.clone()))
                }
            })
            .collect()
    }

    /// Wrap a set operation arm in a coercing projection if needed.
    ///
    /// For each output column, if the arm's type differs from the unified output
    /// type, we wrap the arm as:
    ///
    /// ```sql
    /// SELECT CAST(col_i AS unified_i) AS name_i, ...
    /// FROM (<arm>) AS <subquery_alias>
    /// ```
    ///
    /// This preserves the arm's own ORDER BY/LIMIT/OFFSET semantics.
    /// Collation wrappers from the original arm are re-applied so that
    /// collation semantics survive the coercion boundary.
    pub(super) fn wrap_set_op_arm_with_coercion(
        &self,
        arm: AnalyzedQuery,
        unified_schema: &[(String, DataType, Option<ResolvedCollation>)],
        subquery_alias: &str,
    ) -> Result<AnalyzedQuery, AnalyzerError> {
        let arm_output_schema = arm.output_schema.clone();
        let needs_wrap = arm
            .output_schema
            .iter()
            .zip(unified_schema.iter())
            .any(|((_, arm_ty, _), (_, unified_ty, _))| arm_ty != unified_ty);

        if !needs_wrap {
            return Ok(arm);
        }

        // Extract collation names from the original arm before it is moved
        // into the subquery. These are re-applied as Collate wrappers so that
        // collation semantics survive the coercion projection.
        let collation_names = super::extract_output_collation_names(&arm);

        let from = vec![AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Subquery(Box::new(arm)),
            alias: Some(subquery_alias.to_string()),
        }];

        let mut projection: Vec<AnalyzedProjection> = Vec::with_capacity(unified_schema.len());
        for (idx, (out_name, unified_ty, _coll)) in unified_schema.iter().enumerate() {
            // The input type is the arm's output type at this position.
            let (input_name, input_ty, _input_coll) = &arm_output_schema[idx];

            let col_ref = TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: idx,
                    column_name: input_name.clone(),
                },
                input_ty.clone(),
            );

            let mut expr = if col_ref.data_type == *unified_ty {
                col_ref
            } else {
                TypedExpr::new(
                    TypedExprKind::Cast {
                        expr: Box::new(col_ref),
                        target_type: unified_ty.clone(),
                        cast_context: CastContext::Implicit,
                    },
                    unified_ty.clone(),
                )
            };

            // Re-apply collation wrapper from the original arm
            if let Some(Some(ref coll_name)) = collation_names.get(idx) {
                let lower = coll_name.to_lowercase();
                if lower != "default" {
                    let resolved = self
                        .resolve_collation(coll_name)
                        .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                    expr = TypedExpr::new(
                        TypedExprKind::Collate {
                            expr: Box::new(expr),
                            collation: coll_name.clone(),
                            resolved,
                        },
                        unified_ty.clone(),
                    );
                }
            }

            projection.push(AnalyzedProjection {
                expr,
                output_name: out_name.clone(),
            });
        }

        Ok(AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection,
                from,
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: unified_schema.to_vec(),
        })
    }

    pub(super) fn cast_to_implicit(expr: TypedExpr, target_type: &DataType) -> TypedExpr {
        if expr.data_type == *target_type {
            expr
        } else {
            TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: target_type.clone(),
                    cast_context: CastContext::Implicit,
                },
                target_type.clone(),
            )
        }
    }
}
