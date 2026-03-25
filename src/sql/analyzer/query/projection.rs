//! SELECT projection analysis and wildcard expansion.
//!
//! Handles `analyze_projection`, `build_wildcard_projection_order`,
//! `collect_wildcard_sources`, `leaf_wildcard_source_schema`, and
//! `scope_column_projection`.

use sqlparser::ast::{Select, SelectItem};

use crate::model::{ColumnDef, DataType, TableSchema};
use crate::sql::collation::ResolvedCollation;
use crate::sql::expr::collation_aware::extract_resolved_collation;

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
            AnalyzedTableRefKind::Table { name, schema } => TableSchema::virtual_table(
                name.clone(),
                schema
                    .columns
                    .iter()
                    .map(|(col_name, data_type, nullable)| {
                        ColumnDef::new(col_name.clone(), data_type.clone(), *nullable)
                    })
                    .collect(),
            ),
            AnalyzedTableRefKind::Subquery(query) => TableSchema::virtual_table(
                "subquery",
                query
                    .output_schema
                    .iter()
                    .map(|(col_name, data_type, _coll)| {
                        ColumnDef::new(col_name.clone(), data_type.clone(), true)
                    })
                    .collect(),
            ),
            AnalyzedTableRefKind::Function {
                func,
                output_columns,
                ..
            } => TableSchema::virtual_table(
                func.name.clone(),
                output_columns
                    .iter()
                    .map(|(col_name, data_type)| {
                        ColumnDef::new(col_name.clone(), data_type.clone(), true)
                    })
                    .collect(),
            ),
            AnalyzedTableRefKind::Join { .. } => unreachable!(),
        }
    }

    pub(super) fn scope_column_projection(
        &self,
        scope: &Scope,
        column_index: usize,
    ) -> Result<Option<(String, TypedExpr, DataType)>, AnalyzerError> {
        let Some(col) = scope.columns().get(column_index) else {
            return Ok(None);
        };
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

            let mut expr = TypedExpr::new(
                TypedExprKind::Coalesce(vec![
                    Self::cast_to_implicit(left_expr, &merged.data_type),
                    Self::cast_to_implicit(right_expr, &merged.data_type),
                ]),
                merged.data_type.clone(),
            );

            // Wrap in Collate if the USING merged column has a declared collation.
            // Hard-fail on invalid collation — consistent with all other column paths.
            if let Some(ref collation) = col.collation {
                let lower = collation.to_lowercase();
                if lower != "default" {
                    let resolved = self
                        .resolve_collation(collation)
                        .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                    expr = TypedExpr::new(
                        TypedExprKind::Collate {
                            expr: Box::new(expr),
                            collation: collation.clone(),
                            resolved,
                        },
                        merged.data_type.clone(),
                    );
                }
            }

            return Ok(Some((
                col.column_name.clone(),
                expr,
                merged.data_type.clone(),
            )));
        }

        let mut expr = TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: col.column_index,
                column_name: col.column_name.clone(),
            },
            col.data_type.clone(),
        );

        // Wrap in Collate if the column has a declared collation (preserves
        // collation semantics through SELECT * / wildcard expansion).
        // Hard-fail on invalid collation — consistent with explicit column paths
        // in analyze_identifier / analyze_compound_identifier.
        if let Some(ref collation) = col.collation {
            let lower = collation.to_lowercase();
            if lower != "default" {
                let resolved = self
                    .resolve_collation(collation)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                expr = TypedExpr::new(
                    TypedExprKind::Collate {
                        expr: Box::new(expr),
                        collation: collation.clone(),
                        resolved,
                    },
                    col.data_type.clone(),
                );
            }
        }

        Ok(Some((col.column_name.clone(), expr, col.data_type.clone())))
    }

    #[allow(clippy::type_complexity)]
    pub(in crate::sql::analyzer) fn analyze_projection(
        &mut self,
        items: &[SelectItem],
        wildcard_order: Option<&[(String, usize)]>,
    ) -> Result<
        (
            Vec<AnalyzedProjection>,
            Vec<(String, DataType, Option<ResolvedCollation>)>,
        ),
        AnalyzerError,
    > {
        let mut projection = Vec::new();
        let mut output_schema = Vec::new();

        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let analyzed = self.analyze_expr(expr)?;
                    let analyzed = self.resolve_projection_param_type(analyzed)?;
                    let name = self.infer_column_alias(expr);
                    let coll = extract_resolved_collation(&analyzed);
                    output_schema.push((name.clone(), analyzed.data_type.clone(), coll));
                    projection.push(AnalyzedProjection {
                        expr: analyzed,
                        output_name: name,
                    });
                }

                SelectItem::ExprWithAlias { expr, alias } => {
                    let analyzed = self.analyze_expr(expr)?;
                    let analyzed = self.resolve_projection_param_type(analyzed)?;
                    let name = alias.value.clone();
                    let coll = extract_resolved_collation(&analyzed);
                    output_schema.push((name.clone(), analyzed.data_type.clone(), coll));
                    projection.push(AnalyzedProjection {
                        expr: analyzed,
                        output_name: name,
                    });
                }

                SelectItem::Wildcard(_) => {
                    let scope = self.scopes.current();
                    if let Some(order) = wildcard_order {
                        for (name, column_index) in order {
                            // Skip hidden (dropped) columns in the ordered path.
                            if let Some(col) = scope.columns().get(*column_index) {
                                if col.hidden {
                                    continue;
                                }
                            }
                            if let Some((_col_name, expr, data_type)) =
                                self.scope_column_projection(scope, *column_index)?
                            {
                                let coll = extract_resolved_collation(&expr);
                                output_schema.push((name.clone(), data_type.clone(), coll));
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
                            self.scope_column_projection(scope, col.column_index)?
                        {
                            let coll = extract_resolved_collation(&expr);
                            output_schema.push((name.clone(), data_type.clone(), coll));
                            projection.push(AnalyzedProjection {
                                expr,
                                output_name: name,
                            });
                        }
                    }
                }

                SelectItem::QualifiedWildcard(name, _) => {
                    // 3+ identifier parts before `.*` (e.g. `db.schema.table.*`):
                    // PostgreSQL rejects with "cross-database references are
                    // not implemented". Three idents = db.schema.table.
                    if name.0.len() >= 3 {
                        return Err(AnalyzerError::CrossDatabaseReference(format!("{}.*", name)));
                    }

                    // Extract the last identifier as the table qualifier.
                    // For `t.*` → ident "t"; for `schema.t.*` → ident "t".
                    let table_ident = name.0.last().ok_or_else(|| {
                        AnalyzerError::Unsupported("empty qualified wildcard".to_string())
                    })?;
                    let scope = self.scopes.current();

                    let matching_cols = scope.columns_for_table_alias_ident(table_ident);

                    if matching_cols.is_empty() {
                        return Err(AnalyzerError::ColumnNotFound {
                            name: format!("{}.*", name),
                            available: self.scopes.current().available_columns(),
                        });
                    }

                    // Validate schema prefix for 2-part qualified wildcards
                    // (e.g. `schema.table.*`). PostgreSQL requires:
                    // - The binding must be an unaliased real table (not CTE/subquery)
                    // - The schema must match the source schema
                    // When the table has an alias, source_schema is not recorded
                    // (see from_clause.rs), so `source_schemas_for_alias` returns None
                    // and we reject the reference — PG requires bare alias usage.
                    // When the same table name appears from multiple schemas
                    // (e.g. `FROM s1.t, s2.t`), the schema prefix disambiguates:
                    // `s1.t.*` expands only s1.t's columns (matching PG 17.7).
                    let schema_col_range = if name.0.len() == 2 {
                        let schema_ident = &name.0[0];
                        let schema_name = crate::sql::names::normalize_ident(schema_ident);
                        let table_name = crate::sql::names::normalize_ident(table_ident);
                        match scope.source_schemas_for_alias(&table_name) {
                            Some(entries) => {
                                // Find entries whose schema matches the qualifier.
                                let matched: Vec<_> =
                                    entries.iter().filter(|(s, _)| *s == schema_name).collect();
                                match matched.len() {
                                    0 => {
                                        // Schema doesn't match any recorded entry
                                        return Err(AnalyzerError::ColumnNotFound {
                                            name: format!("{}.*", name),
                                            available: self.scopes.current().available_columns(),
                                        });
                                    }
                                    1 => {
                                        // Exactly one match — use its column range
                                        // to filter the expansion below.
                                        Some(matched[0].1.clone())
                                    }
                                    _ => {
                                        // Same schema appears multiple times for the
                                        // same alias — truly ambiguous.
                                        let tables: Vec<String> =
                                            matched.iter().map(|(s, _)| s.clone()).collect();
                                        return Err(AnalyzerError::AmbiguousColumn {
                                            name: format!("{}.*", name),
                                            tables,
                                        });
                                    }
                                }
                            }
                            None => {
                                // No source schema: binding is a CTE, subquery,
                                // or aliased table — cannot be schema-qualified
                                return Err(AnalyzerError::ColumnNotFound {
                                    name: format!("{}.*", name),
                                    available: self.scopes.current().available_columns(),
                                });
                            }
                        }
                    } else {
                        None
                    };

                    for col in matching_cols {
                        // When a schema-qualified wildcard matched a specific
                        // table binding, only expand columns within that range.
                        if let Some(ref range) = schema_col_range {
                            if !range.contains(&col.column_index) {
                                continue;
                            }
                        }
                        let mut expr = TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: col.column_index,
                                column_name: col.column_name.clone(),
                            },
                            col.data_type.clone(),
                        );

                        // Wrap in Collate if column has a declared collation.
                        // Hard-fail on invalid collation — consistent with explicit
                        // column paths in analyze_identifier / analyze_compound_identifier.
                        let mut coll: Option<ResolvedCollation> = None;
                        if let Some(ref collation) = col.collation {
                            let lower = collation.to_lowercase();
                            if lower != "default" {
                                let resolved = self
                                    .resolve_collation(collation)
                                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                                coll = Some(resolved.clone());
                                expr = TypedExpr::new(
                                    TypedExprKind::Collate {
                                        expr: Box::new(expr),
                                        collation: collation.clone(),
                                        resolved,
                                    },
                                    col.data_type.clone(),
                                );
                            }
                        }
                        output_schema.push((col.column_name.clone(), col.data_type.clone(), coll));
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
