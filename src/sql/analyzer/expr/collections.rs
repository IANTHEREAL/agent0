use sqlparser::ast;

use crate::model::DataType;
use crate::sql::types::coercion::unify_types;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    /// ARRAY literal typing contract:
    /// - Treat untyped string literals / NULL / unresolved params as semantically UNKNOWN
    /// - If concrete explicit text-like members and concrete non-text members are both
    ///   present, fail ARRAY type resolution (PG parity)
    /// - If UNKNOWNs caused widening to text-like but concrete members are non-text,
    ///   recover element type from the concrete non-text members
    /// - Respect explicit text-like members/casts (do not recover past them)
    fn array_literal_elem_type(&self, elems: &[TypedExpr]) -> Result<DataType, AnalyzerError> {
        // PG resolves ARRAY['1','2'] to text[] at construction time — it does
        // NOT keep unknown[].  When all elements are Unknown, unify_expr_types
        // filters them out and returns Text (the PG default for unresolved
        // unknown literals).  This means `1 = ANY(ARRAY['1','2'])` correctly
        // errors with "operator does not exist: integer = text", matching PG.
        let refs: Vec<&TypedExpr> = elems.iter().collect();
        let mut elem_type = self.unify_expr_types(&refs, "ARRAY")?;

        if Self::is_text_like_type(&elem_type) {
            let mut concrete_non_text = Vec::new();
            let mut has_concrete_text_like = false;

            for elem in elems {
                if self.any_rhs_elem_is_semantically_unknown(elem) {
                    continue;
                }

                if Self::is_text_like_type(&elem.data_type) {
                    has_concrete_text_like = true;
                } else {
                    concrete_non_text.push(elem.data_type.clone());
                }
            }

            if has_concrete_text_like && !concrete_non_text.is_empty() {
                return Err(AnalyzerError::TypesCannotBeMatched {
                    types: elems.iter().map(|e| e.data_type.clone()).collect(),
                    context: "ARRAY".to_string(),
                });
            }

            if !has_concrete_text_like && !concrete_non_text.is_empty() {
                elem_type = unify_types(&concrete_non_text).ok_or_else(|| {
                    AnalyzerError::TypesCannotBeMatched {
                        types: elems.iter().map(|e| e.data_type.clone()).collect(),
                        context: "ARRAY".to_string(),
                    }
                })?;
            }
        }

        Ok(elem_type)
    }

    pub(super) fn analyze_tuple(
        &mut self,
        items: &[ast::Expr],
    ) -> Result<TypedExpr, AnalyzerError> {
        let analyzed_items: Vec<TypedExpr> = items
            .iter()
            .map(|e| self.analyze_expr(e))
            .collect::<Result<_, _>>()?;
        Ok(TypedExpr::new(
            TypedExprKind::Row(analyzed_items),
            DataType::UserDefined("record".to_string()),
        ))
    }

    pub(super) fn analyze_array_literal(
        &mut self,
        arr: &ast::Array,
    ) -> Result<TypedExpr, AnalyzerError> {
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
            self.array_literal_elem_type(&elems)?
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

    pub(super) fn analyze_array_index(
        &mut self,
        obj: &ast::Expr,
        indexes: &[ast::Expr],
    ) -> Result<TypedExpr, AnalyzerError> {
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
                        left: other.pg_display_name(),
                        right: idx.data_type.pg_display_name(),
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

    pub(super) fn analyze_array_agg(
        &mut self,
        agg: &ast::ArrayAgg,
    ) -> Result<TypedExpr, AnalyzerError> {
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
}
