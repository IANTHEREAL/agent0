//! DML analysis: INSERT, UPDATE, DELETE.
//!
//! Validates column existence, type compatibility, boolean WHERE predicates,
//! and arity constraints at analysis time. Produces typed IR that the executor
//! can evaluate without re-analyzing per row.

use sqlparser::ast::{self, Expr, Ident, ObjectName, OnInsert, Query, SelectItem, SetExpr, Values};

use crate::model::{DataType, Value};
use crate::sql::names::{normalize_ident, split_object_name};
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::is_assignment_compatible;

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
        let (resolved_name, schema) = self.resolve_dml_target(table_name)?;
        // DML name resolution should follow relation-name semantics for qualified refs:
        // `schema.table.col` and `table.col` both target relation `table`.
        let target_scope_name = resolved_name
            .rsplit('.')
            .next()
            .unwrap_or(resolved_name.as_str())
            .to_string();

        // Map explicit column names to indices. For INSERT without a column list,
        // PostgreSQL targets the first N table columns where N is the source arity.
        let explicit_target_columns = if columns.is_empty() {
            None
        } else {
            Some(
                columns
                    .iter()
                    .map(|ident| {
                        let col_name = normalize_ident(ident);
                        self.find_column_index(&schema, &col_name, &resolved_name)
                    })
                    .collect::<Result<Vec<usize>, _>>()?,
            )
        };

        // Build scope for the target table (needed for RETURNING and ON CONFLICT).
        // Include all physical columns so ColumnRef indices align with executor's
        // physical row layout. Dropped columns become hidden placeholders.
        let table_cols: Vec<(String, DataType, Option<String>)> = schema
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone(), c.collation.clone()))
            .collect();

        // Analyze source rows.
        let (target_columns, analyzed_source) = match source {
            None => {
                // INSERT ... DEFAULT VALUES
                (
                    explicit_target_columns.clone().unwrap_or_default(),
                    AnalyzedInsertSource::DefaultValues,
                )
            }
            Some(query) => match &*query.body {
                SetExpr::Values(Values { rows, .. }) => {
                    let target_columns = match &explicit_target_columns {
                        Some(cols) => cols.clone(),
                        None => self.resolve_implicit_insert_target_columns(
                            &schema,
                            rows.first().map(|row| row.len()).unwrap_or(0),
                        )?,
                    };
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
                    (target_columns, AnalyzedInsertSource::Values(analyzed_rows))
                }
                _ => {
                    // INSERT ... SELECT ...
                    let analyzed_query = self.analyze_query(query)?;
                    let target_columns = match &explicit_target_columns {
                        Some(cols) => cols.clone(),
                        None => self.resolve_implicit_insert_target_columns(
                            &schema,
                            analyzed_query.output_schema.len(),
                        )?,
                    };

                    // Validate arity: SELECT output columns must match target columns.
                    if analyzed_query.output_schema.len() != target_columns.len() {
                        return Err(AnalyzerError::InsertColumnCountMismatch {
                            columns: target_columns.len(),
                            values: analyzed_query.output_schema.len(),
                        });
                    }

                    (
                        target_columns,
                        AnalyzedInsertSource::Query(Box::new(analyzed_query)),
                    )
                }
            },
        };
        self.validate_generated_insert_targets(&schema, &target_columns, &analyzed_source)?;

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
            target_columns,
            source: analyzed_source,
            on_conflict: analyzed_on_conflict,
            returning: analyzed_returning,
        })
    }

    /// Resolve ON CONFLICT target to analyzed form.
    fn resolve_conflict_target(
        &self,
        target: &Option<ast::ConflictTarget>,
        schema: &crate::model::TableSchema,
        table_name_for_errors: &str,
    ) -> Result<Option<AnalyzedConflictTarget>, AnalyzerError> {
        match target {
            None => Ok(None),
            Some(ast::ConflictTarget::Columns(idents)) => {
                let col_names: Vec<String> = idents
                    .iter()
                    .map(|ident| {
                        let col_name = normalize_ident(ident);
                        // Validate the column exists.
                        self.find_column_index(schema, &col_name, table_name_for_errors)?;
                        Ok(col_name)
                    })
                    .collect::<Result<Vec<_>, AnalyzerError>>()?;
                Ok(Some(AnalyzedConflictTarget::Columns(col_names)))
            }
            Some(ast::ConflictTarget::OnConstraint(name)) => {
                let (_schema, constraint_name) = split_object_name(name)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                Ok(Some(AnalyzedConflictTarget::Constraint(constraint_name)))
            }
        }
    }

    /// Analyze ON CONFLICT clause.
    fn analyze_on_conflict(
        &mut self,
        on_insert: &OnInsert,
        table_cols: &[(String, DataType, Option<String>)],
        schema: &crate::model::TableSchema,
        table_scope_name: &str,
        table_name_for_errors: &str,
    ) -> Result<AnalyzedOnConflict, AnalyzerError> {
        match on_insert {
            OnInsert::OnConflict(oc) => match &oc.action {
                ast::OnConflictAction::DoNothing => Ok(AnalyzedOnConflict::DoNothing),
                ast::OnConflictAction::DoUpdate(do_update) => {
                    // Resolve conflict target.
                    let target = self.resolve_conflict_target(
                        &oc.conflict_target,
                        schema,
                        table_name_for_errors,
                    )?;

                    // Build scope with both target table and "excluded" pseudo-table.
                    // Use dropped-aware path to preserve physical row alignment.
                    let mut scope = Scope::new();
                    let has_dropped = schema.columns.iter().any(|c| c.is_dropped);
                    if has_dropped {
                        let cols_with_dropped: Vec<(String, DataType, Option<String>, bool)> =
                            schema
                                .columns
                                .iter()
                                .map(|c| {
                                    (
                                        c.name.clone(),
                                        c.data_type.clone(),
                                        c.collation.clone(),
                                        c.is_dropped,
                                    )
                                })
                                .collect();
                        scope.add_table_with_dropped_columns(
                            table_scope_name,
                            &cols_with_dropped,
                            false,
                        );
                        scope.add_table_with_dropped_columns("excluded", &cols_with_dropped, false);
                    } else {
                        scope.add_table(table_scope_name, table_cols);
                        scope.add_table("excluded", table_cols);
                    }
                    self.scopes.push(scope);

                    let mut assignments = Vec::new();
                    for assignment in &do_update.assignments {
                        let col_name = normalize_ident(assignment.id.last().ok_or_else(|| {
                            AnalyzerError::Internal("empty assignment target".to_string())
                        })?);
                        let col_idx =
                            self.find_column_index(schema, &col_name, table_name_for_errors)?;
                        let typed =
                            self.analyze_dml_assignment_expr(&assignment.value, schema, col_idx)?;
                        self.validate_generated_update_target(schema, col_idx, &typed)?;
                        assignments.push((col_idx, typed));
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
                        target,
                        assignments,
                        where_clause,
                    })
                }
            },
            OnInsert::DuplicateKeyUpdate(assignments) => {
                // MySQL-style ON DUPLICATE KEY UPDATE — treated like DO UPDATE.
                let mut scope = Scope::new();
                let has_dropped = schema.columns.iter().any(|c| c.is_dropped);
                if has_dropped {
                    let cols_with_dropped: Vec<(String, DataType, Option<String>, bool)> = schema
                        .columns
                        .iter()
                        .map(|c| {
                            (
                                c.name.clone(),
                                c.data_type.clone(),
                                c.collation.clone(),
                                c.is_dropped,
                            )
                        })
                        .collect();
                    scope.add_table_with_dropped_columns(
                        table_scope_name,
                        &cols_with_dropped,
                        false,
                    );
                    scope.add_table_with_dropped_columns("excluded", &cols_with_dropped, false);
                } else {
                    scope.add_table(table_scope_name, table_cols);
                    scope.add_table("excluded", table_cols);
                }
                self.scopes.push(scope);

                let mut analyzed_assignments = Vec::new();
                for assignment in assignments {
                    let col_name = normalize_ident(assignment.id.last().ok_or_else(|| {
                        AnalyzerError::Internal("empty assignment target".to_string())
                    })?);
                    let col_idx =
                        self.find_column_index(schema, &col_name, table_name_for_errors)?;
                    let typed =
                        self.analyze_dml_assignment_expr(&assignment.value, schema, col_idx)?;
                    self.validate_generated_update_target(schema, col_idx, &typed)?;
                    analyzed_assignments.push((col_idx, typed));
                }

                self.scopes.pop();

                Ok(AnalyzedOnConflict::DoUpdate {
                    target: None,
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
        let (resolved_name, schema) = self.resolve_dml_target(target_name)?;

        // Build scope: target table (+ FROM tables if present).
        // Include all physical columns so ColumnRef indices align with physical rows.
        let table_cols: Vec<(String, DataType, Option<String>)> = schema
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone(), c.collation.clone()))
            .collect();

        let has_dropped = schema.columns.iter().any(|c| c.is_dropped);
        let mut scope = Scope::new();
        scope.set_add_system_columns(true);
        if has_dropped {
            let cols_with_dropped: Vec<(String, DataType, Option<String>, bool)> = schema
                .columns
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        c.data_type.clone(),
                        c.collation.clone(),
                        c.is_dropped,
                    )
                })
                .collect();
            scope.add_table_with_dropped_columns(&target_alias, &cols_with_dropped, true);
        } else {
            scope.add_table(&target_alias, &table_cols);
        }

        // Analyze FROM clause if present.
        let analyzed_from = if let Some(from_table) = from {
            self.scopes.push(scope);
            let from_ref = self.analyze_table_with_joins(from_table)?;
            scope = self
                .scopes
                .pop()
                .ok_or_else(|| AnalyzerError::Internal("scope stack underflow".into()))?;
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
            let typed = self.analyze_dml_assignment_expr(&assignment.value, &schema, col_idx)?;
            self.validate_generated_update_target(&schema, col_idx, &typed)?;
            analyzed_assignments.push((col_idx, typed));
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
        let (resolved_name, schema) = self.resolve_dml_target(target_name)?;

        // Build scope: target table + USING tables.
        // Include all physical columns so ColumnRef indices align with physical rows.
        let table_cols: Vec<(String, DataType, Option<String>)> = schema
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone(), c.collation.clone()))
            .collect();

        let has_dropped = schema.columns.iter().any(|c| c.is_dropped);
        let mut scope = Scope::new();
        scope.set_add_system_columns(true);
        if has_dropped {
            let cols_with_dropped: Vec<(String, DataType, Option<String>, bool)> = schema
                .columns
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        c.data_type.clone(),
                        c.collation.clone(),
                        c.is_dropped,
                    )
                })
                .collect();
            scope.add_table_with_dropped_columns(&target_alias, &cols_with_dropped, true);
        } else {
            scope.add_table(&target_alias, &table_cols);
        }

        // Analyze USING clause if present.
        let analyzed_using = if !using.is_empty() {
            self.scopes.push(scope);
            let mut using_refs = Vec::new();
            for twj in using {
                let table_ref = self.analyze_table_with_joins(twj)?;
                using_refs.push(table_ref);
            }
            scope = self
                .scopes
                .pop()
                .ok_or_else(|| AnalyzerError::Internal("scope stack underflow".into()))?;
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
            using: analyzed_using,
            where_clause: analyzed_where,
            returning: analyzed_returning,
        })
    }

    // ── Helpers ─────────────────────────────────────────────

    /// Resolve a DML target table via the catalog.
    ///
    /// Returns (resolved_table_name, full_table_schema).
    fn resolve_dml_target(
        &self,
        name: &ObjectName,
    ) -> Result<(String, crate::model::TableSchema), AnalyzerError> {
        let (schema_opt, obj_name) =
            split_object_name(name).map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
        let (resolved_name, table_schema) = self
            .catalog
            .resolve_table(&obj_name, schema_opt.as_deref())
            .map_err(|e| AnalyzerError::Internal(e.to_string()))?
            .ok_or_else(|| AnalyzerError::TableNotFound(obj_name.clone()))?;

        Ok((resolved_name, table_schema))
    }

    /// Find a column index by name in a table schema.
    /// Skips logically dropped columns (PostgreSQL `attisdropped`).
    fn find_column_index(
        &self,
        schema: &crate::model::TableSchema,
        col_name: &str,
        table_name: &str,
    ) -> Result<usize, AnalyzerError> {
        schema
            .columns
            .iter()
            .position(|c| !c.is_dropped && c.name == col_name)
            .ok_or_else(|| AnalyzerError::DmlColumnNotFound {
                column: col_name.to_string(),
                table: table_name.to_string(),
            })
    }

    fn validate_generated_insert_targets(
        &self,
        schema: &crate::model::TableSchema,
        target_columns: &[usize],
        source: &AnalyzedInsertSource,
    ) -> Result<(), AnalyzerError> {
        match source {
            AnalyzedInsertSource::DefaultValues => Ok(()),
            AnalyzedInsertSource::Values(rows) => {
                for row in rows {
                    for (&col_idx, expr) in target_columns.iter().zip(row.iter()) {
                        self.validate_generated_insert_target(schema, col_idx, expr)?;
                    }
                }
                Ok(())
            }
            AnalyzedInsertSource::Query(_) => {
                for &col_idx in target_columns {
                    let Some(col) = schema.columns.get(col_idx) else {
                        continue;
                    };
                    if col.generation_expr.is_some() {
                        return Err(AnalyzerError::SqlStructure(format!(
                            "cannot insert a non-DEFAULT value into column \"{}\"\nDETAIL:  Column \"{}\" is a generated column.",
                            col.name, col.name
                        )));
                    }
                }
                Ok(())
            }
        }
    }

    fn validate_generated_insert_target(
        &self,
        schema: &crate::model::TableSchema,
        col_idx: usize,
        expr: &TypedExpr,
    ) -> Result<(), AnalyzerError> {
        let Some(col) = schema.columns.get(col_idx) else {
            return Ok(());
        };
        if col.generation_expr.is_some() && !matches!(expr.kind, TypedExprKind::Default) {
            return Err(AnalyzerError::SqlStructure(format!(
                "cannot insert a non-DEFAULT value into column \"{}\"\nDETAIL:  Column \"{}\" is a generated column.",
                col.name, col.name
            )));
        }
        Ok(())
    }

    fn resolve_implicit_insert_target_columns(
        &self,
        schema: &crate::model::TableSchema,
        source_arity: usize,
    ) -> Result<Vec<usize>, AnalyzerError> {
        // Skip logically dropped columns — implicit INSERT targets only
        // visible columns (matching PostgreSQL attisdropped semantics).
        let visible_indices: Vec<usize> = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_dropped)
            .map(|(i, _)| i)
            .collect();
        if source_arity > visible_indices.len() {
            return Err(AnalyzerError::InsertColumnCountMismatch {
                columns: visible_indices.len(),
                values: source_arity,
            });
        }
        Ok(visible_indices[..source_arity].to_vec())
    }

    fn validate_generated_update_target(
        &self,
        schema: &crate::model::TableSchema,
        col_idx: usize,
        expr: &TypedExpr,
    ) -> Result<(), AnalyzerError> {
        let Some(col) = schema.columns.get(col_idx) else {
            return Ok(());
        };
        if col.generation_expr.is_some() && !matches!(expr.kind, TypedExprKind::Default) {
            return Err(AnalyzerError::SqlStructure(format!(
                "column \"{}\" can only be updated to DEFAULT\nDETAIL:  Column \"{}\" is a generated column.",
                col.name, col.name
            )));
        }
        Ok(())
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
        schema: &crate::model::TableSchema,
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
