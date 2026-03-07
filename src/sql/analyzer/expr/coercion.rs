//! Parameter typing, implicit coercion, type unification, and ORDER BY analysis.
//!
//! Contains `is_unresolved_param`, `resolve_param_type`, `coerce_if_needed`,
//! `unify_expr_types`, `analyze_order_by_exprs`, and `extract_array_literal_elems`.

use sqlparser::ast::{self as ast, Expr};

use crate::model::DataType;
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::{common_type, unify_types};

use crate::sql::analyzer::error::AnalyzerError;
use crate::sql::analyzer::types::*;
use crate::sql::analyzer::Analyzer;

impl<'a> Analyzer<'a> {
    // -- Helper: parameter typing --

    /// True if expr is a Parameter not yet resolved from context or client OID.
    pub(in crate::sql::analyzer) fn is_unresolved_param(&self, expr: &TypedExpr) -> bool {
        if let TypedExprKind::Parameter { index } = &expr.kind {
            self.param_types.get(*index).is_none_or(|v| v.is_none())
                && self.inferred_params.get(*index).is_none_or(|v| v.is_none())
        } else {
            false
        }
    }

    /// Record inferred type for parameter. If already inferred, check compatibility.
    pub(in crate::sql::analyzer) fn resolve_param_type(
        &mut self,
        index: usize,
        data_type: &DataType,
    ) -> Result<(), AnalyzerError> {
        // Client-specified OID makes the parameter type authoritative.
        // Do not run inference/conflict logic in this case; normal operator
        // type checks handle incompatible contexts.
        if self
            .param_types
            .get(index)
            .and_then(|t| t.as_ref())
            .is_some()
        {
            return Ok(());
        }

        if let Some(existing) = self.inferred_params.get(index).cloned().flatten() {
            // Keep the first inferred type stable. Widening here can
            // desynchronize already-built TypedExpr::Parameter node types
            // from finalize_param_types() output.
            if existing != *data_type && common_type(&existing, data_type).is_none() {
                return Err(AnalyzerError::InconsistentParameterTypes {
                    index: index + 1,
                    first: existing,
                    second: data_type.clone(),
                });
            }
        } else if let Some(slot) = self.inferred_params.get_mut(index) {
            *slot = Some(data_type.clone());
        }
        Ok(())
    }

    // -- Helper: implicit cast insertion --

    /// Wrap an expression in an implicit Cast if its type differs from the target.
    ///
    /// Used after `unify_types` to ensure all branches/arguments have uniform
    /// types in the IR -- the evaluator never needs runtime coercion.
    ///
    /// NULL constants are retyped directly (no Cast node needed).
    /// Unresolved parameters are re-typed (no Cast node) and their inferred type
    /// is recorded for `finalize_param_types()`.
    pub(in crate::sql::analyzer) fn coerce_if_needed(
        &mut self,
        expr: TypedExpr,
        target: &DataType,
    ) -> Result<TypedExpr, AnalyzerError> {
        // Snapshot before resolve_param_type mutates inferred_params
        let was_unresolved = self.is_unresolved_param(&expr);

        // For any parameter, always register type for conflict detection.
        // This ensures InconsistentParameterTypes fires when the same $N
        // appears in incompatible type contexts (e.g. WHERE id=$1 AND flag=$1).
        if let TypedExprKind::Parameter { index } = &expr.kind {
            self.resolve_param_type(*index, target)?;
        }

        if expr.data_type == *target {
            Ok(expr)
        } else if expr.is_null_constant() {
            // NULL constants can be retyped directly -- no Cast node needed.
            Ok(TypedExpr::null(target.clone()))
        } else if was_unresolved {
            // Unresolved parameter -- re-type without Cast
            if let TypedExprKind::Parameter { index } = &expr.kind {
                Ok(TypedExpr::new(
                    TypedExprKind::Parameter { index: *index },
                    target.clone(),
                ))
            } else {
                unreachable!()
            }
        } else {
            Ok(TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: target.clone(),
                    cast_context: CastContext::Implicit,
                },
                target.clone(),
            ))
        }
    }

    /// Unify types across a list of expressions, treating NULL constants and
    /// unresolved parameters as wildcards (they adopt the unified type of the
    /// concrete expressions).
    ///
    /// If all expressions are NULL/unresolved params, defaults to Text
    /// (PostgreSQL semantics). `finalize_param_types()` catches truly
    /// unresolved parameters later.
    pub(in crate::sql::analyzer) fn unify_expr_types(
        &self,
        exprs: &[&TypedExpr],
        context: &str,
    ) -> Result<DataType, AnalyzerError> {
        let concrete_types: Vec<DataType> = exprs
            .iter()
            .filter(|e| !e.is_null_constant() && !self.is_unresolved_param(e))
            .map(|e| e.data_type.clone())
            .collect();

        if concrete_types.is_empty() {
            // All NULLs and/or unresolved params -> Text fallback
            return Ok(DataType::Text);
        }

        unify_types(&concrete_types).ok_or_else(|| AnalyzerError::TypesCannotBeMatched {
            types: exprs.iter().map(|e| e.data_type.clone()).collect(),
            context: context.to_string(),
        })
    }

    // -- Helper: ORDER BY analysis --

    pub(in crate::sql::analyzer) fn analyze_order_by_exprs(
        &mut self,
        order_by: &[ast::OrderByExpr],
        projection: &[AnalyzedProjection],
    ) -> Result<Vec<TypedOrderByExpr>, AnalyzerError> {
        order_by
            .iter()
            .map(|ob| {
                // PostgreSQL: ORDER BY can reference output aliases. Check first.
                let expr = if let Expr::Identifier(ident) = &ob.expr {
                    let is_quoted = ident.quote_style.is_some();
                    let id_norm = if is_quoted {
                        ident.value.clone()
                    } else {
                        ident.value.to_lowercase()
                    };
                    if let Some(proj) = projection.iter().find(|p| {
                        if is_quoted {
                            p.output_name == id_norm
                        } else {
                            p.output_name.to_lowercase() == id_norm
                        }
                    }) {
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
                let asc = ob.asc.unwrap_or(true);
                Ok(TypedOrderByExpr {
                    expr,
                    asc,
                    nulls_first: ob.nulls_first.unwrap_or(!asc),
                })
            })
            .collect()
    }
}

/// Extract array literal elements from a typed expression.
///
/// Handles direct `ArrayLiteral` and cast-wrapped array literals
/// (`CAST(ARRAY[...] AS type[])`, `ARRAY[...]::type[]`).
pub(super) fn extract_array_literal_elems(expr: &TypedExpr) -> Option<Vec<TypedExpr>> {
    match &expr.kind {
        TypedExprKind::ArrayLiteral(elems) => Some(elems.clone()),
        TypedExprKind::Cast { expr: inner, .. } => extract_array_literal_elems(inner),
        _ => None,
    }
}
