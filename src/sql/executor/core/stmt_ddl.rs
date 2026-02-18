//! DDL statement sub-dispatcher

use super::*;
use crate::auth::{Privilege, PrivilegeObject};
use crate::sql::error::SqlError;
use sqlparser::ast::{ObjectType, ReferentialAction, SchemaName};

impl Executor {
    pub(super) async fn execute_ddl_statement(
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
}
