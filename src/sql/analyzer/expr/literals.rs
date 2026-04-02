//! Literal and identifier analysis for the expression analyzer.
//!
//! Contains `analyze_identifier`, `analyze_compound_identifier`,
//! `analyze_value`, and `parse_placeholder_index`.

use rust_decimal::Decimal;
use sqlparser::ast;
use std::str::FromStr;

use crate::model::{DataType, Value};

use crate::sql::analyzer::error::AnalyzerError;
use crate::sql::analyzer::types::*;
use crate::sql::analyzer::Analyzer;

impl<'a> Analyzer<'a> {
    fn ident_is_xmin(ident: &ast::Ident) -> bool {
        if ident.quote_style.is_some() {
            ident.value == "xmin"
        } else {
            ident.value.eq_ignore_ascii_case("xmin")
        }
    }

    fn table_ident_is_pg_namespace(&self, table: &ast::Ident) -> bool {
        self.scopes
            .table_ident_matches_relation(table, "pg_catalog.pg_namespace")
    }

    fn ident_is_tableoid(ident: &ast::Ident) -> bool {
        if ident.quote_style.is_some() {
            ident.value == "tableoid"
        } else {
            ident.value.eq_ignore_ascii_case("tableoid")
        }
    }

    /// Resolve the tableoid OID for a table alias.  Returns `Some(oid)` when
    /// the alias is bound to a catalog relation that has a known OID.
    fn resolve_tableoid_for_alias(&self, table: &ast::Ident) -> Option<i64> {
        let relation = self.scopes.resolve_table_source_relation(table)?;
        // Extract the table name from "pg_catalog.pg_extension" → "pg_extension"
        let name = relation
            .strip_prefix("pg_catalog.")
            .or_else(|| relation.strip_prefix("information_schema."))
            .unwrap_or(&relation);
        crate::sql::catalog::catalog_relation_oid("pg_catalog", name)
            .or_else(|| crate::sql::catalog::catalog_relation_oid("information_schema", name))
    }

    fn maybe_pg_namespace_xmin_unqualified(
        &self,
        ident: &ast::Ident,
    ) -> Result<Option<TypedExpr>, AnalyzerError> {
        if !Self::ident_is_xmin(ident) {
            return Ok(None);
        }
        // PostgreSQL exposes xmin as a system column while keeping it out of
        // SELECT * expansion. For synthetic pg_namespace rows we model xmin as
        // a stable constant.
        if self
            .scopes
            .resolve_unqualified_relation_column_scope_depth(
                &ident.value,
                "pg_catalog.pg_namespace",
            )?
            .is_some()
        {
            return Ok(Some(TypedExpr::new(
                TypedExprKind::Constant(Value::Int64(1)),
                DataType::Int64,
            )));
        }
        Ok(None)
    }

    // -- Helper: identifier resolution --

    pub(super) fn analyze_identifier(
        &mut self,
        ident: &ast::Ident,
    ) -> Result<TypedExpr, AnalyzerError> {
        match self.scopes.resolve_column_ident(ident) {
            Ok(resolved) => {
                if let Some(merged) = resolved.merged_using.clone() {
                    let mut left_expr = TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: resolved.scope_depth,
                            column_index: merged.left_index,
                            column_name: merged.column_name.clone(),
                        },
                        merged.left_type.clone(),
                    );
                    let mut right_expr = TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: resolved.scope_depth,
                            column_index: merged.right_index,
                            column_name: merged.column_name.clone(),
                        },
                        merged.right_type.clone(),
                    );

                    if left_expr.data_type != merged.data_type {
                        left_expr = self.coerce_if_needed(left_expr, &merged.data_type)?;
                    }
                    if right_expr.data_type != merged.data_type {
                        right_expr = self.coerce_if_needed(right_expr, &merged.data_type)?;
                    }

                    let mut expr = TypedExpr::new(
                        TypedExprKind::Coalesce(vec![left_expr, right_expr]),
                        merged.data_type.clone(),
                    );

                    // Wrap in Collate if the USING merged column has a declared
                    // collation — consistent with the non-merged path below and
                    // the wildcard path in scope_column_projection.
                    if let Some(collation) = resolved.collation {
                        if collation.to_lowercase() != "default" {
                            let resolved_coll = self
                                .resolve_collation(&collation)
                                .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                            expr = TypedExpr::new(
                                TypedExprKind::Collate {
                                    expr: Box::new(expr),
                                    collation,
                                    resolved: resolved_coll,
                                },
                                merged.data_type,
                            );
                        }
                    }

                    return Ok(expr);
                }

                let mut expr = TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: resolved.scope_depth,
                        column_index: resolved.column_index,
                        column_name: resolved.column_name,
                    },
                    resolved.data_type.clone(),
                );

                // Wrap in Collate if column has a declared collation.
                // Skip "default" — it means "use the database default collation",
                // which is the engine's default compare_text_pg path (no Collate node).
                if let Some(collation) = resolved.collation {
                    if collation.to_lowercase() != "default" {
                        let resolved_coll = self
                            .resolve_collation(&collation)
                            .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                        expr = TypedExpr::new(
                            TypedExprKind::Collate {
                                expr: Box::new(expr),
                                collation,
                                resolved: resolved_coll,
                            },
                            resolved.data_type.clone(),
                        );
                    }
                }

                Ok(expr)
            }
            Err(AnalyzerError::ColumnNotFound { .. }) => {
                if let Some(expr) = self.maybe_pg_namespace_xmin_unqualified(ident)? {
                    return Ok(expr);
                }
                if let Some((scope_depth, cols)) = self.scopes.resolve_table_alias_columns(ident) {
                    let row_items: Vec<TypedExpr> = cols
                        .into_iter()
                        .map(|c| {
                            TypedExpr::new(
                                TypedExprKind::ColumnRef {
                                    scope_depth,
                                    column_index: c.column_index,
                                    column_name: c.column_name,
                                },
                                c.data_type,
                            )
                        })
                        .collect();
                    return Ok(TypedExpr::new(
                        TypedExprKind::Row(row_items),
                        DataType::UserDefined("record".to_string()),
                    ));
                }
                Err(AnalyzerError::ColumnNotFound {
                    name: ident.value.clone(),
                    available: self.scopes.current().available_columns(),
                })
            }
            Err(e) => Err(e),
        }
    }

    pub(super) fn analyze_compound_identifier(
        &mut self,
        parts: &[ast::Ident],
    ) -> Result<TypedExpr, AnalyzerError> {
        if parts.is_empty() {
            return Err(AnalyzerError::Internal(
                "empty compound identifier".to_string(),
            ));
        }

        // Single part (shouldn't reach here, but handle gracefully)
        if parts.len() == 1 {
            return self.analyze_identifier(&parts[0]);
        }

        let (table_ident, column_ident) = if parts.len() == 2 {
            (&parts[0], &parts[1])
        } else {
            let n = parts.len();
            (&parts[n - 2], &parts[n - 1])
        };

        if Self::ident_is_xmin(column_ident) && self.table_ident_is_pg_namespace(table_ident) {
            return Ok(TypedExpr::new(
                TypedExprKind::Constant(Value::Int64(1)),
                DataType::Int64,
            ));
        }

        // tableoid: resolve to the catalog relation OID as a constant.
        // pg_dump SELECTs tableoid from nearly every catalog table.
        if Self::ident_is_tableoid(column_ident) {
            if let Some(oid) = self.resolve_tableoid_for_alias(table_ident) {
                return Ok(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(oid)),
                    DataType::Int64,
                ));
            }
        }

        let resolved = self
            .scopes
            .resolve_qualified_column_idents(table_ident, column_ident)?;
        let mut expr = TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: resolved.scope_depth,
                column_index: resolved.column_index,
                column_name: resolved.column_name,
            },
            resolved.data_type.clone(),
        );

        // Wrap in Collate if column has a declared collation (skip "default").
        if let Some(collation) = resolved.collation {
            if collation.to_lowercase() != "default" {
                let resolved_coll = self
                    .resolve_collation(&collation)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                expr = TypedExpr::new(
                    TypedExprKind::Collate {
                        expr: Box::new(expr),
                        collation,
                        resolved: resolved_coll,
                    },
                    resolved.data_type.clone(),
                );
            }
        }

        Ok(expr)
    }

    // -- Helper: value literal analysis --

    pub(super) fn analyze_value(&self, val: &ast::Value) -> Result<TypedExpr, AnalyzerError> {
        match val {
            ast::Value::Number(n, _) => {
                if n.contains(['e', 'E']) {
                    let d =
                        Decimal::from_scientific(n).map_err(|e| AnalyzerError::InvalidLiteral {
                            value: n.clone(),
                            target_type: DataType::Numeric {
                                precision: None,
                                scale: None,
                            },
                            parse_error: e.to_string(),
                        })?;
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Numeric(d)),
                        DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    ))
                } else if n.contains('.') {
                    let d = Decimal::from_str(n).map_err(|e| AnalyzerError::InvalidLiteral {
                        value: n.clone(),
                        target_type: DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                        parse_error: e.to_string(),
                    })?;
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Numeric(d)),
                        DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    ))
                } else if let Ok(i) = n.parse::<i32>() {
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Int32(i)),
                        DataType::Int32,
                    ))
                } else if let Ok(i) = n.parse::<i64>() {
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Int64(i)),
                        DataType::Int64,
                    ))
                } else {
                    let d = Decimal::from_str(n).map_err(|e| AnalyzerError::InvalidLiteral {
                        value: n.clone(),
                        target_type: DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                        parse_error: e.to_string(),
                    })?;
                    Ok(TypedExpr::new(
                        TypedExprKind::Constant(Value::Numeric(d)),
                        DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    ))
                }
            }

            ast::Value::SingleQuotedString(s)
            | ast::Value::DoubleQuotedString(s)
            | ast::Value::EscapedStringLiteral(s) => Ok(TypedExpr::new(
                TypedExprKind::Constant(Value::Text(s.clone())),
                DataType::Unknown,
            )),

            ast::Value::DollarQuotedString(dqs) => Ok(TypedExpr::new(
                TypedExprKind::Constant(Value::Text(dqs.value.clone())),
                DataType::Unknown,
            )),

            ast::Value::Boolean(b) => Ok(TypedExpr::new(
                TypedExprKind::Constant(Value::Boolean(*b)),
                DataType::Boolean,
            )),

            ast::Value::Null => {
                // Untyped NULL — starts as Unknown (PostgreSQL's UNKNOWNOID).
                // The Analyzer resolves it to a concrete type via contextual
                // coercion in binary ops, function args, or assignment.
                Ok(TypedExpr::null(DataType::Unknown))
            }

            ast::Value::HexStringLiteral(s) => {
                let bytes = hex::decode(s).map_err(|e| AnalyzerError::InvalidLiteral {
                    value: s.clone(),
                    target_type: DataType::Bytes,
                    parse_error: e.to_string(),
                })?;
                Ok(TypedExpr::new(
                    TypedExprKind::Constant(Value::Bytes(bytes)),
                    DataType::Bytes,
                ))
            }

            ast::Value::Placeholder(s) => {
                let index = Self::parse_placeholder_index(s)?;
                if index >= self.param_types.len() {
                    return Err(AnalyzerError::InvalidParameterUsage {
                        index: index + 1,
                        context: format!(
                            "parameter index exceeds placeholder count ({})",
                            self.param_types.len()
                        ),
                    });
                }

                // Determine type: client OID > previously inferred > Text seed.
                // Text seed is intentional -- is_unresolved_param() checks
                // inferred_params, not the data_type on the node, so Text
                // never skews type resolution.
                let data_type = if let Some(Some(dt)) = self.param_types.get(index) {
                    dt.clone()
                } else if let Some(Some(dt)) = self.inferred_params.get(index) {
                    dt.clone()
                } else {
                    DataType::Text // Seed only; filtered out by unify_expr_types
                };

                Ok(TypedExpr::new(
                    TypedExprKind::Parameter { index },
                    data_type,
                ))
            }

            other => Err(AnalyzerError::Unsupported(format!(
                "value literal type: {:?}",
                other,
            ))),
        }
    }

    /// Parse `$N` placeholder string to 0-indexed parameter index.
    /// Rejects `$0` (PG parameters are 1-based).
    pub(in crate::sql::analyzer) fn parse_placeholder_index(
        s: &str,
    ) -> Result<usize, AnalyzerError> {
        let n = s
            .strip_prefix('$')
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or_else(|| AnalyzerError::Unsupported(format!("invalid placeholder: {}", s)))?;
        if n == 0 {
            return Err(AnalyzerError::InvalidParameterUsage {
                index: 0,
                context: "parameters are numbered from $1".to_string(),
            });
        }
        Ok(n - 1) // 0-indexed internally
    }
}
