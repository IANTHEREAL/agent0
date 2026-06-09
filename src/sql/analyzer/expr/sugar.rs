//! Syntax sugar normalization: expressions that desugar to `FunctionCall`.
//!
//! Each arm analyzes its child expressions and then delegates to
//! `make_function_call` — no custom IR nodes are produced.

use sqlparser::ast::{Expr, TrimWhereField};

use crate::model::{DataType, Value};

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    /// Analyze syntax-sugar expressions that normalize to a built-in function call.
    pub(in crate::sql::analyzer) fn analyze_syntax_sugar(
        &mut self,
        expr: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        match expr {
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
                self.make_function_call("EXTRACT", vec![field_const, date_expr])
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

            _ => Err(AnalyzerError::Unsupported(format!(
                "not a syntax-sugar expression: {:?}",
                std::mem::discriminant(expr),
            ))),
        }
    }
}
