//! DDL statement sub-dispatcher

use super::*;
use crate::auth::{GrantedPrivilege, Privilege, PrivilegeObject};
use crate::sql::error::SqlError;
use crate::sql::sequences::SequenceSession;
use sqlparser::ast::{Expr, ObjectType, ReferentialAction, SchemaName};

impl Executor {
    pub(super) async fn execute_ddl_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        stmt: &Statement,
        create_index_with_params: Option<&str>,
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
                if let Some(index_name) = name.as_ref() {
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
                        create_index_with_params,
                        current_role.unwrap_or("postgres"),
                    )
                    .await
                } else {
                    self.execute_create_index_with_implicit_name(
                        txn,
                        db_id,
                        search_path,
                        &resolved.name,
                        table_name,
                        using.as_ref(),
                        columns,
                        *unique,
                        *if_not_exists,
                        *concurrently,
                        predicate.as_ref(),
                        create_index_with_params,
                        current_role.unwrap_or("postgres"),
                    )
                    .await
                }
            }
            Statement::Drop {
                object_type,
                names,
                if_exists,
                cascade,
                ..
            } => match object_type {
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
                        sequence_values,
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
                        sequence_values,
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
                        sequence_values,
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
                        sequence_values,
                    )
                    .await
                }
                _ => Err(
                    SqlError::Unsupported(format!("DROP {} is not supported", object_type)).into(),
                ),
            },
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
                let (schema, owner) = match schema_name {
                    SchemaName::Simple(name) => {
                        let (schema_prefix, schema) = names::split_object_name(name)?;
                        if schema_prefix.is_some() {
                            return Err(anyhow!("Invalid schema name '{}'", name));
                        }
                        (schema, current_role.unwrap_or("postgres").to_string())
                    }
                    SchemaName::NamedAuthorization(name, auth_role) => {
                        let (schema_prefix, schema) = names::split_object_name(name)?;
                        if schema_prefix.is_some() {
                            return Err(anyhow!("Invalid schema name '{}'", name));
                        }
                        (schema, names::normalize_ident(auth_role))
                    }
                    SchemaName::UnnamedAuthorization(auth_role) => {
                        let owner = names::normalize_ident(auth_role);
                        (owner.clone(), owner)
                    }
                };

                let created = self
                    .store
                    .create_schema(txn, db_id, &schema, *if_not_exists)
                    .await?;
                if created {
                    self.grant_schema_owner_privileges(txn, &owner, &schema)
                        .await?;
                }
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
                        current_role.unwrap_or("postgres"),
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
            _ => unreachable!("DDL dispatcher received non-DDL statement"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_create_index_with_implicit_name(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        resolved_table_name: &str,
        table_name: &sqlparser::ast::ObjectName,
        using: Option<&sqlparser::ast::Ident>,
        columns: &[sqlparser::ast::OrderByExpr],
        unique: bool,
        if_not_exists: bool,
        concurrently: bool,
        predicate: Option<&Expr>,
        create_index_with_params: Option<&str>,
        current_role: &str,
    ) -> Result<ExecuteResult> {
        // PostgreSQL auto-generates a relation name for unnamed CREATE INDEX.
        // For compatibility we generate `<table>_<cols>_idx`, retrying with
        // numeric suffixes on name collisions.
        let base_name = build_implicit_index_name(resolved_table_name, columns);

        for attempt in 0..MAX_IMPLICIT_INDEX_NAME_RETRIES {
            let candidate = if attempt == 0 {
                base_name.clone()
            } else {
                format!("{base_name}{attempt}")
            };
            let idx_name = names::object_name_from_str(&candidate)?;
            match self
                .execute_create_index(
                    txn,
                    db_id,
                    search_path,
                    &idx_name,
                    table_name,
                    using,
                    columns,
                    unique,
                    if_not_exists,
                    concurrently,
                    predicate,
                    create_index_with_params,
                    current_role,
                )
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if e.downcast_ref::<SqlError>()
                        .is_some_and(|se| matches!(se, SqlError::DuplicateRelation(_)))
                    {
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(anyhow!(
            "failed to generate a unique implicit index name for table '{}' after {} attempts",
            resolved_table_name,
            MAX_IMPLICIT_INDEX_NAME_RETRIES
        ))
    }

    async fn grant_schema_owner_privileges(
        &self,
        txn: &mut Transaction,
        owner: &str,
        schema: &str,
    ) -> Result<()> {
        let object = PrivilegeObject::Schema(schema.to_string());
        let privileges = [Privilege::Usage, Privilege::CreateTable];

        if let Some(mut user) = self.auth_manager.get_user(txn, owner).await? {
            for privilege in &privileges {
                user.grant_privilege(privilege.clone(), object.clone(), true);
            }
            self.auth_manager.update_user(txn, user).await?;
            return Ok(());
        }

        if let Some(mut role) = self.auth_manager.get_role(txn, owner).await? {
            for privilege in &privileges {
                role.privileges
                    .retain(|p| !(p.privilege == *privilege && p.object == object));
                role.privileges.push(GrantedPrivilege {
                    privilege: privilege.clone(),
                    object: object.clone(),
                    with_grant_option: true,
                });
            }
            self.auth_manager.update_role(txn, role).await?;
            return Ok(());
        }

        Err(anyhow!("Role or user '{}' does not exist", owner))
    }

    async fn execute_drop_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        _search_path: &[String],
        names: &[sqlparser::ast::ObjectName],
        if_exists: bool,
        cascade: bool,
        sequence_values: &mut SequenceSession,
    ) -> Result<ExecuteResult> {
        for name in names {
            let (schema_prefix, schema) = names::split_object_name(name)?;
            if schema_prefix.is_some() {
                return Err(anyhow!("Invalid schema name '{}'", name));
            }
            if cascade {
                // Collect sequence names that will be dropped by the cascade,
                // so we can invalidate session state afterwards.
                let seqs = self.store.list_sequences(txn, db_id).await?;
                let schema_prefix_dot = format!("{}.", schema);
                let tables = self.store.list_tables(txn, db_id).await?;
                let tables_in_schema: std::collections::HashSet<&str> = tables
                    .iter()
                    .filter(|t| t.starts_with(&schema_prefix_dot))
                    .map(|t| t.as_str())
                    .collect();
                let mut dropped_seq_names = Vec::new();
                for def in &seqs {
                    // Sequences directly in the schema
                    if def.schema == schema {
                        dropped_seq_names.push(def.full_name());
                        continue;
                    }
                    // Sequences owned by tables in the schema
                    if let Some((owned_table, _)) = &def.owned_by {
                        if tables_in_schema.contains(owned_table.as_str()) {
                            dropped_seq_names.push(def.full_name());
                        }
                    }
                }

                self.store
                    .drop_schema_cascade(txn, db_id, &schema, if_exists)
                    .await?;

                for seq in dropped_seq_names {
                    sequence_values.defer_sequence_drop(seq);
                }
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
            let table_schema = table_name.split('.').next().unwrap_or("");
            if !schema_filter.contains(&table_schema) {
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
            found_table.ok_or_else(|| anyhow!("relation \"{}\" does not exist", idx_name))?;

        // Schema-wide namespace uniqueness check for the new name.
        let owning_schema = table_name.split('.').next().unwrap_or("public");
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

    /// Handle `ALTER INDEX IF EXISTS <name> RENAME TO <name>`.
    ///
    /// Intercepted via `RawSqlKind::AlterIndexIfExists` because sqlparser 0.40
    /// cannot parse `IF EXISTS` after `ALTER INDEX`. Extracts index names from
    /// raw SQL via regex and delegates to existing rename logic, suppressing
    /// "index not found" when `IF EXISTS` applies.
    pub(crate) async fn execute_alter_index_if_exists_rename(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let re = regex::Regex::new(
            r#"(?i)ALTER\s+INDEX\s+IF\s+EXISTS\s+("(?:[^"]+)"|[^\s]+)\s+RENAME\s+TO\s+("(?:[^"]+)"|[^\s;]+)"#,
        )?;
        let caps = re.captures(sql.trim()).ok_or_else(|| {
            anyhow!("syntax error: expected ALTER INDEX IF EXISTS <name> RENAME TO <name>")
        })?;
        let old_raw = caps.get(1).unwrap().as_str();
        let new_raw = caps.get(2).unwrap().as_str();
        let old_name = old_raw.trim_matches('"');
        let new_name = new_raw.trim_matches('"');

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let search_path: Vec<String> = session.search_path().to_vec();
            let old_obj =
                sqlparser::ast::ObjectName(vec![sqlparser::ast::Ident::with_quote('"', old_name)]);
            let new_obj =
                sqlparser::ast::ObjectName(vec![sqlparser::ast::Ident::with_quote('"', new_name)]);
            let txn = session.get_mut_txn().expect("txn must be active");
            match self
                .execute_alter_index_rename(txn, db_id, &search_path, &old_obj, &new_obj)
                .await
            {
                Ok(result) => Ok(result),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("does not exist") {
                        Ok(ExecuteResult::CommandComplete { tag: "ALTER INDEX" })
                    } else {
                        Err(e)
                    }
                }
            }
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}

const MAX_IMPLICIT_INDEX_NAME_RETRIES: u32 = 100;

fn build_implicit_index_name(table_name: &str, columns: &[sqlparser::ast::OrderByExpr]) -> String {
    let mut col_parts: Vec<String> = columns
        .iter()
        .map(|ob| implicit_index_col_part(&ob.expr))
        .collect();
    if col_parts.is_empty() {
        col_parts.push("expr".to_string());
    }
    format!("{}_{}_idx", table_name, col_parts.join("_"))
}

fn implicit_index_col_part(expr: &Expr) -> String {
    let mut cur = expr;
    while let Expr::Nested(inner) = cur {
        cur = inner.as_ref();
    }
    match cur {
        Expr::Identifier(ident) => names::normalize_ident(ident),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(names::normalize_ident)
            .unwrap_or_else(|| "expr".to_string()),
        _ => "expr".to_string(),
    }
}
