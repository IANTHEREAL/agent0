//! Comparison expression analysis: BETWEEN, IN list, SIMILAR TO.

use sqlparser::ast::Expr;

use crate::model::{DataType, Value};

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
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
