use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use sqlparser::ast::{Function, Ident, ObjectName};
use tikv_client::Transaction;

pub(crate) fn function_name_upper(func: &Function) -> String {
    func.name
        .0
        .last()
        .map(|n| n.value.to_uppercase())
        .unwrap_or_default()
}

pub fn normalize_ident(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_lowercase()
    }
}

/// Like [`normalize_ident`], but operates on a raw string slice.
/// Quoted identifiers (`"Foo"`) preserve case; unquoted are lowercased.
#[cfg_attr(not(feature = "parquet"), allow(dead_code))]
pub(crate) fn normalize_ident_str(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() > 1 {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_lowercase()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ResolvedName {
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) full: String,
}

impl ResolvedName {
    pub(crate) fn new(schema: String, name: String) -> Result<Self> {
        validate_schema_ident(&schema)?;
        validate_object_ident(&name)?;
        Ok(Self {
            full: format!("{}.{}", schema, name),
            schema,
            name,
        })
    }
}

pub(crate) fn default_schema(search_path: &[String]) -> &str {
    search_path
        .iter()
        .map(|s| s.as_str())
        .find(|s| !s.eq_ignore_ascii_case("$user"))
        .unwrap_or("public")
}

pub(crate) fn validate_schema_ident(schema: &str) -> Result<()> {
    if schema.is_empty() {
        return Err(anyhow!("schema name must not be empty"));
    }
    if schema.contains('.') {
        return Err(anyhow!("schema name '{}' must not contain '.'", schema));
    }
    Ok(())
}

pub(crate) fn validate_object_ident(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("object name must not be empty"));
    }
    if name.contains('.') {
        return Err(anyhow!("object name '{}' must not contain '.'", name));
    }
    Ok(())
}

pub(crate) fn parse_full_name(full: &str) -> Result<(String, String)> {
    let mut parts = full.splitn(3, '.');
    let schema = parts
        .next()
        .ok_or_else(|| anyhow!("invalid full name '{}'", full))?;
    let name = parts
        .next()
        .ok_or_else(|| anyhow!("invalid full name '{}'", full))?;
    if parts.next().is_some() || schema.is_empty() || name.is_empty() {
        return Err(anyhow!("invalid full name '{}'", full));
    }
    Ok((schema.to_string(), name.to_string()))
}

pub(crate) fn split_object_name(name: &ObjectName) -> Result<(Option<String>, String)> {
    let parts: Vec<String> = name.0.iter().map(normalize_ident).collect();
    match parts.as_slice() {
        [single] => Ok((None, single.clone())),
        [schema, obj] => Ok((Some(schema.clone()), obj.clone())),
        _ => Err(SqlError::Unsupported(format!("unsupported object name '{}'", name)).into()),
    }
}

pub(crate) fn object_name_from_str(s: &str) -> Result<ObjectName> {
    use sqlparser::ast::Ident;
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty object name"));
    }
    let parts: Vec<&str> = s.split('.').collect();
    match parts.as_slice() {
        [name] => Ok(ObjectName(vec![Ident::new(*name)])),
        [schema, name] => Ok(ObjectName(vec![Ident::new(*schema), Ident::new(*name)])),
        _ => Err(anyhow!("invalid object name '{}'", s)),
    }
}

pub(crate) fn resolve_ddl_object_name(
    name: &ObjectName,
    search_path: &[String],
) -> Result<ResolvedName> {
    let (schema_opt, obj) = split_object_name(name)?;
    let schema = schema_opt.unwrap_or_else(|| default_schema(search_path).to_string());
    ResolvedName::new(schema, obj)
}

fn search_path_schemas(search_path: &[String]) -> Vec<&str> {
    let mut schemas: Vec<&str> = search_path
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.eq_ignore_ascii_case("$user"))
        .collect();
    if schemas.is_empty() {
        schemas.push("public");
    }
    schemas
}

pub(crate) async fn resolve_existing_table_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_schema(txn, db_id, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store
                    .get_schema(txn, db_id, &resolved.full)
                    .await?
                    .is_some()
                {
                    return Ok(Some(resolved));
                }
            }
            Ok(None)
        }
    }
}

pub(crate) async fn resolve_existing_view_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_view(txn, db_id, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store.get_view(txn, db_id, &resolved.full).await?.is_some() {
                    return Ok(Some(resolved));
                }
            }
            Ok(None)
        }
    }
}

pub(crate) async fn resolve_existing_materialized_view_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_materialized_view(txn, db_id, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store
                    .get_materialized_view(txn, db_id, &resolved.full)
                    .await?
                    .is_some()
                {
                    return Ok(Some(resolved));
                }
            }
            Ok(None)
        }
    }
}

pub(crate) async fn resolve_existing_procedure_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_procedure(txn, db_id, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store
                    .get_procedure(txn, db_id, &resolved.full)
                    .await?
                    .is_some()
                {
                    return Ok(Some(resolved));
                }
            }
            Ok(None)
        }
    }
}

pub(crate) async fn resolve_existing_function_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_function(txn, db_id, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store
                    .get_function(txn, db_id, &resolved.full)
                    .await?
                    .is_some()
                {
                    return Ok(Some(resolved));
                }
            }
            Ok(None)
        }
    }
}

pub(crate) async fn resolve_existing_sequence_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_sequence(txn, db_id, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store
                    .get_sequence(txn, db_id, &resolved.full)
                    .await?
                    .is_some()
                {
                    return Ok(Some(resolved));
                }
            }
            Ok(None)
        }
    }
}

pub(crate) async fn resolve_existing_type_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_type(txn, db_id, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store.get_type(txn, db_id, &resolved.full).await?.is_some() {
                    return Ok(Some(resolved));
                }
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Ident;

    fn ident(v: &str) -> Ident {
        Ident::new(v)
    }

    #[test]
    fn resolve_ddl_object_name_defaults_to_public() {
        let name = ObjectName(vec![ident("t")]);
        let resolved = resolve_ddl_object_name(&name, &[]).unwrap();
        assert_eq!(resolved.schema, "public");
        assert_eq!(resolved.name, "t");
        assert_eq!(resolved.full, "public.t");
    }

    #[test]
    fn resolve_ddl_object_name_uses_search_path_head() {
        let name = ObjectName(vec![ident("t")]);
        let resolved =
            resolve_ddl_object_name(&name, &["app".to_string(), "public".to_string()]).unwrap();
        assert_eq!(resolved.full, "app.t");
    }

    #[test]
    fn resolve_ddl_object_name_skips_user_placeholder_head() {
        let name = ObjectName(vec![ident("t")]);
        let resolved =
            resolve_ddl_object_name(&name, &["$user".to_string(), "public".to_string()]).unwrap();
        assert_eq!(resolved.full, "public.t");
    }

    #[test]
    fn resolve_ddl_object_name_explicit_schema() {
        let name = ObjectName(vec![ident("app"), ident("t")]);
        let resolved = resolve_ddl_object_name(&name, &["public".to_string()]).unwrap();
        assert_eq!(resolved.full, "app.t");
    }

    #[test]
    fn parse_full_name_rejects_invalid() {
        assert!(parse_full_name("a").is_err());
        assert!(parse_full_name("a.b.c").is_err());
        assert!(parse_full_name("a.").is_err());
        assert!(parse_full_name(".b").is_err());
    }
}
