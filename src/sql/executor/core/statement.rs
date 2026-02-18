//! Statement execution

use super::catalog_prefetch::build_catalog_snapshot_for_statement;
use super::*;
use crate::auth::{Privilege, PrivilegeObject};
use crate::sql::analyzer::types::AnalyzedStatement;
use crate::sql::analyzer::Analyzer;
use crate::sql::error::SqlError;

impl Executor {
    async fn require_privilege(
        &self,
        txn: &mut Transaction,
        current_role: Option<&str>,
        privilege: Privilege,
        object: PrivilegeObject,
        object_type: &str,
        object_name: String,
    ) -> Result<()> {
        let Some(username) = current_role else {
            // Internal execution path (trigger worker, internal plumbing).
            // Until we have explicit security context propagation for internal
            // statements, bypass RBAC checks when no user is provided.
            return Ok(());
        };

        let ok = self
            .auth_manager
            .check_privilege(txn, username, &privilege, &object)
            .await?;
        if !ok {
            return Err(SqlError::PermissionDenied {
                object_type: object_type.to_string(),
                object_name,
            }
            .into());
        }
        Ok(())
    }

    pub(crate) async fn require_table_privilege(
        &self,
        txn: &mut Transaction,
        current_role: Option<&str>,
        privilege: Privilege,
        table_full_name: &str,
    ) -> Result<()> {
        let (schema, name) = names::parse_full_name(table_full_name)
            .unwrap_or(("public".to_string(), table_full_name.to_string()));
        self.require_privilege(
            txn,
            current_role,
            privilege,
            PrivilegeObject::Table {
                schema: schema.clone(),
                name: name.clone(),
            },
            "table",
            format!("{}.{}", schema, name),
        )
        .await
    }

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
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::CreateTable,
                    PrivilegeObject::Schema(resolved.schema.clone()),
                    "schema",
                    resolved.schema.clone(),
                )
                .await?;
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
                        current_role,
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
                concurrently,
                predicate,
                ..
            } => {
                let index_name = name
                    .as_ref()
                    .ok_or_else(|| anyhow!("Index name required"))?;
                let resolved = names::resolve_existing_table_name(
                    self.store.as_ref(),
                    txn,
                    db_id,
                    table_name,
                    search_path,
                )
                .await?
                .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::CreateTable,
                    PrivilegeObject::Schema(resolved.schema.clone()),
                    "schema",
                    resolved.schema.clone(),
                )
                .await?;
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
                    *concurrently,
                    predicate.as_ref(),
                    current_role.unwrap_or("postgres"),
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
                        for name in names {
                            let resolved = names::resolve_existing_table_name(
                                self.store.as_ref(),
                                txn,
                                db_id,
                                name,
                                search_path,
                            )
                            .await?;
                            if let Some(resolved) = resolved {
                                self.require_privilege(
                                    txn,
                                    current_role,
                                    Privilege::DropTable,
                                    PrivilegeObject::Table {
                                        schema: resolved.schema.clone(),
                                        name: resolved.name.clone(),
                                    },
                                    "table",
                                    resolved.full.clone(),
                                )
                                .await?;
                            } else if !*if_exists {
                                let resolved = names::resolve_ddl_object_name(name, search_path)?;
                                self.require_privilege(
                                    txn,
                                    current_role,
                                    Privilege::DropTable,
                                    PrivilegeObject::Table {
                                        schema: resolved.schema.clone(),
                                        name: resolved.name.clone(),
                                    },
                                    "table",
                                    resolved.full.clone(),
                                )
                                .await?;
                            }
                        }
                        ddl::execute_drop_table(
                            &self.store,
                            txn,
                            db_id,
                            search_path,
                            names,
                            *if_exists,
                            *cascade,
                            &self.stats_cache,
                        )
                        .await
                    }
                    ObjectType::View => {
                        for name in names {
                            let resolved = names::resolve_existing_view_name(
                                self.store.as_ref(),
                                txn,
                                db_id,
                                name,
                                search_path,
                            )
                            .await?;
                            if let Some(resolved) = resolved {
                                self.require_privilege(
                                    txn,
                                    current_role,
                                    Privilege::DropTable,
                                    PrivilegeObject::Table {
                                        schema: resolved.schema.clone(),
                                        name: resolved.name.clone(),
                                    },
                                    "view",
                                    resolved.full.clone(),
                                )
                                .await?;
                            } else if !*if_exists {
                                let resolved = names::resolve_ddl_object_name(name, search_path)?;
                                self.require_privilege(
                                    txn,
                                    current_role,
                                    Privilege::DropTable,
                                    PrivilegeObject::Table {
                                        schema: resolved.schema.clone(),
                                        name: resolved.name.clone(),
                                    },
                                    "view",
                                    resolved.full.clone(),
                                )
                                .await?;
                            }
                        }
                        ddl::execute_drop_view(
                            &self.store,
                            txn,
                            db_id,
                            search_path,
                            names,
                            *if_exists,
                            *cascade,
                        )
                        .await
                    }
                    ObjectType::Index => {
                        self.require_privilege(
                            txn,
                            current_role,
                            Privilege::SuperUser,
                            PrivilegeObject::Global,
                            "index",
                            "index".to_string(),
                        )
                        .await?;
                        self.execute_drop_index(txn, db_id, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::Role => {
                        self.require_privilege(
                            txn,
                            current_role,
                            Privilege::CreateRole,
                            PrivilegeObject::Global,
                            "role",
                            "role".to_string(),
                        )
                        .await?;
                        rbac::execute_drop_role(&self.auth_manager, txn, names, *if_exists).await
                    }
                    ObjectType::Sequence => {
                        self.require_privilege(
                            txn,
                            current_role,
                            Privilege::SuperUser,
                            PrivilegeObject::Global,
                            "sequence",
                            "sequence".to_string(),
                        )
                        .await?;
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
                        self.require_privilege(
                            txn,
                            current_role,
                            Privilege::SuperUser,
                            PrivilegeObject::Global,
                            "schema",
                            "schema".to_string(),
                        )
                        .await?;
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
                let resolved = names::resolve_existing_table_name(
                    self.store.as_ref(),
                    txn,
                    db_id,
                    table_name,
                    search_path,
                )
                .await?
                .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::Truncate,
                    PrivilegeObject::Table {
                        schema: resolved.schema.clone(),
                        name: resolved.name.clone(),
                    },
                    "table",
                    resolved.full.clone(),
                )
                .await?;
                ddl::execute_truncate(&self.store, txn, db_id, search_path, table_name).await
            }
            Statement::AlterTable {
                name, operations, ..
            } => {
                let resolved = names::resolve_ddl_object_name(name, search_path)?;
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::SuperUser,
                    PrivilegeObject::Global,
                    "table",
                    resolved.full.clone(),
                )
                .await?;
                for op in operations {
                    self.execute_alter_table(txn, db_id, search_path, name, op)
                        .await?;
                }
                let table_name = name.0.last().unwrap().value.clone();
                Ok(ExecuteResult::AlterTable { table_name })
            }
            Statement::Insert { .. } => {
                // All INSERT variants (VALUES, DEFAULT VALUES, SELECT) use the
                // fully analyzed path.
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;
                let mut analyzer = Analyzer::new(&catalog);
                let analyzed = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                match analyzed {
                    AnalyzedStatement::Insert(ins) => {
                        self.require_table_privilege(
                            txn,
                            current_role,
                            Privilege::Insert,
                            &ins.table_name,
                        )
                        .await?;
                        if matches!(
                            ins.on_conflict,
                            Some(crate::sql::analyzer::types::AnalyzedOnConflict::DoUpdate { .. })
                        ) {
                            self.require_table_privilege(
                                txn,
                                current_role,
                                Privilege::Update,
                                &ins.table_name,
                            )
                            .await?;
                        }
                        self.execute_analyzed_insert(txn, db_id, sequence_values, search_path, &ins)
                            .await
                    }
                    _ => unreachable!("INSERT statement should analyze to AnalyzedInsert"),
                }
            }
            Statement::Delete { .. } => {
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;
                let mut analyzer = Analyzer::new(&catalog);
                let analyzed = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                match analyzed {
                    AnalyzedStatement::Delete(del) => {
                        self.require_table_privilege(
                            txn,
                            current_role,
                            Privilege::Delete,
                            &del.table_name,
                        )
                        .await?;
                        self.execute_analyzed_delete(txn, db_id, sequence_values, search_path, &del)
                            .await
                    }
                    _ => unreachable!("DELETE statement should analyze to AnalyzedDelete"),
                }
            }
            Statement::Update { .. } => {
                let catalog = build_catalog_snapshot_for_statement(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    search_path,
                    self.tenant_keyspace(),
                    stmt,
                )
                .await?;
                let mut analyzer = Analyzer::new(&catalog);
                let analyzed = analyzer.analyze_statement(stmt).map_err(SqlError::from)?;
                match analyzed {
                    AnalyzedStatement::Update(upd) => {
                        self.require_table_privilege(
                            txn,
                            current_role,
                            Privilege::Update,
                            &upd.table_name,
                        )
                        .await?;
                        self.execute_analyzed_update(txn, db_id, sequence_values, search_path, &upd)
                            .await
                    }
                    _ => unreachable!("UPDATE statement should analyze to AnalyzedUpdate"),
                }
            }
            Statement::Query(query) => {
                self.execute_query(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    current_role,
                )
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
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::SuperUser,
                    PrivilegeObject::Global,
                    "schema",
                    "schema".to_string(),
                )
                .await?;
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
                let resolved = names::resolve_ddl_object_name(name, search_path)?;
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::CreateTable,
                    PrivilegeObject::Schema(resolved.schema.clone()),
                    "schema",
                    resolved.schema.clone(),
                )
                .await?;
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
                    let resolved = names::resolve_ddl_object_name(name, search_path)?;
                    self.require_privilege(
                        txn,
                        current_role,
                        Privilege::CreateTable,
                        PrivilegeObject::Schema(resolved.schema.clone()),
                        "schema",
                        resolved.schema.clone(),
                    )
                    .await?;
                    self.execute_create_materialized_view(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        name,
                        query,
                        *or_replace,
                        current_role,
                    )
                    .await
                } else {
                    let resolved = names::resolve_ddl_object_name(name, search_path)?;
                    self.require_privilege(
                        txn,
                        current_role,
                        Privilege::CreateTable,
                        PrivilegeObject::Schema(resolved.schema.clone()),
                        "schema",
                        resolved.schema.clone(),
                    )
                    .await?;
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
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::CreateRole,
                    PrivilegeObject::Global,
                    "role",
                    "role".to_string(),
                )
                .await?;
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
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::CreateRole,
                    PrivilegeObject::Global,
                    "role",
                    name.value.clone(),
                )
                .await?;
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
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::SuperUser,
                    PrivilegeObject::Global,
                    "privilege",
                    "privilege".to_string(),
                )
                .await?;
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
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::SuperUser,
                    PrivilegeObject::Global,
                    "privilege",
                    "privilege".to_string(),
                )
                .await?;
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
                    current_role,
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

        // Find the table that owns this index (check both regular indexes and PK constraint)
        let tables = self.store().list_tables(txn, db_id).await?;
        let mut found_table: Option<String> = None;
        let mut is_pk = false;
        for table_name in &tables {
            let table_schema = table_name.splitn(2, '.').next().unwrap_or("");
            if !schema_filter.iter().any(|s| *s == table_schema) {
                continue;
            }
            let schema = match self.store().get_schema(txn, db_id, table_name).await? {
                Some(s) => s,
                None => continue,
            };
            // Check PK constraint name
            if !schema.pk_indices.is_empty() {
                let pk_name = schema.pk_constraint_name.as_deref().unwrap_or("");
                let short = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                let default_pk = format!("{}_pkey", short);
                let effective_pk = if pk_name.is_empty() {
                    &default_pk
                } else {
                    pk_name
                };
                if effective_pk == idx_name {
                    found_table = Some(table_name.clone());
                    is_pk = true;
                    break;
                }
            }
            if schema.indexes.iter().any(|i| i.name == idx_name) {
                found_table = Some(table_name.clone());
                break;
            }
        }

        let table_name =
            found_table.ok_or_else(|| anyhow!("index \"{}\" does not exist", idx_name))?;

        // Schema-wide namespace uniqueness check for the new name.
        let owning_schema = table_name.splitn(2, '.').next().unwrap_or("public");
        ddl::check_relation_name_available(
            &self.store(),
            txn,
            db_id,
            owning_schema,
            &new_idx_name,
            false,
            None,
        )
        .await?;

        let mut schema = self
            .store()
            .get_schema(txn, db_id, &table_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(table_name.clone()))?;

        if is_pk {
            schema.pk_constraint_name = Some(new_idx_name.clone());
        } else {
            for idx in &mut schema.indexes {
                if idx.name == idx_name {
                    idx.name = new_idx_name.clone();
                    break;
                }
            }
        }

        schema.version += 1;
        self.store().update_schema(txn, db_id, schema).await?;

        // Release the old name's reservation key.
        let old_full = format!("{}.{}", owning_schema, idx_name);
        self.store()
            .release_relation_name(txn, db_id, &old_full)
            .await?;

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
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        let (actual_rows, execution_time_ms, kv_stats) = if analyze {
            match statement {
                Statement::Query(query) => {
                    let start = Instant::now();
                    let (result, kv_stats) = with_kv_read_stats(async {
                        self.execute_query(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            query,
                            current_role,
                        )
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

        // For SELECT/WITH queries, run the same analysis pipeline as execution
        // (view expansion → catalog snapshot → Analyzer) so EXPLAIN shows the
        // same plan that actually runs.  Non-SELECT statements use the legacy
        // AST-based plan generation.
        let plan = if let Statement::Query(query) = statement {
            use crate::sql::executor::core::catalog_prefetch::build_catalog_snapshot;
            use crate::sql::executor::core::view_rewrite::expand_views_in_query;

            let expanded =
                expand_views_in_query(self.store().as_ref(), txn, db_id, search_path, query)
                    .await?;
            let catalog = build_catalog_snapshot(
                self.store().as_ref(),
                txn,
                db_id,
                search_path,
                self.tenant_keyspace(),
                &expanded,
                &HashMap::new(),
            )
            .await?;
            let mut analyzer = Analyzer::new(&catalog);
            match analyzer.analyze_query(&expanded) {
                Ok(analyzed) => {
                    // Same rewrite as execution path — invariant: EXPLAIN = execution.
                    let analyzed = crate::sql::rewriter::rewrite_query(analyzed);

                    // Always use the optimizer pipeline — single execution path.
                    // Build PlanningContext with real table statistics and schemas,
                    // identical to execution path.
                    let mut planning_ctx = crate::sql::optimizer::PlanningContext::empty();
                    {
                        let table_refs = crate::sql::optimizer::collect_query_table_refs(&analyzed);
                        let cte_names: HashSet<String> = analyzed
                            .ctes
                            .iter()
                            .map(|c| c.name.to_lowercase())
                            .collect();
                        let mut stats_attempted = HashSet::new();
                        for (name, schema, alias) in &table_refs {
                            let ctx_key = alias.unwrap_or(name);
                            let tid = schema.table_id;
                            let stats = if stats_attempted.insert(tid) {
                                self.get_or_load_stats(txn, db_id, tid).await?
                            } else {
                                self.stats_cache().get_full_stats(db_id, tid)
                            };
                            if let Some(stats) = stats {
                                planning_ctx.table_stats.insert(ctx_key.to_string(), stats);
                            }
                            let cte_key = name.to_lowercase();
                            if !cte_names.contains(&cte_key) {
                                if let Some(table_schema) =
                                    self.store().get_schema(txn, db_id, name).await?
                                {
                                    planning_ctx
                                        .table_schemas
                                        .insert(ctx_key.to_string(), table_schema);
                                }
                            }
                        }
                    }
                    {
                        let physical = crate::sql::optimizer::optimize(&analyzed, &planning_ctx);
                        explain::physical_plan_to_plan_node(&physical)
                    }
                }
                Err(_) => {
                    // Fallback to AST path if analysis fails (e.g. invalid query)
                    explain::generate_plan(statement, &schema_lookup, &row_count_lookup)
                }
            }
        } else {
            // Non-SELECT statements (DDL, DML) — use AST path
            // (these produce a trivial Result node)
            explain::generate_plan(statement, &schema_lookup, &row_count_lookup)
        };
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
