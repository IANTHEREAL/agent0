//! DML analysis: INSERT, UPDATE, DELETE.
//!
//! Validates column existence, type compatibility, boolean WHERE predicates,
//! and arity constraints at analysis time. Produces typed IR that the executor
//! can evaluate without re-analyzing per row.

use sqlparser::ast::{self, Expr, Ident, ObjectName, OnInsert, Query, SelectItem, SetExpr, Values};

use crate::sql::names::{normalize_ident, split_object_name};
use crate::sql::types::cast::CastContext;
use crate::types::{DataType, Value};

use super::error::AnalyzerError;
use super::scope::Scope;
use super::types::*;
use super::Analyzer;

impl<'a> Analyzer<'a> {
    // ── INSERT ──────────────────────────────────────────────

    /// Analyze an INSERT statement.
    pub fn analyze_insert(
        &mut self,
        table_name: &ObjectName,
        columns: &[Ident],
        source: &Option<Box<Query>>,
        returning: &Option<Vec<SelectItem>>,
        on_conflict: &Option<OnInsert>,
    ) -> Result<AnalyzedInsert, AnalyzerError> {
        // Resolve target table.
        let (resolved_name, table_schema, schema) = self.resolve_dml_target(table_name)?;
        // DML name resolution should follow relation-name semantics for qualified refs:
        // `schema.table.col` and `table.col` both target relation `table`.
        let target_scope_name = resolved_name
            .rsplit('.')
            .next()
            .unwrap_or(resolved_name.as_str())
            .to_string();

        // Map column names to indices.
        let target_columns = if columns.is_empty() {
            // No column list → all columns in schema order.
            (0..schema.columns.len()).collect()
        } else {
            columns
                .iter()
                .map(|ident| {
                    let col_name = normalize_ident(ident);
                    self.find_column_index(&schema, &col_name, &resolved_name)
                })
                .collect::<Result<Vec<usize>, _>>()?
        };

        // Build scope for the target table (needed for RETURNING and ON CONFLICT).
        let table_cols: Vec<(String, DataType, bool, Option<String>)> = schema
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    c.data_type.clone(),
                    c.nullable,
                    c.collation.clone(),
                )
            })
            .collect();

        // Analyze source rows.
        let analyzed_source = match source {
            None => {
                // INSERT ... DEFAULT VALUES
                AnalyzedInsertSource::DefaultValues
            }
            Some(query) => match &*query.body {
                SetExpr::Values(Values { rows, .. }) => {
                    let mut analyzed_rows = Vec::with_capacity(rows.len());
                    for row_exprs in rows {
                        // Validate arity.
                        if row_exprs.len() != target_columns.len() {
                            return Err(AnalyzerError::InsertColumnCountMismatch {
                                columns: target_columns.len(),
                                values: row_exprs.len(),
                            });
                        }
                        // Analyze each value expression in a minimal scope
                        // (values don't reference the target table).
                        self.scopes.push(Scope::new());
                        let mut typed_vals = Vec::with_capacity(row_exprs.len());
                        for (i, expr) in row_exprs.iter().enumerate() {
                            let col_idx = target_columns[i];
                            typed_vals
                                .push(self.analyze_dml_assignment_expr(expr, &schema, col_idx)?);
                        }
                        self.scopes.pop();
                        analyzed_rows.push(typed_vals);
                    }
                    AnalyzedInsertSource::Values(analyzed_rows)
                }
                _ => {
                    // INSERT ... SELECT ...
                    let analyzed_query = self.analyze_query(query)?;

                    // Validate arity: SELECT output columns must match target columns.
                    if analyzed_query.output_schema.len() != target_columns.len() {
                        return Err(AnalyzerError::InsertColumnCountMismatch {
                            columns: target_columns.len(),
                            values: analyzed_query.output_schema.len(),
                        });
                    }

                    AnalyzedInsertSource::Query(Box::new(analyzed_query))
                }
            },
        };

        // Analyze ON CONFLICT.
        let analyzed_on_conflict = if let Some(on) = on_conflict {
            Some(self.analyze_on_conflict(
                on,
                &table_cols,
                &schema,
                &target_scope_name,
                &resolved_name,
            )?)
        } else {
            None
        };

        // Analyze RETURNING.
        let analyzed_returning = if let Some(ret_items) = returning {
            self.scopes
                .push(Scope::from_table_schema(&target_scope_name, &schema));
            let (proj, _) = self.analyze_projection(ret_items, None)?;
            self.scopes.pop();
            Some(proj)
        } else {
            None
        };

        Ok(AnalyzedInsert {
            table_name: resolved_name,
            table_schema,
            target_columns,
            source: analyzed_source,
            on_conflict: analyzed_on_conflict,
            returning: analyzed_returning,
        })
    }

    /// Analyze ON CONFLICT clause.
    fn analyze_on_conflict(
        &mut self,
        on_insert: &OnInsert,
        table_cols: &[(String, DataType, bool, Option<String>)],
        schema: &crate::types::TableSchema,
        table_scope_name: &str,
        table_name_for_errors: &str,
    ) -> Result<AnalyzedOnConflict, AnalyzerError> {
        match on_insert {
            OnInsert::OnConflict(oc) => match &oc.action {
                ast::OnConflictAction::DoNothing => Ok(AnalyzedOnConflict::DoNothing),
                ast::OnConflictAction::DoUpdate(do_update) => {
                    // Build scope with both target table and "excluded" pseudo-table.
                    let mut scope = Scope::new();
                    scope.add_table(table_scope_name, table_cols);
                    scope.add_table("excluded", table_cols);
                    self.scopes.push(scope);

                    let mut assignments = Vec::new();
                    for assignment in &do_update.assignments {
                        let col_name = normalize_ident(assignment.id.last().ok_or_else(|| {
                            AnalyzerError::Internal("empty assignment target".to_string())
                        })?);
                        let col_idx =
                            self.find_column_index(schema, &col_name, table_name_for_errors)?;
                        assignments.push((
                            col_idx,
                            self.analyze_dml_assignment_expr(&assignment.value, schema, col_idx)?,
                        ));
                    }

                    let where_clause = if let Some(ref sel) = do_update.selection {
                        let analyzed = self.analyze_expr(sel)?;
                        let analyzed = self.ensure_boolean_dml(analyzed)?;
                        Some(analyzed)
                    } else {
                        None
                    };

                    self.scopes.pop();

                    Ok(AnalyzedOnConflict::DoUpdate {
                        assignments,
                        where_clause,
                    })
                }
            },
            OnInsert::DuplicateKeyUpdate(assignments) => {
                // MySQL-style ON DUPLICATE KEY UPDATE — treated like DO UPDATE.
                let mut scope = Scope::new();
                scope.add_table(table_scope_name, table_cols);
                // MySQL uses VALUES(col) to reference the row being inserted.
                // We approximate by adding the same columns under "excluded".
                scope.add_table("excluded", table_cols);
                self.scopes.push(scope);

                let mut analyzed_assignments = Vec::new();
                for assignment in assignments {
                    let col_name = normalize_ident(assignment.id.last().ok_or_else(|| {
                        AnalyzerError::Internal("empty assignment target".to_string())
                    })?);
                    let col_idx =
                        self.find_column_index(schema, &col_name, table_name_for_errors)?;
                    analyzed_assignments.push((
                        col_idx,
                        self.analyze_dml_assignment_expr(&assignment.value, schema, col_idx)?,
                    ));
                }

                self.scopes.pop();

                Ok(AnalyzedOnConflict::DoUpdate {
                    assignments: analyzed_assignments,
                    where_clause: None,
                })
            }
            _ => Err(AnalyzerError::Unsupported(
                "unsupported ON CONFLICT variant".to_string(),
            )),
        }
    }

    // ── UPDATE ──────────────────────────────────────────────

    /// Analyze an UPDATE statement.
    pub fn analyze_update(
        &mut self,
        table: &ast::TableWithJoins,
        assignments: &[ast::Assignment],
        from: &Option<ast::TableWithJoins>,
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<AnalyzedUpdate, AnalyzerError> {
        // Resolve target table.
        let (target_name, target_alias) = match &table.relation {
            ast::TableFactor::Table { name, alias, .. } => {
                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| {
                        split_object_name(name)
                            .map(|(_, n)| n)
                            .unwrap_or_else(|_| name.to_string())
                    });
                (name, alias_str)
            }
            _ => {
                return Err(AnalyzerError::Unsupported(
                    "unsupported UPDATE target".to_string(),
                ))
            }
        };
        let (resolved_name, table_schema, schema) = self.resolve_dml_target(target_name)?;

        // Build scope: target table (+ FROM tables if present).
        let table_cols: Vec<(String, DataType, bool, Option<String>)> = schema
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    c.data_type.clone(),
                    c.nullable,
                    c.collation.clone(),
                )
            })
            .collect();

        let mut scope = Scope::new();
        scope.add_table(&target_alias, &table_cols);

        // Analyze FROM clause if present.
        let analyzed_from = if let Some(from_table) = from {
            self.scopes.push(scope);
            let from_ref = self.analyze_table_with_joins(from_table)?;
            scope = self.scopes.pop().unwrap();
            // Re-push the combined scope.
            self.scopes.push(scope);
            vec![from_ref]
        } else {
            self.scopes.push(scope);
            vec![]
        };

        // Analyze SET assignments.
        let mut analyzed_assignments = Vec::new();
        for assignment in assignments {
            let col_name =
                normalize_ident(assignment.id.last().ok_or_else(|| {
                    AnalyzerError::Internal("empty assignment target".to_string())
                })?);
            let col_idx = self.find_column_index(&schema, &col_name, &resolved_name)?;
            analyzed_assignments.push((
                col_idx,
                self.analyze_dml_assignment_expr(&assignment.value, &schema, col_idx)?,
            ));
        }

        // Analyze WHERE.
        let analyzed_where = if let Some(sel) = selection {
            let analyzed = self.analyze_expr(sel)?;
            let analyzed = self.ensure_boolean_dml(analyzed)?;
            Some(analyzed)
        } else {
            None
        };

        // Analyze RETURNING.
        let analyzed_returning = if let Some(ret_items) = returning {
            let (proj, _) = self.analyze_projection(ret_items, None)?;
            Some(proj)
        } else {
            None
        };

        self.scopes.pop();

        Ok(AnalyzedUpdate {
            table_name: resolved_name,
            table_schema,
            table_alias: target_alias,
            assignments: analyzed_assignments,
            from: analyzed_from,
            where_clause: analyzed_where,
            returning: analyzed_returning,
        })
    }

    // ── DELETE ──────────────────────────────────────────────

    /// Analyze a DELETE statement.
    pub fn analyze_delete(
        &mut self,
        from: &[ast::TableWithJoins],
        using: &[ast::TableWithJoins],
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<AnalyzedDelete, AnalyzerError> {
        // Resolve target table from the first FROM item.
        let (target_name, target_alias) = match &from[0].relation {
            ast::TableFactor::Table { name, alias, .. } => {
                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| {
                        split_object_name(name)
                            .map(|(_, n)| n)
                            .unwrap_or_else(|_| name.to_string())
                    });
                (name, alias_str)
            }
            _ => {
                return Err(AnalyzerError::Unsupported(
                    "unsupported DELETE target".to_string(),
                ))
            }
        };
        let (resolved_name, table_schema, schema) = self.resolve_dml_target(target_name)?;

        // Build scope: target table + USING tables.
        let table_cols: Vec<(String, DataType, bool, Option<String>)> = schema
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    c.data_type.clone(),
                    c.nullable,
                    c.collation.clone(),
                )
            })
            .collect();

        let mut scope = Scope::new();
        scope.add_table(&target_alias, &table_cols);

        // Analyze USING clause if present.
        let analyzed_using = if !using.is_empty() {
            self.scopes.push(scope);
            let mut using_refs = Vec::new();
            for twj in using {
                let table_ref = self.analyze_table_with_joins(twj)?;
                using_refs.push(table_ref);
            }
            scope = self.scopes.pop().unwrap();
            self.scopes.push(scope);
            using_refs
        } else {
            self.scopes.push(scope);
            vec![]
        };

        // Analyze WHERE.
        let analyzed_where = if let Some(sel) = selection {
            let analyzed = self.analyze_expr(sel)?;
            let analyzed = self.ensure_boolean_dml(analyzed)?;
            Some(analyzed)
        } else {
            None
        };

        // Analyze RETURNING.
        let analyzed_returning = if let Some(ret_items) = returning {
            let (proj, _) = self.analyze_projection(ret_items, None)?;
            Some(proj)
        } else {
            None
        };

        self.scopes.pop();

        Ok(AnalyzedDelete {
            table_name: resolved_name,
            table_schema,
            table_alias: target_alias,
            using: analyzed_using,
            where_clause: analyzed_where,
            returning: analyzed_returning,
        })
    }

    // ── Helpers ─────────────────────────────────────────────

    /// Resolve a DML target table via the catalog.
    ///
    /// Returns (resolved_table_name, table_ref_schema, full_table_schema).
    fn resolve_dml_target(
        &self,
        name: &ObjectName,
    ) -> Result<(String, TableRefSchema, crate::types::TableSchema), AnalyzerError> {
        let (schema_opt, obj_name) =
            split_object_name(name).map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
        let (resolved_name, table_schema) = self
            .catalog
            .resolve_table(&obj_name, schema_opt.as_deref())
            .map_err(|e| AnalyzerError::Internal(e.to_string()))?
            .ok_or_else(|| AnalyzerError::TableNotFound(obj_name.clone()))?;

        let ref_schema = TableRefSchema {
            table_id: table_schema.table_id,
            columns: table_schema
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.data_type.clone(), c.nullable))
                .collect(),
        };

        Ok((resolved_name, ref_schema, table_schema))
    }

    /// Find a column index by name in a table schema.
    fn find_column_index(
        &self,
        schema: &crate::types::TableSchema,
        col_name: &str,
        table_name: &str,
    ) -> Result<usize, AnalyzerError> {
        schema
            .columns
            .iter()
            .position(|c| c.name == col_name)
            .ok_or_else(|| AnalyzerError::DmlColumnNotFound {
                column: col_name.to_string(),
                table: table_name.to_string(),
            })
    }

    /// Coerce a value expression to match a target column type.
    ///
    /// Inserts an implicit Assignment cast if the types differ and are compatible.
    fn coerce_assignment(
        &self,
        expr: TypedExpr,
        target_type: &DataType,
        col_name: &str,
    ) -> Result<TypedExpr, AnalyzerError> {
        if expr.data_type == *target_type {
            return Ok(expr);
        }
        if expr.is_null_constant() {
            return Ok(TypedExpr::null(target_type.clone()));
        }
        // Text type is universally coercible in assignment context (PostgreSQL behavior).
        // Other types need compatible cast.
        if is_assignment_compatible(&expr.data_type, target_type) {
            Ok(TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: target_type.clone(),
                    cast_context: CastContext::Assignment,
                },
                target_type.clone(),
            ))
        } else {
            Err(AnalyzerError::AssignmentTypeMismatch {
                column: col_name.to_string(),
                expected: target_type.clone(),
                found: expr.data_type.clone(),
            })
        }
    }

    /// Analyze a DML assignment/value expression for a target column.
    ///
    /// `DEFAULT` is represented explicitly in typed IR and evaluated by executor
    /// against the column default contract.
    fn analyze_dml_assignment_expr(
        &mut self,
        expr: &Expr,
        schema: &crate::types::TableSchema,
        col_idx: usize,
    ) -> Result<TypedExpr, AnalyzerError> {
        let col = &schema.columns[col_idx];
        if is_default_expr(expr) {
            return Ok(TypedExpr::new(
                TypedExprKind::Default,
                col.data_type.clone(),
            ));
        }

        let analyzed = self.analyze_expr(expr)?;
        // Resolve parameters from target column type (always call for conflict detection)
        if let TypedExprKind::Parameter { index } = &analyzed.kind {
            let was_unresolved = self.is_unresolved_param(&analyzed);
            self.resolve_param_type(*index, &col.data_type)?;
            if was_unresolved {
                return Ok(TypedExpr::new(
                    TypedExprKind::Parameter { index: *index },
                    col.data_type.clone(),
                ));
            }
        }
        self.coerce_assignment(analyzed, &col.data_type, &col.name)
    }

    /// Validate that an expression has Boolean type (DML WHERE context).
    fn ensure_boolean_dml(&mut self, expr: TypedExpr) -> Result<TypedExpr, AnalyzerError> {
        if expr.data_type == DataType::Boolean {
            return Ok(expr);
        }

        if expr.is_null_constant() {
            return Ok(TypedExpr::null(DataType::Boolean));
        }
        if let TypedExprKind::Parameter { index } = &expr.kind {
            let was_unresolved = self.is_unresolved_param(&expr);
            self.resolve_param_type(*index, &DataType::Boolean)?;
            if was_unresolved {
                return Ok(TypedExpr::new(
                    TypedExprKind::Parameter { index: *index },
                    DataType::Boolean,
                ));
            }
        }
        if matches!(&expr.kind, TypedExprKind::Constant(Value::Text(_))) {
            return Ok(TypedExpr::new(
                TypedExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: DataType::Boolean,
                    cast_context: CastContext::Implicit,
                },
                DataType::Boolean,
            ));
        }

        Err(AnalyzerError::DmlWhereNotBoolean {
            found: expr.data_type,
        })
    }
}

/// Check if `DEFAULT` is being used as a value expression.
fn is_default_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case("DEFAULT"))
}

/// Check if a type can be implicitly cast to another in assignment context.
///
/// PostgreSQL's assignment coercion is permissive — most types can be assigned
/// to compatible types via implicit cast (e.g. Int32 → Int64, Text → anything).
fn is_assignment_compatible(from: &DataType, to: &DataType) -> bool {
    use crate::sql::types::coercion::common_type;

    // Text is universally assignable (PostgreSQL I/O coercion).
    if matches!(from, DataType::Text) || matches!(to, DataType::Text) {
        return true;
    }
    // If there's a common type, assignment is valid.
    common_type(from, to).is_some()
}
