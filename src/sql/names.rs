use super::helpers::normalize_ident;
use anyhow::{anyhow, Result};
use sqlparser::ast::ObjectName;
use tikv_client::Transaction;

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
    search_path.first().map(|s| s.as_str()).unwrap_or("public")
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
        _ => Err(anyhow!("unsupported object name '{}'", name)),
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

fn search_path_schemas<'a>(search_path: &'a [String]) -> Vec<&'a str> {
    if search_path.is_empty() {
        return vec!["public"];
    }
    search_path.iter().map(|s| s.as_str()).collect()
}

pub(crate) async fn resolve_existing_table_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_schema(txn, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store.get_schema(txn, &resolved.full).await?.is_some() {
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
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store.get_view(txn, &resolved.full).await?.map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store.get_view(txn, &resolved.full).await?.is_some() {
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
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_materialized_view(txn, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store
                    .get_materialized_view(txn, &resolved.full)
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
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_procedure(txn, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store.get_procedure(txn, &resolved.full).await?.is_some() {
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
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store
                .get_sequence(txn, &resolved.full)
                .await?
                .map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store.get_sequence(txn, &resolved.full).await?.is_some() {
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
    name: &ObjectName,
    search_path: &[String],
) -> Result<Option<ResolvedName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    match schema_opt {
        Some(schema) => {
            let resolved = ResolvedName::new(schema, obj)?;
            Ok(store.get_type(txn, &resolved.full).await?.map(|_| resolved))
        }
        None => {
            for schema in search_path_schemas(search_path) {
                let resolved = ResolvedName::new(schema.to_string(), obj.clone())?;
                if store.get_type(txn, &resolved.full).await?.is_some() {
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
