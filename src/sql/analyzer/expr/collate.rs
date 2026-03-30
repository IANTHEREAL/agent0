use sqlparser::ast;

use crate::model::DataType;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    pub(super) fn analyze_collate(
        &mut self,
        expr: &ast::Expr,
        collation: &ast::ObjectName,
    ) -> Result<TypedExpr, AnalyzerError> {
        let analyzed_expr = self.analyze_expr(expr)?;
        // Validate that the expression is a text type
        match &analyzed_expr.data_type {
            DataType::Text | DataType::Varchar(_) | DataType::Unknown => {}
            other => {
                return Err(AnalyzerError::Unsupported(format!(
                    "COLLATE can only be applied to text types, got {}",
                    other
                )));
            }
        }
        // ObjectName is a Vec<Ident>. PostgreSQL allows at most schema.collation
        // (2-part); 3+ parts are "cross-database references" and rejected.
        // Only pg_catalog is accepted as schema for built-in collations.
        let collation_name = match collation.0.len() {
            1 => crate::sql::names::normalize_ident(&collation.0[0]),
            2 => {
                let schema = crate::sql::names::normalize_ident(&collation.0[0]);
                if schema != "pg_catalog" {
                    // PostgreSQL distinguishes: known schema → "collation not found" (42704),
                    // unknown schema → "schema does not exist" (3F000).
                    let known = matches!(schema.as_str(), "public" | "information_schema")
                        || self.catalog.search_path().iter().any(|s| s == &schema);
                    if known {
                        let coll = crate::sql::names::normalize_ident(&collation.0[1]);
                        return Err(AnalyzerError::CollationNotFound(format!(
                            "{}.{}",
                            schema, coll
                        )));
                    }
                    return Err(AnalyzerError::SchemaNotFound(schema));
                }
                crate::sql::names::normalize_ident(&collation.0[1])
            }
            _ => {
                return Err(AnalyzerError::CrossDatabaseReference(collation.to_string()));
            }
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
}
