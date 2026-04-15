//! Comparison expression analysis: BETWEEN, IN list, SIMILAR TO.

use sqlparser::ast::Expr;

use crate::model::{DataType, Value};

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    fn analyze_row_in_list(
        &mut self,
        left: TypedExpr,
        list: Vec<TypedExpr>,
        negated: bool,
    ) -> Result<Option<TypedExpr>, AnalyzerError> {
        let TypedExprKind::Row(left_items) = &left.kind else {
            return Ok(None);
        };

        if !list
            .iter()
            .all(|item| matches!(item.kind, TypedExprKind::Row(_)))
        {
            return Ok(None);
        }

        let arity = left_items.len();
        let mut list_rows: Vec<Vec<TypedExpr>> = Vec::with_capacity(list.len());
        for item in &list {
            let TypedExprKind::Row(items) = &item.kind else {
                unreachable!("checked above");
            };
            if items.len() != arity {
                return Err(AnalyzerError::TypesCannotBeMatched {
                    types: vec![left.data_type.clone(), item.data_type.clone()],
                    context: "IN list row arity".to_string(),
                });
            }
            list_rows.push(items.clone());
        }

        let mut coerced_left_items = left_items.clone();
        for idx in 0..arity {
            let mut refs: Vec<&TypedExpr> = vec![&coerced_left_items[idx]];
            refs.extend(list_rows.iter().map(|row| &row[idx]));
            let common = self.unify_expr_types(&refs, "IN list")?;

            coerced_left_items[idx] =
                self.coerce_if_needed(coerced_left_items[idx].clone(), &common)?;
            for row in &mut list_rows {
                row[idx] = self.coerce_if_needed(row[idx].clone(), &common)?;
            }
        }

        let coerced_left = TypedExpr::new(TypedExprKind::Row(coerced_left_items), left.data_type);
        let coerced_list = list_rows
            .into_iter()
            .map(|items| {
                TypedExpr::new(
                    TypedExprKind::Row(items),
                    DataType::UserDefined("record".to_string()),
                )
            })
            .collect();

        Ok(Some(TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(coerced_left),
                list: coerced_list,
                negated,
            },
            DataType::Boolean,
        )))
    }

    pub(super) fn analyze_between(&mut self, expr: &Expr) -> Result<TypedExpr, AnalyzerError> {
        let Expr::Between {
            expr,
            negated,
            low,
            high,
        } = expr
        else {
            unreachable!("analyze_between called with non-Between expr");
        };
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

    pub(super) fn analyze_in_list(&mut self, expr: &Expr) -> Result<TypedExpr, AnalyzerError> {
        let Expr::InList {
            expr,
            list,
            negated,
        } = expr
        else {
            unreachable!("analyze_in_list called with non-InList expr");
        };
        let e = self.analyze_expr(expr)?;
        let analyzed_list: Vec<TypedExpr> = list
            .iter()
            .map(|item| self.analyze_expr(item))
            .collect::<Result<_, _>>()?;
        if let Some(row_expr) =
            self.analyze_row_in_list(e.clone(), analyzed_list.clone(), *negated)?
        {
            return Ok(row_expr);
        }
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

    pub(super) fn analyze_similar_to(&mut self, expr: &Expr) -> Result<TypedExpr, AnalyzerError> {
        let Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } = expr
        else {
            unreachable!("analyze_similar_to called with non-SimilarTo expr");
        };
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
}
