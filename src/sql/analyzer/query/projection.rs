//! SELECT projection analysis and wildcard expansion.
//!
//! Handles `analyze_projection`, `build_wildcard_projection_order`,
//! `collect_wildcard_sources`, `leaf_wildcard_source_schema`, and
//! `scope_column_projection`.

use sqlparser::ast::{self as ast, Select, SelectItem};

use crate::types::{ColumnDef, DataType, TableSchema};

use super::super::error::AnalyzerError;
use super::super::scope::Scope;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    pub(super) fn build_wildcard_projection_order(
        select: &Select,
        from_refs: &[AnalyzedTableRef],
    ) -> Option<Vec<(String, usize)>> {
        let mut sources: Vec<TableSchema> = Vec::new();
        for table_ref in from_refs {
            Self::collect_wildcard_sources(table_ref, &mut sources);
        }

        let schema_refs: Vec<&TableSchema> = sources.iter().collect();
        let plan = crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs)?;
        if !plan.any_merge {
            return None;
        }

        let mut source_offsets = Vec::with_capacity(sources.len());
        let mut next_offset = 0usize;
        for source in &sources {
            source_offsets.push(next_offset);
            next_offset = next_offset.saturating_add(source.columns.len());
        }

        let mut ordered = Vec::with_capacity(plan.columns.len());
        for col in plan.columns {
            let source_offset = *source_offsets.get(col.source_idx)?;
            ordered.push((col.name, source_offset + col.col_idx));
        }
        Some(ordered)
    }

    fn collect_wildcard_sources(table_ref: &AnalyzedTableRef, out: &mut Vec<TableSchema>) {
        match &table_ref.kind {
            AnalyzedTableRefKind::Join { left, right, .. } => {
                Self::collect_wildcard_sources(left, out);
                Self::collect_wildcard_sources(right, out);
            }
            _ => out.push(Self::leaf_wildcard_source_schema(table_ref)),
        }
    }

    fn leaf_wildcard_source_schema(table_ref: &AnalyzedTableRef) -> TableSchema {
        match &table_ref.kind {
            AnalyzedTableRefKind::Table { name, schema } => TableSchema {
                name: name.clone(),
                table_id: 0,
                columns: schema
                    .columns
                    .iter()
                    .map(|(col_name, data_type, nullable)| ColumnDef {
                        name: col_name.clone(),
                        data_type: data_type.clone(),
                        nullable: *nullable,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    })
                    .collect(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            },
            AnalyzedTableRefKind::Subquery(query) => TableSchema {
                name: "subquery".to_string(),
                table_id: 0,
                columns: query
                    .output_schema
                    .iter()
                    .map(|(col_name, data_type)| ColumnDef {
                        name: col_name.clone(),
                        data_type: data_type.clone(),
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    })
                    .collect(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            },
            AnalyzedTableRefKind::Function {
                func,
                output_columns,
                ..
            } => TableSchema {
                name: func.name.clone(),
                table_id: 0,
                columns: output_columns
                    .iter()
                    .map(|(col_name, data_type)| ColumnDef {
                        name: col_name.clone(),
                        data_type: data_type.clone(),
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    })
                    .collect(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            },
            AnalyzedTableRefKind::Join { .. } => unreachable!(),
        }
    }

    pub(super) fn scope_column_projection(
        scope: &Scope,
        column_index: usize,
    ) -> Option<(String, TypedExpr, DataType)> {
        let col = scope.columns().get(column_index)?;
        if let Some(merged) = scope.using_column_for_left(column_index) {
            let left_expr = TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: merged.left_index,
                    column_name: merged.column_name.clone(),
                },
                merged.left_type.clone(),
            );
            let right_expr = TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: merged.right_index,
                    column_name: merged.column_name.clone(),
                },
                merged.right_type.clone(),
            );

            let expr = TypedExpr::new(
                TypedExprKind::Coalesce(vec![
                    Self::cast_to_implicit(left_expr, &merged.data_type),
                    Self::cast_to_implicit(right_expr, &merged.data_type),
                ]),
                merged.data_type.clone(),
            );
            return Some((col.column_name.clone(), expr, merged.data_type.clone()));
        }

        let expr = TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: col.column_index,
                column_name: col.column_name.clone(),
            },
            col.data_type.clone(),
        );
        Some((col.column_name.clone(), expr, col.data_type.clone()))
    }

    pub(in crate::sql::analyzer) fn analyze_projection(
        &mut self,
        items: &[SelectItem],
        wildcard_order: Option<&[(String, usize)]>,
    ) -> Result<(Vec<AnalyzedProjection>, Vec<(String, DataType)>), AnalyzerError> {
        let mut projection = Vec::new();
        let mut output_schema = Vec::new();

        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let analyzed = self.analyze_expr(expr)?;
                    let analyzed = self.resolve_projection_param_type(analyzed)?;
                    let name = self.infer_column_alias(expr);
                    output_schema.push((name.clone(), analyzed.data_type.clone()));
                    projection.push(AnalyzedProjection {
                        expr: analyzed,
                        output_name: name,
                    });
                }

                SelectItem::ExprWithAlias { expr, alias } => {
                    let analyzed = self.analyze_expr(expr)?;
                    let analyzed = self.resolve_projection_param_type(analyzed)?;
                    let name = alias.value.clone();
                    output_schema.push((name.clone(), analyzed.data_type.clone()));
                    projection.push(AnalyzedProjection {
                        expr: analyzed,
                        output_name: name,
                    });
                }

                SelectItem::Wildcard(_) => {
                    let scope = self.scopes.current();
                    if let Some(order) = wildcard_order {
                        for (name, column_index) in order {
                            if let Some((_col_name, expr, data_type)) =
                                Self::scope_column_projection(scope, *column_index)
                            {
                                output_schema.push((name.clone(), data_type.clone()));
                                projection.push(AnalyzedProjection {
                                    expr,
                                    output_name: name.clone(),
                                });
                            }
                        }
                        continue;
                    }

                    for col in scope.columns() {
                        // Skip USING join right-side duplicate columns.
                        if col.hidden {
                            continue;
                        }
                        if let Some((name, expr, data_type)) =
                            Self::scope_column_projection(scope, col.column_index)
                        {
                            output_schema.push((name.clone(), data_type.clone()));
                            projection.push(AnalyzedProjection {
                                expr,
                                output_name: name,
                            });
                        }
                    }
                }

                SelectItem::QualifiedWildcard(name, _) => {
                    let table_name = name.to_string();
                    let scope = self.scopes.current();
                    let lower_table = table_name.to_lowercase();

                    let matching_cols: Vec<_> = scope
                        .columns()
                        .iter()
                        .filter(|c| {
                            c.table_alias
                                .as_ref()
                                .map(|a| a.to_lowercase() == lower_table)
                                .unwrap_or(false)
                        })
                        .collect();

                    if matching_cols.is_empty() {
                        return Err(AnalyzerError::ColumnNotFound {
                            name: format!("{}.*", table_name),
                            available: self.scopes.current().available_columns(),
                        });
                    }

                    for col in matching_cols {
                        let expr = TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: col.column_index,
                                column_name: col.column_name.clone(),
                            },
                            col.data_type.clone(),
                        );
                        output_schema.push((col.column_name.clone(), col.data_type.clone()));
                        projection.push(AnalyzedProjection {
                            expr,
                            output_name: col.column_name.clone(),
                        });
                    }
                }
            }
        }

        Ok((projection, output_schema))
    }
}
