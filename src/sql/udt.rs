use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{ObjectName, UserDefinedTypeRepresentation};
use tikv_client::Transaction;

use super::helpers::{convert_data_type, normalize_ident};
use super::names;
use super::ExecuteResult;
use crate::storage::TikvStore;
use crate::types::{UserTypeDef, UserTypeKind};

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
                let field_type = convert_data_type(&attr.data_type)?;
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
    Ok(ExecuteResult::Empty)
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
    if labels.is_empty() {
        return Err(anyhow!("ENUM must have at least one value"));
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
    Ok(ExecuteResult::Empty)
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
                    .find(|c| matches!(&c.data_type, crate::types::DataType::UserDefined(t) if t == full_name))
                {
                    return Err(anyhow!(
                        "cannot drop type {} because column {}.{} depends on it",
                        full_name, table_name, col.name
                    ));
                }
            }
        }

        store.drop_type(txn, db_id, full_name).await?;
    }
    Ok(ExecuteResult::Empty)
}
