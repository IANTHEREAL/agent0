//! FROM clause analysis: table references, joins, subqueries, table functions.
//!
//! Handles `analyze_from`, `analyze_table_with_joins`, `analyze_table_factor`,
//! and join constraint/condition resolution.

use sqlparser::ast::{self as ast, TableFactor, TableWithJoins};

use crate::model::DataType;
use crate::sql::names::{normalize_ident, split_object_name};
use crate::sql::table_functions::table_function_key;
use crate::sql::types::coercion::common_type;

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    fn analyze_table_function_args(
        &mut self,
        func_args: &[ast::FunctionArg],
    ) -> Result<Vec<TypedFunctionArg>, AnalyzerError> {
        self.validate_no_positional_after_named(func_args)?;
        let mut typed_args = Vec::with_capacity(func_args.len());
        for arg in func_args {
            match arg {
                ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => {
                    typed_args.push(TypedFunctionArg::Positional(self.analyze_expr(e)?));
                }
                ast::FunctionArg::Named {
                    name,
                    arg: ast::FunctionArgExpr::Expr(e),
                    ..
                } => {
                    typed_args.push(TypedFunctionArg::Named {
                        name: crate::sql::names::normalize_ident(name),
                        expr: self.analyze_expr(e)?,
                    });
                }
                _ => {
                    return Err(AnalyzerError::Unsupported(
                        "unsupported table function argument".to_string(),
                    ));
                }
            }
        }
        Ok(typed_args)
    }

    fn analyze_named_table_function(
        &mut self,
        name: &ast::ObjectName,
        alias: Option<&ast::TableAlias>,
        func_args: &[ast::FunctionArg],
    ) -> Result<AnalyzedTableRef, AnalyzerError> {
        let (schema_opt, obj_name) =
            split_object_name(name).map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
        let alias_str = alias
            .as_ref()
            .map(|a| normalize_ident(&a.name))
            .unwrap_or_else(|| obj_name.clone());
        let dispatch_name = if obj_name.eq_ignore_ascii_case("generate_series")
            || obj_name.eq_ignore_ascii_case("unnest")
            || obj_name.eq_ignore_ascii_case("_db9_sys_record_migration")
            || obj_name.eq_ignore_ascii_case("current_schema")
            || obj_name.eq_ignore_ascii_case("current_database")
            || obj_name.eq_ignore_ascii_case("current_user")
            || obj_name.eq_ignore_ascii_case("session_user")
            || obj_name.eq_ignore_ascii_case("user")
            || obj_name.eq_ignore_ascii_case("jsonb_object_keys")
            || obj_name.eq_ignore_ascii_case("json_object_keys")
            || obj_name.eq_ignore_ascii_case("jsonb_array_elements")
            || obj_name.eq_ignore_ascii_case("json_array_elements")
            || obj_name.eq_ignore_ascii_case("jsonb_array_elements_text")
            || obj_name.eq_ignore_ascii_case("json_array_elements_text")
            || obj_name.eq_ignore_ascii_case("jsonb_each")
            || obj_name.eq_ignore_ascii_case("json_each")
            || obj_name.eq_ignore_ascii_case("jsonb_each_text")
            || obj_name.eq_ignore_ascii_case("json_each_text")
            || obj_name.eq_ignore_ascii_case("chunk_text")
        {
            // Built-in table/scalar-in-FROM functions should keep their canonical
            // dispatch name even when schema-qualified in SQL (e.g. pg_catalog.unnest).
            obj_name.clone()
        } else {
            match &schema_opt {
                Some(schema) => format!("{}.{}", schema, obj_name),
                None => obj_name.clone(),
            }
        };

        let typed_args = self.analyze_table_function_args(func_args)?;

        let key = table_function_key(name, func_args);
        let mut output_cols: Vec<(String, DataType, bool, Option<String>)> = if let Some(schema) =
            self.catalog.resolve_table_function(&key)
        {
            schema
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
                .collect()
        } else if obj_name.eq_ignore_ascii_case("generate_series") {
            // generate_series(start, stop [, step]) returns a single column.
            let positional: Vec<&TypedExpr> = typed_args
                .iter()
                .filter_map(|a| match a {
                    TypedFunctionArg::Positional(e) => Some(e),
                    _ => None,
                })
                .collect();
            if positional.len() < 2 {
                return Err(AnalyzerError::Unsupported(
                    "generate_series requires at least 2 arguments".to_string(),
                ));
            }

            let start_ty = &positional[0].data_type;
            let stop_ty = &positional[1].data_type;
            let out_ty = if matches!(start_ty, DataType::Date) && matches!(stop_ty, DataType::Date)
            {
                DataType::TimestampTz
            } else if matches!(start_ty, DataType::Timestamp)
                && matches!(stop_ty, DataType::Timestamp)
            {
                DataType::Timestamp
            } else if matches!(start_ty, DataType::Int32) && matches!(stop_ty, DataType::Int32) {
                DataType::Int32
            } else if matches!(start_ty, DataType::Int64) && matches!(stop_ty, DataType::Int64) {
                DataType::Int64
            } else if matches!(start_ty, DataType::Float64) && matches!(stop_ty, DataType::Float64)
            {
                DataType::Float64
            } else if matches!(start_ty, DataType::Numeric { .. })
                && matches!(stop_ty, DataType::Numeric { .. })
            {
                start_ty.clone()
            } else if let Some(common) = common_type(start_ty, stop_ty) {
                common
            } else {
                DataType::Text
            };

            // Column name semantics:
            // - `FROM generate_series(...) AS n` -> column name "n"
            // - `FROM generate_series(...) AS t(col)` -> column name "col"
            let col_name = if let Some(ta) = alias {
                if !ta.columns.is_empty() {
                    crate::sql::names::normalize_ident(&ta.columns[0])
                } else {
                    crate::sql::names::normalize_ident(&ta.name)
                }
            } else {
                "generate_series".to_string()
            };

            vec![(col_name, out_ty, false, None)]
        } else if obj_name.eq_ignore_ascii_case("unnest") {
            // unnest(array) as table-valued function (e.g. FROM pg_catalog.unnest(...))
            let positional: Vec<&TypedExpr> = typed_args
                .iter()
                .filter_map(|a| match a {
                    TypedFunctionArg::Positional(e) => Some(e),
                    _ => None,
                })
                .collect();
            if positional.is_empty() {
                return Err(AnalyzerError::Unsupported(
                    "unnest requires at least 1 argument".to_string(),
                ));
            }
            let elem_type = match &positional[0].data_type {
                DataType::Array(inner) => inner.as_ref().clone(),
                _ => DataType::Text,
            };
            let col_name = alias
                .as_ref()
                .map(|ta| {
                    if !ta.columns.is_empty() {
                        crate::sql::names::normalize_ident(&ta.columns[0])
                    } else {
                        crate::sql::names::normalize_ident(&ta.name)
                    }
                })
                .unwrap_or_else(|| "unnest".to_string());
            vec![(col_name, elem_type, true, None)]
        } else if obj_name.eq_ignore_ascii_case("current_schema")
            || obj_name.eq_ignore_ascii_case("current_database")
            || obj_name.eq_ignore_ascii_case("current_user")
            || obj_name.eq_ignore_ascii_case("session_user")
            || obj_name.eq_ignore_ascii_case("user")
        {
            // Scalar functions used in FROM return a single-row, single-column relation.
            let col_name = obj_name.to_lowercase();
            vec![(col_name, DataType::Text, false, None)]
        } else if obj_name.eq_ignore_ascii_case("jsonb_object_keys")
            || obj_name.eq_ignore_ascii_case("json_object_keys")
        {
            let col_name = if let Some(ta) = alias {
                if !ta.columns.is_empty() {
                    crate::sql::names::normalize_ident(&ta.columns[0])
                } else {
                    crate::sql::names::normalize_ident(&ta.name)
                }
            } else {
                obj_name.to_lowercase()
            };
            vec![(col_name, DataType::Text, false, None)]
        } else if obj_name.eq_ignore_ascii_case("jsonb_array_elements")
            || obj_name.eq_ignore_ascii_case("json_array_elements")
        {
            let out_ty = if obj_name.eq_ignore_ascii_case("json_array_elements") {
                DataType::Json
            } else {
                DataType::Jsonb
            };
            vec![("value".to_string(), out_ty, false, None)]
        } else if obj_name.eq_ignore_ascii_case("jsonb_array_elements_text")
            || obj_name.eq_ignore_ascii_case("json_array_elements_text")
        {
            vec![("value".to_string(), DataType::Text, true, None)]
        } else if obj_name.eq_ignore_ascii_case("jsonb_each")
            || obj_name.eq_ignore_ascii_case("json_each")
        {
            let val_ty = if obj_name.eq_ignore_ascii_case("json_each") {
                DataType::Json
            } else {
                DataType::Jsonb
            };
            vec![
                ("key".to_string(), DataType::Text, false, None),
                ("value".to_string(), val_ty, false, None),
            ]
        } else if obj_name.eq_ignore_ascii_case("jsonb_each_text")
            || obj_name.eq_ignore_ascii_case("json_each_text")
        {
            vec![
                ("key".to_string(), DataType::Text, false, None),
                ("value".to_string(), DataType::Text, true, None),
            ]
        } else if obj_name.eq_ignore_ascii_case("chunk_text") {
            // Require at least 1 arg (positional or named "content")
            let has_content = typed_args
                .iter()
                .any(|a| matches!(a, TypedFunctionArg::Positional(_)))
                || typed_args.iter().any(
                    |a| matches!(a, TypedFunctionArg::Named { name, .. } if name == "content"),
                );
            if !has_content {
                return Err(AnalyzerError::Unsupported(
                    "chunk_text requires at least 1 argument (content TEXT)".to_string(),
                ));
            }
            vec![
                ("chunk_index".to_string(), DataType::Int32, false, None),
                ("chunk_text".to_string(), DataType::Text, false, None),
                ("chunk_pos".to_string(), DataType::Int32, false, None),
            ]
        } else if obj_name.eq_ignore_ascii_case("_db9_sys_record_migration") {
            vec![
                ("name".to_string(), DataType::Text, false, None),
                ("applied_at".to_string(), DataType::Text, false, None),
                ("status".to_string(), DataType::Text, false, None),
            ]
        } else {
            return Err(AnalyzerError::Unsupported(format!(
                "unsupported table-valued function: {}",
                obj_name
            )));
        };

        // Apply alias column list (renames output columns).
        if let Some(ta) = alias {
            if !ta.columns.is_empty() {
                if ta.columns.len() != output_cols.len() {
                    return Err(AnalyzerError::Unsupported(format!(
                        "table function alias column count mismatch: expected {}, got {}",
                        output_cols.len(),
                        ta.columns.len()
                    )));
                }
                for (i, ident) in ta.columns.iter().enumerate() {
                    output_cols[i].0 = crate::sql::names::normalize_ident(ident);
                }
            }
        }

        let col_start = self.scopes.current().column_count();
        self.scopes
            .current_mut()
            .add_table_without_system_columns(&alias_str, &output_cols);
        let col_end = self.scopes.current().column_count();
        self.scopes
            .current_mut()
            .set_table_source_relation(&alias_str, "", col_start..col_end);
        if output_cols.len() == 1 {
            self.scopes
                .current_mut()
                .register_single_column_function_alias(&alias_str, col_start);
        }

        let output_columns: Vec<(String, DataType)> = output_cols
            .iter()
            .map(|(n, dt, _, _)| (n.clone(), dt.clone()))
            .collect();

        let func = ResolvedFunction {
            name: dispatch_name,
            kind: FunctionKind::Builtin,
            return_type: DataType::Text,
        };

        Ok(AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Function {
                func,
                args: typed_args,
                output_columns,
            },
            alias: Some(alias_str),
        })
    }

    pub(super) fn analyze_from(
        &mut self,
        from: &[TableWithJoins],
    ) -> Result<Vec<AnalyzedTableRef>, AnalyzerError> {
        let mut refs = Vec::new();
        for twj in from {
            let table_ref = self.analyze_table_with_joins(twj)?;
            refs.push(table_ref);
        }
        Ok(refs)
    }

    pub(in crate::sql::analyzer) fn analyze_table_with_joins(
        &mut self,
        twj: &TableWithJoins,
    ) -> Result<AnalyzedTableRef, AnalyzerError> {
        // Record where this table group's columns start (excludes preceding comma-FROM items).
        let left_start = self.scopes.current().column_count();
        let mut result = self.analyze_table_factor(&twj.relation)?;

        // Process JOINs left-to-right, each one wraps the accumulated result
        for join in &twj.joins {
            // Record boundary before right table is added to scope.
            // Columns at indices left_start..right_start belong to left, >= right_start to right.
            let right_start = self.scopes.current().column_count();
            let right = self.analyze_table_factor(&join.relation)?;
            let (join_type, condition) =
                self.analyze_join_constraint(&join.join_operator, left_start, right_start)?;

            // USING join: hide right-side duplicate columns from SELECT * expansion.
            // right_index is LOCAL to the right operator; convert to GLOBAL scope index for hiding.
            if let JoinCondition::Using(ref cols) = condition {
                for uc in cols {
                    self.scopes.current_mut().register_using_column(
                        &uc.name,
                        left_start + uc.left_index,
                        right_start + uc.right_index,
                        uc.data_type.clone(),
                        uc.left_type.clone(),
                        uc.right_type.clone(),
                    );
                }
            }

            result = AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Join {
                    left: Box::new(result),
                    right: Box::new(right),
                    join_type,
                    condition,
                    left_col_start: left_start,
                },
                alias: None,
            };
        }

        Ok(result)
    }

    fn analyze_table_factor(
        &mut self,
        factor: &TableFactor,
    ) -> Result<AnalyzedTableRef, AnalyzerError> {
        match factor {
            TableFactor::Table {
                name,
                alias,
                args: Some(func_args),
                ..
            } => self.analyze_named_table_function(name, alias.as_ref(), func_args),

            TableFactor::Function {
                name, args, alias, ..
            } => self.analyze_named_table_function(name, alias.as_ref(), args),

            TableFactor::TableFunction { expr, alias } => match expr {
                ast::Expr::Function(func) => {
                    self.analyze_named_table_function(&func.name, alias.as_ref(), &func.args)
                }
                _ => Err(AnalyzerError::Unsupported(
                    "TABLE(expr) requires a function call".to_string(),
                )),
            },

            TableFactor::Table {
                name,
                alias,
                args: None,
                ..
            } => {
                // PostgreSQL SQL value functions have special syntax and can appear in FROM
                // without trailing parentheses (e.g. `FROM CURRENT_USER`).
                //
                // Treat these as scalar table functions (single-row, single-column relation).
                // Quoted identifiers should remain resolvable as real tables.
                if name.0.len() == 1 {
                    let ident = &name.0[0];
                    if ident.quote_style.is_none()
                        && (ident.value.eq_ignore_ascii_case("current_user")
                            || ident.value.eq_ignore_ascii_case("session_user")
                            || ident.value.eq_ignore_ascii_case("user")
                            || ident.value.eq_ignore_ascii_case("current_schema"))
                    {
                        let obj_name = ident.value.to_lowercase();
                        let alias_str = alias
                            .as_ref()
                            .map(|a| normalize_ident(&a.name))
                            .unwrap_or_else(|| obj_name.clone());

                        let mut output_cols: Vec<(String, DataType, bool, Option<String>)> =
                            vec![(obj_name.clone(), DataType::Text, false, None)];

                        // Apply alias column list (renames output columns).
                        if let Some(ta) = alias {
                            if !ta.columns.is_empty() {
                                if ta.columns.len() != output_cols.len() {
                                    return Err(AnalyzerError::Unsupported(format!(
                                        "table function alias column count mismatch: expected {}, got {}",
                                        output_cols.len(),
                                        ta.columns.len()
                                    )));
                                }
                                for (i, ident) in ta.columns.iter().enumerate() {
                                    output_cols[i].0 = crate::sql::names::normalize_ident(ident);
                                }
                            }
                        }

                        let col_start = self.scopes.current().column_count();
                        self.scopes
                            .current_mut()
                            .add_table_without_system_columns(&alias_str, &output_cols);
                        let col_end = self.scopes.current().column_count();
                        self.scopes.current_mut().set_table_source_relation(
                            &alias_str,
                            "",
                            col_start..col_end,
                        );
                        if output_cols.len() == 1 {
                            self.scopes
                                .current_mut()
                                .register_single_column_function_alias(&alias_str, col_start);
                        }

                        let output_columns: Vec<(String, DataType)> = output_cols
                            .iter()
                            .map(|(n, dt, _, _)| (n.clone(), dt.clone()))
                            .collect();

                        let func = ResolvedFunction {
                            name: obj_name.to_ascii_uppercase(),
                            kind: FunctionKind::Builtin,
                            return_type: DataType::Text,
                        };

                        return Ok(AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Function {
                                func,
                                args: vec![],
                                output_columns,
                            },
                            alias: Some(alias_str),
                        });
                    }
                }

                // Split ObjectName into (optional schema, object name).
                let (schema_opt, obj_name) = split_object_name(name)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| obj_name.clone());

                // Check CTE first (CTEs are always unqualified in PostgreSQL).
                if schema_opt.is_none() {
                    if let Some(cte_cols) = self.scopes.resolve_cte(&obj_name) {
                        let columns_for_scope: Vec<(String, DataType, bool, Option<String>)> =
                            cte_cols
                                .iter()
                                .map(|(n, dt, coll)| (n.clone(), dt.clone(), true, coll.clone()))
                                .collect();

                        let col_start = self.scopes.current().column_count();
                        self.scopes
                            .current_mut()
                            .add_table_without_system_columns(&alias_str, &columns_for_scope);
                        let col_end = self.scopes.current().column_count();
                        self.scopes.current_mut().set_table_source_relation(
                            &alias_str,
                            "",
                            col_start..col_end,
                        );

                        let columns_for_schema: Vec<(String, DataType, bool)> = cte_cols
                            .iter()
                            .map(|(n, dt, _coll)| (n.clone(), dt.clone(), true))
                            .collect();

                        let schema = TableRefSchema {
                            table_id: 0,
                            columns: columns_for_schema,
                        };

                        return Ok(AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Table {
                                name: obj_name,
                                schema,
                            },
                            alias: Some(alias_str),
                        });
                    }
                }

                // Try catalog with proper schema qualification.
                match self.catalog.resolve_table(&obj_name, schema_opt.as_deref()) {
                    Ok(Some((qualified_name, table_schema))) => {
                        let has_dropped = table_schema.columns.iter().any(|c| c.is_dropped);

                        let col_start = self.scopes.current().column_count();
                        if has_dropped {
                            // All physical columns (including dropped) occupy scope
                            // slots so join-row indexing stays correct. Dropped slots
                            // are marked hidden and excluded from name resolution.
                            let columns_for_scope: Vec<(
                                String,
                                DataType,
                                bool,
                                Option<String>,
                                bool,
                            )> = table_schema
                                .columns
                                .iter()
                                .map(|c| {
                                    (
                                        c.name.clone(),
                                        c.data_type.clone(),
                                        c.nullable,
                                        c.collation.clone(),
                                        c.is_dropped,
                                    )
                                })
                                .collect();
                            let include_sys = self.scopes.current().system_columns_enabled();
                            self.scopes.current_mut().add_table_with_dropped_columns(
                                &alias_str,
                                &columns_for_scope,
                                include_sys,
                            );
                        } else {
                            let columns_for_scope: Vec<(String, DataType, bool, Option<String>)> =
                                table_schema
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
                            self.scopes
                                .current_mut()
                                .add_table(&alias_str, &columns_for_scope);
                        }
                        let col_end = self.scopes.current().column_count();
                        let relation_binding =
                            if qualified_name.eq_ignore_ascii_case("pg_namespace") {
                                "pg_catalog.pg_namespace"
                            } else {
                                qualified_name.as_str()
                            };
                        self.scopes.current_mut().set_table_source_relation(
                            &alias_str,
                            relation_binding,
                            col_start..col_end,
                        );

                        // Record the source schema so schema-qualified wildcards
                        // (e.g. `schema.table.*`) can be validated.
                        // Only record when the table is NOT aliased — PostgreSQL
                        // rejects `schema.alias.*` and requires bare alias usage.
                        if alias.is_none() {
                            if let Some(dot_pos) = qualified_name.find('.') {
                                let resolved_schema = &qualified_name[..dot_pos];
                                self.scopes.current_mut().set_table_source_schema(
                                    &alias_str,
                                    resolved_schema,
                                    col_start..col_end,
                                );
                            }
                        }

                        // Include all physical columns in TableRefSchema so
                        // wildcard offset computation aligns with scope slots.
                        // Dropped columns are filtered in the wildcard plan consumer.
                        let columns_for_schema: Vec<(String, DataType, bool)> = table_schema
                            .columns
                            .iter()
                            .map(|c| (c.name.clone(), c.data_type.clone(), c.nullable))
                            .collect();

                        let schema = TableRefSchema {
                            table_id: table_schema.table_id,
                            columns: columns_for_schema,
                        };

                        Ok(AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Table {
                                name: qualified_name,
                                schema,
                            },
                            alias: Some(alias_str),
                        })
                    }
                    Ok(None) => Err(AnalyzerError::TableNotFound(obj_name)),
                    Err(e) => Err(AnalyzerError::Internal(e.to_string())),
                }
            }

            TableFactor::UNNEST {
                array_exprs,
                alias,
                with_offset,
                with_offset_alias,
            } => {
                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| "unnest".to_string());

                let mut typed_args = Vec::with_capacity(array_exprs.len());
                let mut output_cols: Vec<(String, DataType, bool, Option<String>)> =
                    Vec::with_capacity(array_exprs.len() + usize::from(*with_offset));

                for (i, expr) in array_exprs.iter().enumerate() {
                    let analyzed = self.analyze_expr(expr)?;
                    let elem_type = match &analyzed.data_type {
                        DataType::Array(inner) => inner.as_ref().clone(),
                        _ => {
                            return Err(AnalyzerError::FunctionNotFound {
                                name: "unnest".to_string(),
                                arg_types: vec![analyzed.data_type.clone()],
                            });
                        }
                    };

                    let col_name = if array_exprs.len() <= 1 || i == 0 {
                        "unnest".to_string()
                    } else {
                        format!("unnest_{}", i + 1)
                    };
                    output_cols.push((col_name, elem_type, true, None));
                    typed_args.push(TypedFunctionArg::Positional(analyzed));
                }

                if *with_offset {
                    let ord_name = with_offset_alias
                        .as_ref()
                        .map(crate::sql::names::normalize_ident)
                        .unwrap_or_else(|| "ordinality".to_string());
                    output_cols.push((ord_name, DataType::Int64, false, None));
                }

                // Apply alias column list (renames output columns).
                if let Some(ta) = alias {
                    if !ta.columns.is_empty() {
                        if ta.columns.len() != output_cols.len() {
                            return Err(AnalyzerError::Unsupported(format!(
                                "table function alias column count mismatch: expected {}, got {}",
                                output_cols.len(),
                                ta.columns.len()
                            )));
                        }
                        for (i, ident) in ta.columns.iter().enumerate() {
                            output_cols[i].0 = crate::sql::names::normalize_ident(ident);
                        }
                    }
                }

                let col_start = self.scopes.current().column_count();
                self.scopes
                    .current_mut()
                    .add_table_without_system_columns(&alias_str, &output_cols);
                let col_end = self.scopes.current().column_count();
                self.scopes.current_mut().set_table_source_relation(
                    &alias_str,
                    "",
                    col_start..col_end,
                );
                if output_cols.len() == 1 {
                    self.scopes
                        .current_mut()
                        .register_single_column_function_alias(&alias_str, col_start);
                }

                let output_columns: Vec<(String, DataType)> = output_cols
                    .iter()
                    .map(|(n, dt, _, _)| (n.clone(), dt.clone()))
                    .collect();

                let func = ResolvedFunction {
                    name: "UNNEST".to_string(),
                    kind: FunctionKind::Builtin,
                    // Placeholder for table-function refs; caller consumes output_columns
                    // for the real per-column output types.
                    return_type: DataType::Text,
                };

                Ok(AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Function {
                        func,
                        args: typed_args,
                        output_columns,
                    },
                    alias: Some(alias_str),
                })
            }

            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let analyzed = self.analyze_query(subquery)?;

                let alias_str = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| "subquery".to_string());

                // Add subquery output columns to current scope (preserving collation names)
                let coll_names = super::extract_output_collation_names(&analyzed);
                let mut columns: Vec<(String, DataType, bool, Option<String>)> = analyzed
                    .output_schema
                    .iter()
                    .zip(coll_names)
                    .map(|((name, dt, _coll), cn)| (name.clone(), dt.clone(), true, cn))
                    .collect();

                if let Some(a) = alias {
                    if !a.columns.is_empty() {
                        if a.columns.len() != columns.len() {
                            return Err(AnalyzerError::Unsupported(format!(
                                "derived table alias column count mismatch: expected {}, got {}",
                                columns.len(),
                                a.columns.len()
                            )));
                        }
                        for (i, ident) in a.columns.iter().enumerate() {
                            columns[i].0 = normalize_ident(ident);
                        }
                    }
                }
                let col_start = self.scopes.current().column_count();
                self.scopes
                    .current_mut()
                    .add_table_without_system_columns(&alias_str, &columns);
                let col_end = self.scopes.current().column_count();
                self.scopes.current_mut().set_table_source_relation(
                    &alias_str,
                    "",
                    col_start..col_end,
                );

                Ok(AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(analyzed)),
                    alias: Some(alias_str),
                })
            }

            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                let mut result = self.analyze_table_with_joins(table_with_joins)?;
                if let Some(a) = alias {
                    result.alias = Some(normalize_ident(&a.name));
                }
                Ok(result)
            }

            other => Err(AnalyzerError::Unsupported(format!(
                "table factor: {:?}",
                std::mem::discriminant(other),
            ))),
        }
    }

    // -- JOIN constraint --

    pub(super) fn analyze_join_constraint(
        &mut self,
        join_op: &ast::JoinOperator,
        left_start: usize,
        right_start: usize,
    ) -> Result<(JoinType, JoinCondition), AnalyzerError> {
        match join_op {
            ast::JoinOperator::Inner(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Inner, cond))
            }
            ast::JoinOperator::LeftOuter(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Left, cond))
            }
            ast::JoinOperator::RightOuter(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Right, cond))
            }
            ast::JoinOperator::FullOuter(constraint) => {
                let cond = self.analyze_join_condition(constraint, left_start, right_start)?;
                Ok((JoinType::Full, cond))
            }
            ast::JoinOperator::CrossJoin => Ok((JoinType::Cross, JoinCondition::None)),
            other => Err(AnalyzerError::Unsupported(format!(
                "join type: {:?}",
                std::mem::discriminant(other),
            ))),
        }
    }

    fn analyze_join_condition(
        &mut self,
        constraint: &ast::JoinConstraint,
        left_start: usize,
        right_start: usize,
    ) -> Result<JoinCondition, AnalyzerError> {
        match constraint {
            ast::JoinConstraint::On(expr) => {
                let analyzed = self.analyze_expr(expr)?;
                let analyzed = self.ensure_boolean(analyzed, "JOIN ON clause")?;
                Ok(JoinCondition::On(analyzed))
            }
            ast::JoinConstraint::Using(columns) => {
                let mut resolved = Vec::new();
                for col_ident in columns {
                    let col_name = &col_ident.value;
                    let scope = self.scopes.current();
                    let lower = col_name.to_lowercase();

                    // Find matching column in left side (indices in left_start..right_start).
                    let left_match = scope.columns().iter().find(|c| {
                        c.column_index >= left_start
                            && c.column_index < right_start
                            && c.column_name.to_lowercase() == lower
                    });

                    // Find matching column in right side (indices >= right_start).
                    let right_match = scope.columns().iter().find(|c| {
                        c.column_index >= right_start && c.column_name.to_lowercase() == lower
                    });

                    let (left_col, right_col) = match (left_match, right_match) {
                        (Some(l), Some(r)) => (l, r),
                        _ => {
                            return Err(AnalyzerError::ColumnNotFound {
                                name: col_name.clone(),
                                available: scope.available_columns(),
                            });
                        }
                    };

                    // Unify left and right column types (e.g., INT vs BIGINT).
                    let unified_type = common_type(&left_col.data_type, &right_col.data_type)
                        .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                            operator: "=".to_string(),
                            left: left_col.data_type.pg_display_name(),
                            right: right_col.data_type.pg_display_name(),
                        })?;

                    resolved.push(ResolvedUsingColumn {
                        name: col_name.clone(),
                        left_index: left_col.column_index - left_start,
                        right_index: right_col.column_index - right_start,
                        data_type: unified_type,
                        left_type: left_col.data_type.clone(),
                        right_type: right_col.data_type.clone(),
                    });
                }
                Ok(JoinCondition::Using(resolved))
            }
            ast::JoinConstraint::Natural => {
                // NATURAL JOIN = USING on all columns with matching names.
                let scope = self.scopes.current();
                let mut resolved = Vec::new();
                let mut seen = std::collections::HashSet::new();

                // Collect left-side column names (only this join's left table).
                let left_cols: Vec<_> = scope
                    .columns()
                    .iter()
                    .filter(|c| c.column_index >= left_start && c.column_index < right_start)
                    .collect();

                // For each left column, find a matching right column.
                for lc in &left_cols {
                    let lower = lc.column_name.to_lowercase();
                    if seen.contains(&lower) {
                        continue;
                    }
                    if let Some(rc) = scope.columns().iter().find(|c| {
                        c.column_index >= right_start && c.column_name.to_lowercase() == lower
                    }) {
                        let unified_type =
                            common_type(&lc.data_type, &rc.data_type).ok_or_else(|| {
                                AnalyzerError::OperatorTypeMismatch {
                                    operator: "=".to_string(),
                                    left: lc.data_type.pg_display_name(),
                                    right: rc.data_type.pg_display_name(),
                                }
                            })?;
                        resolved.push(ResolvedUsingColumn {
                            name: lc.column_name.clone(),
                            left_index: lc.column_index - left_start,
                            right_index: rc.column_index - right_start,
                            data_type: unified_type,
                            left_type: lc.data_type.clone(),
                            right_type: rc.data_type.clone(),
                        });
                        seen.insert(lower);
                    }
                }

                Ok(JoinCondition::Using(resolved))
            }
            ast::JoinConstraint::None => Ok(JoinCondition::None),
        }
    }
}
