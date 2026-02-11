//! Statement execution

use super::*;

impl Executor {
    /// Execute a parsed SQL statement on a given transaction
    pub(crate) async fn execute_statement_on_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        stmt: &Statement,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        match stmt {
            Statement::CreateTable {
                name,
                columns,
                constraints,
                if_not_exists,
                query,
                temporary,
                ..
            } => {
                let resolved = names::resolve_ddl_object_name(name, search_path)?;
                let table_full_name = resolved.full.clone();
                let already_exists = *if_not_exists
                    && self
                        .store
                        .table_exists(txn, db_id, &table_full_name)
                        .await?;

                let result = if let Some(q) = query {
                    self.execute_create_table_as(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        name,
                        q,
                        columns,
                        *if_not_exists,
                        *temporary,
                    )
                    .await?
                } else {
                    ddl::execute_create_table(
                        &self.store,
                        txn,
                        db_id,
                        search_path,
                        name,
                        columns,
                        constraints,
                        *if_not_exists,
                    )
                    .await?
                };

                if !already_exists {
                    let owner = current_role.unwrap_or("postgres");
                    if let Some(mut schema) =
                        self.store.get_schema(txn, db_id, &table_full_name).await?
                    {
                        schema.owner = owner.to_string();
                        self.store.update_schema(txn, db_id, schema).await?;
                    }

                    crate::sql::default_privileges::apply_default_table_privileges_for_new_table(
                        &self.auth_manager,
                        txn,
                        db_id,
                        owner,
                        &table_full_name,
                    )
                    .await?;
                }

                Ok(result)
            }
            Statement::CreateIndex {
                name,
                table_name,
                using,
                columns,
                unique,
                if_not_exists,
                predicate,
                ..
            } => {
                let index_name = name
                    .as_ref()
                    .ok_or_else(|| anyhow!("Index name required"))?;
                self.execute_create_index(
                    txn,
                    db_id,
                    search_path,
                    index_name,
                    table_name,
                    using.as_ref(),
                    columns,
                    *unique,
                    *if_not_exists,
                    predicate.as_ref(),
                )
                .await
            }
            Statement::Drop {
                object_type,
                names,
                if_exists,
                cascade,
                ..
            } => {
                use sqlparser::ast::ObjectType;
                match object_type {
                    ObjectType::Table => {
                        ddl::execute_drop_table(
                            &self.store,
                            txn,
                            db_id,
                            search_path,
                            names,
                            *if_exists,
                        )
                        .await
                    }
                    ObjectType::View => {
                        ddl::execute_drop_view(
                            &self.store,
                            txn,
                            db_id,
                            search_path,
                            names,
                            *if_exists,
                        )
                        .await
                    }
                    ObjectType::Index => {
                        self.execute_drop_index(txn, db_id, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::Role => {
                        rbac::execute_drop_role(&self.auth_manager, txn, names, *if_exists).await
                    }
                    ObjectType::Sequence => {
                        sequences::execute_drop_sequence(
                            &self.store,
                            txn,
                            db_id,
                            search_path,
                            names,
                            *if_exists,
                        )
                        .await
                    }
                    ObjectType::Schema => {
                        self.execute_drop_schema(
                            txn,
                            db_id,
                            search_path,
                            names,
                            *if_exists,
                            *cascade,
                        )
                        .await
                    }
                    _ => Err(SqlError::Unsupported(format!(
                        "DROP {} is not supported",
                        object_type
                    ))
                    .into()),
                }
            }
            Statement::Truncate { table_name, .. } => {
                ddl::execute_truncate(&self.store, txn, db_id, search_path, table_name).await
            }
            Statement::AlterTable {
                name, operations, ..
            } => {
                for op in operations {
                    self.execute_alter_table(txn, db_id, search_path, name, op)
                        .await?;
                }
                let table_name = name.0.last().unwrap().value.clone();
                Ok(ExecuteResult::AlterTable { table_name })
            }
            Statement::Insert {
                table_name,
                columns,
                source,
                returning,
                on,
                ..
            } => {
                self.execute_insert(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    table_name,
                    columns,
                    source,
                    returning,
                    on,
                )
                .await
            }
            Statement::Delete {
                from,
                using,
                selection,
                returning,
                ..
            } => {
                let using = using.as_deref().unwrap_or(&[]);
                self.execute_delete(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    from,
                    using,
                    selection,
                    returning,
                )
                .await
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
                ..
            } => {
                self.execute_update(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    table,
                    assignments,
                    from,
                    selection,
                    returning,
                )
                .await
            }
            Statement::Query(query) => {
                self.execute_query(txn, db_id, sequence_values, search_path, query)
                    .await
            }
            Statement::ShowTables { .. } => self.execute_show_tables(txn, db_id, search_path).await,
            Statement::SetVariable { .. }
            | Statement::SetTimeZone { .. }
            | Statement::SetNames { .. }
            | Statement::SetTransaction { .. } => {
                Err(SqlError::Unsupported("SET is not supported in this context".into()).into())
            }
            Statement::CreateType {
                name,
                representation,
            } => {
                udt::execute_create_type(&self.store, txn, db_id, search_path, name, representation)
                    .await
            }
            Statement::CreateSchema {
                schema_name,
                if_not_exists,
            } => {
                use sqlparser::ast::SchemaName;
                let schema_obj = match schema_name {
                    SchemaName::Simple(name) => name,
                    SchemaName::NamedAuthorization(name, _) => name,
                    SchemaName::UnnamedAuthorization(_) => {
                        return Err(SqlError::Unsupported(
                            "Unsupported CREATE SCHEMA syntax".into(),
                        )
                        .into());
                    }
                };
                let (schema_prefix, schema) = names::split_object_name(schema_obj)?;
                if schema_prefix.is_some() {
                    return Err(anyhow!("Invalid schema name '{}'", schema_obj));
                }
                self.store
                    .create_schema(txn, db_id, &schema, *if_not_exists)
                    .await?;
                Ok(ExecuteResult::CommandComplete {
                    tag: "CREATE SCHEMA",
                })
            }
            Statement::CreateFunction { .. } => Err(SqlError::Unsupported(
                "CREATE FUNCTION is not supported in this context".into(),
            )
            .into()),
            Statement::CreateProcedure {
                name, params, body, ..
            } => {
                self.execute_create_procedure(
                    txn,
                    db_id,
                    search_path,
                    name,
                    params.as_deref(),
                    body,
                )
                .await
            }
            Statement::CreateSequence {
                name,
                if_not_exists,
                sequence_options,
                ..
            } => {
                sequences::execute_create_sequence(
                    &self.store,
                    txn,
                    db_id,
                    search_path,
                    name,
                    *if_not_exists,
                    sequence_options,
                )
                .await
            }
            Statement::CreateView {
                name,
                query,
                or_replace,
                materialized,
                ..
            } => {
                if *materialized {
                    self.execute_create_materialized_view(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        name,
                        query,
                        *or_replace,
                    )
                    .await
                } else {
                    let resolved = names::resolve_ddl_object_name(name, search_path)?;
                    let view_full_name = resolved.full.clone();
                    let existed = self
                        .store
                        .get_view(txn, db_id, &view_full_name)
                        .await?
                        .is_some();

                    let result = ddl::execute_create_view(
                        &self.store,
                        txn,
                        db_id,
                        search_path,
                        name,
                        query,
                        *or_replace,
                    )
                    .await?;

                    if !existed {
                        let owner = current_role.unwrap_or("postgres");
                        crate::sql::default_privileges::apply_default_table_privileges_for_new_table(
                            &self.auth_manager,
                            txn,
                            db_id,
                            owner,
                            &view_full_name,
                        )
                        .await?;
                    }

                    Ok(result)
                }
            }
            Statement::AlterIndex { name, operation } => match operation {
                AlterIndexOperation::RenameIndex {
                    index_name: new_name,
                } => {
                    self.execute_alter_index_rename(txn, db_id, search_path, name, new_name)
                        .await
                }
            },
            Statement::CreateRole {
                names,
                if_not_exists,
                login,
                password,
                superuser,
                create_db,
                create_role,
                ..
            } => {
                rbac::execute_create_role(
                    &self.auth_manager,
                    txn,
                    names,
                    *if_not_exists,
                    login,
                    password,
                    superuser,
                    create_db,
                    create_role,
                )
                .await
            }
            Statement::AlterRole { name, operation } => {
                rbac::execute_alter_role(&self.store, &self.auth_manager, txn, name, operation)
                    .await
            }
            Statement::Grant {
                privileges,
                objects,
                grantees,
                with_grant_option,
                ..
            } => {
                rbac::execute_grant(
                    &self.store,
                    &self.auth_manager,
                    txn,
                    db_id,
                    privileges,
                    objects,
                    grantees,
                    *with_grant_option,
                )
                .await
            }
            Statement::Revoke {
                privileges,
                objects,
                grantees,
                ..
            } => {
                rbac::execute_revoke(
                    &self.store,
                    &self.auth_manager,
                    txn,
                    db_id,
                    privileges,
                    objects,
                    grantees,
                )
                .await
            }
            Statement::Comment { .. } => {
                Err(SqlError::Unsupported("COMMENT is not supported in this context".into()).into())
            }
            Statement::Copy { .. } => {
                Err(SqlError::Unsupported("COPY is not supported in this context".into()).into())
            }
            Statement::Explain {
                statement,
                analyze,
                verbose,
                ..
            } => {
                self.execute_explain(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    statement,
                    *analyze,
                    *verbose,
                )
                .await
            }
            Statement::DropFunction {
                if_exists,
                func_desc,
                option,
                ..
            } => {
                let cascade = option
                    .as_ref()
                    .is_some_and(|action| matches!(action, ReferentialAction::Cascade));
                let mut last_name = None;
                for desc in func_desc {
                    let func_name = &desc.name;
                    let resolved = names::resolve_existing_function_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        func_name,
                        search_path,
                    )
                    .await?;
                    let func_full_name = match resolved {
                        Some(resolved) => resolved.full,
                        None => names::resolve_ddl_object_name(func_name, search_path)?.full,
                    };
                    last_name = Some(func_full_name.clone());
                    let dropped = self
                        .store
                        .drop_function(txn, db_id, &func_full_name, cascade)
                        .await?;
                    if !dropped && !if_exists {
                        return Err(anyhow!("Function '{}' does not exist", func_full_name));
                    }
                }
                Ok(ExecuteResult::DropFunction {
                    func_name: last_name.unwrap_or_else(|| "unknown".to_string()),
                })
            }
            _ => Err(SqlError::Unsupported(format!("Unsupported statement: {:?}", stmt)).into()),
        }
    }
}

impl Executor {
    async fn execute_drop_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        _search_path: &[String],
        names: &[sqlparser::ast::ObjectName],
        if_exists: bool,
        cascade: bool,
    ) -> Result<ExecuteResult> {
        for name in names {
            let (schema_prefix, schema) = names::split_object_name(name)?;
            if schema_prefix.is_some() {
                return Err(anyhow!("Invalid schema name '{}'", name));
            }
            if cascade {
                self.store
                    .drop_schema_cascade(txn, db_id, &schema, if_exists)
                    .await?;
            } else {
                self.store
                    .drop_schema_restrict(txn, db_id, &schema, if_exists)
                    .await?;
            }
        }
        Ok(ExecuteResult::CommandComplete { tag: "DROP SCHEMA" })
    }
    async fn execute_alter_index_rename(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        old_name: &sqlparser::ast::ObjectName,
        new_name: &sqlparser::ast::ObjectName,
    ) -> Result<ExecuteResult> {
        let (schema_opt, idx_name) = names::split_object_name(old_name)?;
        let (_, new_idx_name) = names::split_object_name(new_name)?;

        let schema_filter: Vec<&str> = match schema_opt.as_deref() {
            Some(schema) => vec![schema],
            None => {
                if search_path.is_empty() {
                    vec!["public"]
                } else {
                    search_path.iter().map(|s| s.as_str()).collect()
                }
            }
        };

        // Find the table that owns this index
        let tables = self.store().list_tables(txn, db_id).await?;
        let mut found_table: Option<String> = None;
        for table_name in &tables {
            let table_schema = table_name.splitn(2, '.').next().unwrap_or("");
            if !schema_filter.iter().any(|s| *s == table_schema) {
                continue;
            }
            let schema = match self.store().get_schema(txn, db_id, table_name).await? {
                Some(s) => s,
                None => continue,
            };
            if schema.indexes.iter().any(|i| i.name == idx_name) {
                found_table = Some(table_name.clone());
                break;
            }
        }

        let table_name =
            found_table.ok_or_else(|| anyhow!("index \"{}\" does not exist", idx_name))?;

        let mut schema = self
            .store()
            .get_schema(txn, db_id, &table_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(table_name.clone()))?;

        // Check for name conflict
        if schema.indexes.iter().any(|i| i.name == new_idx_name) {
            return Err(anyhow!("relation \"{}\" already exists", new_idx_name));
        }

        // Rename the index
        for idx in &mut schema.indexes {
            if idx.name == idx_name {
                idx.name = new_idx_name.clone();
                break;
            }
        }

        schema.version += 1;
        self.store().update_schema(txn, db_id, schema).await?;

        Ok(ExecuteResult::AlterIndex {
            index_name: new_idx_name,
        })
    }

    async fn execute_show_tables(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
    ) -> Result<ExecuteResult> {
        let current_schema = names::default_schema(search_path);
        let mut tables = Vec::new();
        for full_name in self.store.list_tables(txn, db_id).await? {
            match names::parse_full_name(&full_name) {
                Ok((schema, name)) => {
                    if schema == current_schema {
                        tables.push(name);
                    }
                }
                Err(_) => tables.push(full_name),
            }
        }
        Ok(ExecuteResult::ShowTables { tables })
    }

    async fn execute_explain(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        statement: &Statement,
        analyze: bool,
        _verbose: bool,
    ) -> Result<ExecuteResult> {
        let (actual_rows, execution_time_ms, kv_stats) = if analyze {
            match statement {
                Statement::Query(query) => {
                    let start = Instant::now();
                    let (result, kv_stats) = with_kv_read_stats(async {
                        self.execute_query(txn, db_id, sequence_values, search_path, query)
                            .await
                    })
                    .await;
                    let result = result?;
                    let elapsed = start.elapsed();
                    let actual_rows = match result {
                        ExecuteResult::Select { rows, .. } => rows.len(),
                        _ => 0,
                    };
                    (
                        Some(actual_rows),
                        Some(elapsed.as_secs_f64() * 1000.0),
                        Some(kv_stats),
                    )
                }
                _ => {
                    return Err(anyhow!(
                        "EXPLAIN (ANALYZE) is only supported for SELECT/WITH statements"
                    ));
                }
            }
        } else {
            (None, None, None)
        };

        let tables = self.store.list_tables(txn, db_id).await?;
        let mut schemas_by_full: HashMap<String, TableSchema> = HashMap::new();
        let mut schemas_by_short: HashMap<String, Option<TableSchema>> = HashMap::new();
        for table_name in &tables {
            if let Ok(Some(schema)) = self.store.get_schema(txn, db_id, table_name).await {
                schemas_by_full.insert(table_name.clone(), schema.clone());

                // EXPLAIN queries often refer to tables without schema qualification.
                // Provide a short-name lookup when the name is unambiguous.
                let short = table_name
                    .rsplit('.')
                    .next()
                    .unwrap_or(table_name.as_str())
                    .to_string();
                match schemas_by_short.get(&short) {
                    None => {
                        schemas_by_short.insert(short, Some(schema));
                    }
                    Some(Some(_)) => {
                        schemas_by_short.insert(short, None);
                    }
                    Some(None) => {}
                }
            }
        }

        let schema_lookup = |table_name: &str| -> Option<TableSchema> {
            if let Some(schema) = schemas_by_full.get(table_name) {
                return Some(schema.clone());
            }
            schemas_by_short.get(table_name).and_then(|s| s.clone())
        };

        let row_count_lookup = |_table_name: &str| -> usize { 1000 };

        let plan = explain::generate_plan(statement, schema_lookup, row_count_lookup);
        let mut plan_text = explain::format_plan_text(&plan, 0);
        if let (Some(actual_rows), Some(execution_time_ms)) = (actual_rows, execution_time_ms) {
            use std::fmt::Write;
            writeln!(&mut plan_text, "Actual Rows: {}", actual_rows).unwrap();
            writeln!(
                &mut plan_text,
                "Execution Time: {:.3} ms",
                execution_time_ms
            )
            .unwrap();
        }
        if let Some(KvReadStatsSnapshot {
            table_scan_pairs,
            index_scan_pairs,
            batch_get_keys,
        }) = kv_stats
        {
            use std::fmt::Write;
            writeln!(&mut plan_text, "KV Table Scan Pairs: {}", table_scan_pairs).unwrap();
            writeln!(&mut plan_text, "KV Index Scan Pairs: {}", index_scan_pairs).unwrap();
            writeln!(&mut plan_text, "KV Batch Get Keys: {}", batch_get_keys).unwrap();
        }

        let lines: Vec<Row> = plan_text
            .lines()
            .map(|line| Row::new(vec![Value::Text(line.to_string())]))
            .collect();

        Ok(ExecuteResult::Select {
            column_types: None,
            columns: vec!["QUERY PLAN".to_string()],
            rows: lines,
            timezone: session_context::current_timezone(),
        })
    }
}
