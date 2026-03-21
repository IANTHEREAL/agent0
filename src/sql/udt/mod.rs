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
use crate::sql::ddl::{check_relation_name_available, RelationKind};
use crate::storage::TikvStore;

mod enum_rewrite;
mod enum_values;
mod helpers;
mod rename;
#[cfg(test)]
mod tests;
mod validation;

pub use enum_values::{alter_type_add_value, alter_type_rename_value};
pub use rename::alter_type_rename;
pub(crate) use validation::{
    coerce_and_validate_value_for_column, load_enum_value_validator,
    validate_enum_value_against_labels,
};

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

    // Unified namespace check: ensure no table/view/matview/sequence/type/index
    // already holds this name. Writes a sys_relname_ reservation key on success.
    check_relation_name_available(
        store,
        txn,
        db_id,
        &schema,
        &type_name,
        RelationKind::Type,
        false,
        None,
    )
    .await?;

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

    // Unified namespace check.
    check_relation_name_available(
        store,
        txn,
        db_id,
        &schema,
        &type_name,
        RelationKind::Type,
        false,
        None,
    )
    .await?;

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
        // Release unified namespace reservation key (no-op if missing).
        store
            .release_relation_name(txn, db_id, full_name)
            .await?;
    }
    Ok(ExecuteResult::CommandComplete { tag: "DROP TYPE" })
}

/// Resolve a composite field type with catalog access for UDTs.
/// Arrays of custom types (e.g. mood[]) recurse into the inner type.
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
        sqlparser::ast::DataType::Array(inner) => {
            // Unwrap all Array layers to find the leaf type, then resolve it with catalog lookup.
            // This handles multi-dimensional arrays like mood[][] correctly.
            let mut leaf_type = inner;
            let mut array_depth = 1;
            loop {
                match leaf_type {
                    sqlparser::ast::ArrayElemTypeDef::AngleBracket(t)
                    | sqlparser::ast::ArrayElemTypeDef::SquareBracket(t) => match t.as_ref() {
                        sqlparser::ast::DataType::Array(nested) => {
                            leaf_type = nested;
                            array_depth += 1;
                        }
                        _ => break,
                    },
                    sqlparser::ast::ArrayElemTypeDef::None => {
                        return sql_datatype_to_internal_strict(&sqlparser::ast::DataType::Array(
                            sqlparser::ast::ArrayElemTypeDef::None,
                        ));
                    }
                }
            }
            // Now resolve the leaf type with catalog lookup.
            let leaf_sql_type = match leaf_type {
                sqlparser::ast::ArrayElemTypeDef::AngleBracket(t)
                | sqlparser::ast::ArrayElemTypeDef::SquareBracket(t) => t.as_ref(),
                sqlparser::ast::ArrayElemTypeDef::None => {
                    return sql_datatype_to_internal_strict(&sqlparser::ast::DataType::Array(
                        sqlparser::ast::ArrayElemTypeDef::None,
                    ));
                }
            };
            let leaf_dt = match leaf_sql_type {
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
                    .await
                    .map_err(|e| {
                        crate::sql::types::wrap_undefined_object_for_sql_type(sql_type, e)
                    })?;
                    dt
                }
                _ => sql_datatype_to_internal_strict(leaf_sql_type).map_err(|e| {
                    crate::sql::types::wrap_undefined_object_for_sql_type(sql_type, e)
                })?,
            };
            // Re-wrap with all array layers.
            let mut result_dt = leaf_dt;
            for _ in 0..array_depth {
                result_dt = crate::model::DataType::Array(Box::new(result_dt));
            }
            Ok(result_dt)
        }
        _ => sql_datatype_to_internal_strict(sql_type),
    }
}
