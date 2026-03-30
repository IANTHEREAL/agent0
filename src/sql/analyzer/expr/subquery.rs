use sqlparser::ast::Expr;

use crate::model::DataType;
use crate::sql::types::coercion::comparison_target_type;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    pub(super) fn analyze_subquery_expr(
        &mut self,
        query: &sqlparser::ast::Query,
    ) -> Result<TypedExpr, AnalyzerError> {
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

    pub(super) fn analyze_exists(
        &mut self,
        subquery: &sqlparser::ast::Query,
        negated: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let analyzed = self.analyze_query(subquery)?;
        Ok(TypedExpr::new(
            TypedExprKind::Exists {
                subquery: Box::new(analyzed),
                negated,
            },
            DataType::Boolean,
        ))
    }

    pub(super) fn analyze_in_subquery(
        &mut self,
        expr: &Expr,
        subquery: &sqlparser::ast::Query,
        negated: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        // Tuple form: (col1, col2, ...) [NOT] IN (SELECT ...)
        if let Expr::Tuple(tuple_exprs) = expr {
            let mut analyzed_exprs: Vec<TypedExpr> = tuple_exprs
                .iter()
                .map(|e| self.analyze_expr(e))
                .collect::<Result<Vec<_>, _>>()?;
            let analyzed = self.analyze_query(subquery)?;
            if analyzed.output_schema.len() != analyzed_exprs.len() {
                return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                    got: analyzed.output_schema.len(),
                });
            }
            // Pairwise coercion: coerce each tuple element to be
            // comparison-compatible with the corresponding subquery
            // output column, matching the scalar InSubquery path.
            for (i, (_, sub_type, _)) in analyzed.output_schema.iter().enumerate() {
                let elem = &analyzed_exprs[i];
                if elem.data_type != *sub_type {
                    if elem.is_null_constant() {
                        analyzed_exprs[i] = TypedExpr::null(sub_type.clone());
                    } else if let Some(target) = crate::sql::types::coercion::comparison_target_type(
                        &elem.data_type,
                        sub_type,
                    ) {
                        analyzed_exprs[i] =
                            self.coerce_if_needed(analyzed_exprs[i].clone(), &target)?;
                    }
                }
            }
            return Ok(TypedExpr::new(
                TypedExprKind::TupleInSubquery {
                    exprs: analyzed_exprs,
                    subquery: Box::new(analyzed),
                    negated,
                },
                DataType::Boolean,
            ));
        }

        let mut e = self.analyze_expr(expr)?;
        let analyzed = self.analyze_query(subquery)?;
        if analyzed.output_schema.len() != 1 {
            return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                got: analyzed.output_schema.len(),
            });
        }
        // Coerce LHS to be comparison-compatible with subquery output.
        // We cannot easily wrap the subquery output in a cast, so coerce
        // the LHS to the comparison target type (mirrors analyze_any_all_subquery).
        let sub_type = &analyzed.output_schema[0].1;
        if e.data_type != *sub_type {
            if e.is_null_constant() {
                e = TypedExpr::null(sub_type.clone());
            } else if let Some(target) = comparison_target_type(&e.data_type, sub_type) {
                e = self.coerce_if_needed(e, &target)?;
            }
        }
        Ok(TypedExpr::new(
            TypedExprKind::InSubquery {
                expr: Box::new(e),
                subquery: Box::new(analyzed),
                negated,
            },
            DataType::Boolean,
        ))
    }

    pub(super) fn analyze_array_subquery(
        &mut self,
        query: &sqlparser::ast::Query,
    ) -> Result<TypedExpr, AnalyzerError> {
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
}
