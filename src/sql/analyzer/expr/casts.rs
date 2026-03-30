//! Type-cast expression analysis: `CAST`, typed string literals, `INTERVAL`.

use sqlparser::ast::{self as ast, Expr};

use crate::model::{DataType, Value};
use crate::sql::types::cast::CastContext;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    /// Analyze an explicit `CAST(expr AS type)` expression.
    pub(in crate::sql::analyzer) fn analyze_cast(
        &mut self,
        expr: &Expr,
        data_type: &ast::DataType,
    ) -> Result<TypedExpr, AnalyzerError> {
        let inner = self.analyze_expr(expr)?;
        let target = self.resolve_sql_data_type(data_type)?;
        // Explicit cast resolves parameter type: `$1::int4`
        // Always record the inferred type for finalize_param_types,
        // but always emit a Cast node so the runtime converts the
        // value even if the wire decoder produces a different Value
        // variant (e.g. parse_pg_array yields Value::Text for UUID
        // strings — the Cast node converts them at eval time).
        if let TypedExprKind::Parameter { index } = &inner.kind {
            self.resolve_param_type(*index, &target)?;
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

    /// Analyze a typed string literal (e.g. `DATE '2024-01-01'`, `TIMESTAMP '...'`).
    pub(in crate::sql::analyzer) fn analyze_typed_string(
        &mut self,
        data_type: &ast::DataType,
        value: &str,
    ) -> Result<TypedExpr, AnalyzerError> {
        let target = self.resolve_sql_data_type(data_type)?;
        let parsed = self.parse_typed_literal(value, &target)?;
        Ok(TypedExpr::new(TypedExprKind::Constant(parsed), target))
    }

    /// Analyze an `INTERVAL '...'` literal.
    pub(in crate::sql::analyzer) fn analyze_interval(
        &mut self,
        interval: &ast::Interval,
    ) -> Result<TypedExpr, AnalyzerError> {
        let iv = self.parse_interval(&interval.value)?;
        Ok(TypedExpr::new(
            TypedExprKind::Constant(Value::Interval(iv)),
            DataType::Interval,
        ))
    }
}
