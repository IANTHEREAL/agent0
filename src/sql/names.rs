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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedTypeName {
    Builtin { resolved: ResolvedName, oid: i64 },
    UserDefined { resolved: ResolvedName, oid: i64 },
}

impl ResolvedTypeName {
    pub(crate) fn resolved_name(&self) -> &ResolvedName {
        match self {
            Self::Builtin { resolved, .. } | Self::UserDefined { resolved, .. } => resolved,
        }
    }

    pub(crate) fn is_builtin(&self) -> bool {
        matches!(self, Self::Builtin { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedRegclassInput {
    pub(crate) database: Option<String>,
    pub(crate) schema: Option<String>,
    pub(crate) name: String,
}

impl ParsedRegclassInput {
    pub(crate) fn relation_lookup_parts(&self) -> (Option<&str>, &str) {
        (self.schema.as_deref(), self.name.as_str())
    }

    pub(crate) fn is_current_database(&self, current_database: &str) -> bool {
        match self.database.as_deref() {
            Some(database) => database == current_database,
            None => true,
        }
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

fn implicit_pg_catalog_search_path_schemas(search_path: &[String]) -> Vec<String> {
    let mut schemas: Vec<String> = search_path_schemas(search_path)
        .into_iter()
        .map(str::to_string)
        .collect();

    if !schemas.iter().any(|s| s == "pg_catalog") {
        schemas.insert(0, "pg_catalog".to_string());
    }

    schemas
}

/// PostgreSQL relation lookup order.
///
/// `pg_catalog` is implicitly searched first unless explicitly listed in the
/// search_path. When no explicit schema remains after filtering `$user`,
/// PostgreSQL still treats `public` as the default user schema.
pub(crate) fn relation_search_path_schemas(search_path: &[String]) -> Vec<String> {
    implicit_pg_catalog_search_path_schemas(search_path)
}

/// PostgreSQL type lookup order for regtype visibility.
///
/// Like relation lookup, type names implicitly search `pg_catalog` first
/// unless it appears explicitly in `search_path`.
pub(crate) fn type_search_path_schemas(search_path: &[String]) -> Vec<String> {
    implicit_pg_catalog_search_path_schemas(search_path)
}

pub(crate) async fn resolve_existing_relation_oid(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema_opt: Option<&str>,
    name: &str,
    search_path: &[String],
) -> Result<Option<i64>> {
    let schemas: Vec<String> = match schema_opt {
        Some(schema) => vec![schema.to_string()],
        None => relation_search_path_schemas(search_path),
    };
    let all_tables = store.list_tables(txn, db_id).await?;

    for schema in &schemas {
        if let Some(oid) = crate::sql::catalog::catalog_relation_oid(schema, name) {
            return Ok(Some(oid));
        }

        let full = format!("{}.{}", schema, name);

        if let Some(table_schema) = store.get_schema(txn, db_id, &full).await? {
            let oid = crate::sql::catalog_oids::pg_class_table_oid(table_schema.table_id)?;
            return Ok(Some(oid));
        }

        if let Some(view_def) = store.get_view(txn, db_id, &full).await? {
            return Ok(Some(crate::sql::catalog_oids::pg_class_view_oid(
                view_def.oid,
            )));
        }

        if let Some(seq_def) = store.get_sequence(txn, db_id, &full).await? {
            return Ok(Some(crate::sql::catalog_oids::pg_class_sequence_oid(
                seq_def.oid,
            )));
        }

        for table_full in &all_tables {
            if !table_full.starts_with(schema.as_str())
                || table_full.as_bytes().get(schema.len()) != Some(&b'.')
            {
                continue;
            }
            if let Some(table_schema) = store.get_schema(txn, db_id, table_full).await? {
                for idx in &table_schema.indexes {
                    if idx.name == name {
                        let oid = crate::sql::catalog_oids::pg_class_index_oid(
                            table_schema.table_id,
                            idx.id,
                        )?;
                        return Ok(Some(oid));
                    }
                }

                if !table_schema.pk_indices.is_empty() {
                    let (_, table_name) = parse_full_name(table_full).unwrap_or_default();
                    let pk_name = table_schema
                        .pk_constraint_name
                        .clone()
                        .unwrap_or_else(|| format!("{}_pkey", table_name));
                    if pk_name == name {
                        let oid =
                            crate::sql::catalog_oids::pg_class_pk_index_oid(table_schema.table_id)?;
                        return Ok(Some(oid));
                    }
                }
            }
        }
    }

    Ok(None)
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
) -> Result<Option<ResolvedTypeName>> {
    let (schema_opt, obj) = split_object_name(name)?;
    resolve_existing_type_full_name(store, txn, db_id, schema_opt.as_deref(), &obj, search_path)
        .await
}

pub(crate) async fn resolve_type_in_schema(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema: &str,
    name: &str,
) -> Result<Option<ResolvedTypeName>> {
    if schema == "pg_catalog" {
        if let Some(oid) = crate::sql::pg_types::visible_pg_catalog_regtype_oid(name) {
            return Ok(Some(ResolvedTypeName::Builtin {
                resolved: ResolvedName::new(schema.to_string(), name.to_string())?,
                oid,
            }));
        }
    }

    let resolved = ResolvedName::new(schema.to_string(), name.to_string())?;
    let Some(def) = store.get_type(txn, db_id, &resolved.full).await? else {
        return Ok(None);
    };

    Ok(Some(ResolvedTypeName::UserDefined {
        resolved,
        oid: def.oid as i64,
    }))
}

pub(crate) async fn resolve_existing_type_full_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema_opt: Option<&str>,
    name: &str,
    search_path: &[String],
) -> Result<Option<ResolvedTypeName>> {
    let schemas: Vec<String> = match schema_opt {
        Some(schema) => vec![schema.to_string()],
        None => type_search_path_schemas(search_path),
    };

    for schema in schemas {
        if let Some(resolved) = resolve_type_in_schema(store, txn, db_id, &schema, name).await? {
            return Ok(Some(resolved));
        }
    }

    Ok(None)
}

pub(crate) async fn resolve_visible_type_full_name(
    store: &crate::storage::TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema_opt: Option<&str>,
    name: &str,
    search_path: &[String],
) -> Result<Option<ResolvedTypeName>> {
    resolve_existing_type_full_name(store, txn, db_id, schema_opt, name, search_path).await
}

/// Parse a `to_regclass(text)` input string into database/schema/name parts.
///
/// Handles PostgreSQL identifier quoting rules:
/// - Unquoted identifiers are lowercased via [`normalize_ident_str`].
/// - Quoted identifiers (`"Foo"`) preserve case; `""` inside quotes is an
///   escaped double-quote character.
/// - A dot inside `"..."` is literal (not a separator).
///
/// Returns `Err(input)` for four-or-more part names.
pub(crate) fn parse_regclass_input(input: &str) -> Result<ParsedRegclassInput, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(ParsedRegclassInput {
            database: None,
            schema: None,
            name: String::new(),
        });
    }

    let parts = split_dotted_ident(trimmed);
    match parts.as_slice() {
        [single] => Ok(ParsedRegclassInput {
            database: None,
            schema: None,
            name: normalize_ident_part(single),
        }),
        [schema, name] => Ok(ParsedRegclassInput {
            database: None,
            schema: Some(normalize_ident_part(schema)),
            name: normalize_ident_part(name),
        }),
        [database, schema, name] => Ok(ParsedRegclassInput {
            database: Some(normalize_ident_part(database)),
            schema: Some(normalize_ident_part(schema)),
            name: normalize_ident_part(name),
        }),
        _ => Err(trimmed.to_string()),
    }
}

/// Split a dotted identifier string respecting double-quoted segments.
fn split_dotted_ident(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            // Skip to closing quote (handle escaped "")
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        i += 2; // escaped double-quote
                    } else {
                        i += 1; // closing quote
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if bytes[i] == b'.' {
            parts.push(&s[start..i]);
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Normalize a single identifier part: strip quotes and unescape, or lowercase.
fn normalize_ident_part(part: &str) -> String {
    let trimmed = part.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() > 1 {
        // Quoted: strip outer quotes and unescape internal ""
        trimmed[1..trimmed.len() - 1].replace("\"\"", "\"")
    } else {
        trimmed.to_lowercase()
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

    #[test]
    fn parse_regclass_input_unqualified_lowercased() {
        let parsed = parse_regclass_input("MyTable").unwrap();
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.schema, None);
        assert_eq!(parsed.name, "mytable");
    }

    #[test]
    fn parse_regclass_input_quoted_preserves_case() {
        let parsed = parse_regclass_input("\"MixedCase\"").unwrap();
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.schema, None);
        assert_eq!(parsed.name, "MixedCase");
    }

    #[test]
    fn parse_regclass_input_schema_qualified() {
        let parsed = parse_regclass_input("public.my_table").unwrap();
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.schema, Some("public".to_string()));
        assert_eq!(parsed.name, "my_table");
    }

    #[test]
    fn parse_regclass_input_quoted_schema_qualified() {
        let parsed = parse_regclass_input("\"MySchema\".\"MyTable\"").unwrap();
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.schema, Some("MySchema".to_string()));
        assert_eq!(parsed.name, "MyTable");
    }

    #[test]
    fn parse_regclass_input_database_qualified() {
        let parsed = parse_regclass_input("postgres.public.my_table").unwrap();
        assert_eq!(parsed.database, Some("postgres".to_string()));
        assert_eq!(parsed.schema, Some("public".to_string()));
        assert_eq!(parsed.name, "my_table");
        assert!(parsed.is_current_database("postgres"));
        assert!(!parsed.is_current_database("otherdb"));
    }

    #[test]
    fn parse_regclass_input_quoted_database_qualified() {
        let parsed = parse_regclass_input("\"MyDb\".\"MySchema\".\"MyTable\"").unwrap();
        assert_eq!(parsed.database, Some("MyDb".to_string()));
        assert_eq!(parsed.schema, Some("MySchema".to_string()));
        assert_eq!(parsed.name, "MyTable");
    }

    #[test]
    fn parse_regclass_input_escaped_double_quote() {
        let parsed = parse_regclass_input("\"has\"\"quote\"").unwrap();
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.schema, None);
        assert_eq!(parsed.name, "has\"quote");
    }

    #[test]
    fn parse_regclass_input_dot_inside_quoted_name() {
        let parsed = parse_regclass_input("\"schema.table\"").unwrap();
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.schema, None);
        assert_eq!(parsed.name, "schema.table");
    }

    #[test]
    fn parse_regclass_input_four_parts_is_invalid() {
        let err = parse_regclass_input("a.b.c.d").unwrap_err();
        assert_eq!(err, "a.b.c.d");
    }

    #[test]
    fn relation_search_path_schemas_prepends_implicit_pg_catalog() {
        let schemas = relation_search_path_schemas(&["$user".to_string(), "public".to_string()]);
        assert_eq!(schemas, vec!["pg_catalog", "public"]);
    }

    #[test]
    fn relation_search_path_schemas_preserves_explicit_pg_catalog_position() {
        let schemas = relation_search_path_schemas(&[
            "$user".to_string(),
            "public".to_string(),
            "pg_catalog".to_string(),
        ]);
        assert_eq!(schemas, vec!["public", "pg_catalog"]);
    }

    #[test]
    fn relation_search_path_schemas_defaults_to_pg_catalog_then_public() {
        let schemas = relation_search_path_schemas(&[]);
        assert_eq!(schemas, vec!["pg_catalog", "public"]);
    }

    #[test]
    fn type_search_path_schemas_prepends_implicit_pg_catalog() {
        let schemas = type_search_path_schemas(&["$user".to_string(), "public".to_string()]);
        assert_eq!(schemas, vec!["pg_catalog", "public"]);
    }

    #[test]
    fn type_search_path_schemas_preserves_explicit_pg_catalog_position() {
        let schemas = type_search_path_schemas(&[
            "$user".to_string(),
            "public".to_string(),
            "pg_catalog".to_string(),
        ]);
        assert_eq!(schemas, vec!["public", "pg_catalog"]);
    }
}
