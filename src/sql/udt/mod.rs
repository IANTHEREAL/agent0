use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{ObjectName, UserDefinedTypeRepresentation};
use tikv_client::Transaction;

use super::names;
use super::names::normalize_ident;
use super::types::sql_datatype_to_internal_strict;
use super::types::{resolve_custom_type_with_catalog, TypeResolutionContext};
use super::ExecuteResult;
use crate::model::{UserTypeDef, UserTypeKind};
use crate::storage::TikvStore;

mod enum_rewrite;
mod enum_values;
mod helpers;
mod rename;
#[cfg(test)]
mod tests;

pub use enum_values::{alter_type_add_value, alter_type_rename_value};
pub use rename::alter_type_rename;

pub(crate) fn resolve_type_name(
    name: &ObjectName,
    search_path: &[String],
) -> Result<(String, String, String)> {
    let resolved = names::resolve_ddl_object_name(name, search_path)?;
    Ok((resolved.schema, resolved.name, resolved.full))
}

pub async fn execute_create_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    representation: &UserDefinedTypeRepresentation,
) -> Result<ExecuteResult> {
    let (schema, type_name, full_name) = resolve_type_name(name, search_path)?;
    if !store.schema_exists(txn, db_id, &schema).await? {
        return Err(anyhow!("schema '{}' does not exist", schema));
    }

    let kind = match representation {
        UserDefinedTypeRepresentation::Composite { attributes } => {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut fields = Vec::with_capacity(attributes.len());
            for attr in attributes {
                let field_name = normalize_ident(&attr.name);
                if !seen.insert(field_name.clone()) {
                    return Err(anyhow!(
                        "composite type {} has duplicate field \"{}\"",
                        full_name,
                        field_name
                    ));
                }
                let field_type =
                    resolve_composite_field_type(store, txn, db_id, search_path, &attr.data_type)
                        .await?;
                fields.push((field_name, field_type));
            }
            UserTypeKind::Composite { fields }
        }
    };

    let oid = store.next_type_oid(txn, db_id).await?;
    let def = UserTypeDef {
        oid,
        schema,
        name: type_name,
        kind,
        owner: "postgres".to_string(),
    };

    store.create_type(txn, db_id, def).await?;
    Ok(ExecuteResult::CommandComplete { tag: "CREATE TYPE" })
}

pub async fn create_enum_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: String,
    type_name: String,
    labels: Vec<String>,
) -> Result<ExecuteResult> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for label in &labels {
        if !seen.insert(label.clone()) {
            return Err(anyhow!(
                "enum label \"{}\" already exists for type {}.{}",
                label,
                schema,
                type_name
            ));
        }
    }
    let oid = store.next_type_oid(txn, db_id).await?;
    let def = UserTypeDef {
        oid,
        schema,
        name: type_name,
        kind: UserTypeKind::Enum { labels },
        owner: "postgres".to_string(),
    };

    store.create_type(txn, db_id, def).await?;
    Ok(ExecuteResult::CommandComplete { tag: "CREATE TYPE" })
}

pub async fn drop_types(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    full_names: &[String],
    if_exists: bool,
) -> Result<ExecuteResult> {
    let user_tables = store.list_tables(txn, db_id).await?;

    for full_name in full_names {
        let exists = store.get_type(txn, db_id, full_name).await?.is_some();
        if !exists {
            if if_exists {
                continue;
            }
            return Err(anyhow!("Type '{}' does not exist", full_name));
        }

        for table_name in &user_tables {
            if let Some(schema) = store.get_schema(txn, db_id, table_name).await? {
                if let Some(col) = schema
                    .columns
                    .iter()
                    .find(|c| matches!(&c.data_type, crate::model::DataType::UserDefined(t) if t == full_name))
                {
                    let bare_type = full_name.rsplit('.').next().unwrap_or(full_name);
                    let bare_table = table_name.rsplit('.').next().unwrap_or(table_name);
                    return Err(anyhow!(
                        "cannot drop type {} because other objects depend on it\nDETAIL:  column {} of table {} depends on type {}\nHINT:  Use DROP ... CASCADE to drop the dependent objects too.",
                        bare_type, col.name, bare_table, bare_type
                    ));
                }
            }
        }

        store.drop_type(txn, db_id, full_name).await?;
    }
    Ok(ExecuteResult::CommandComplete { tag: "DROP TYPE" })
}

/// Resolve a composite field type with catalog access for UDTs.
async fn resolve_composite_field_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    sql_type: &sqlparser::ast::DataType,
) -> Result<crate::model::DataType> {
    match sql_type {
        sqlparser::ast::DataType::Custom(name, modifiers) => {
            let (dt, _) = resolve_custom_type_with_catalog(
                TypeResolutionContext::DdlOther,
                store,
                txn,
                db_id,
                search_path,
                name,
                modifiers,
            )
            .await?;
            Ok(dt)
        }
        _ => sql_datatype_to_internal_strict(sql_type),
    }
}
