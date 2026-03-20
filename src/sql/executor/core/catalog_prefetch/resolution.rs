//! Table, view, and function resolution through the search path, plus view
//! schema inference via the Analyzer.

use crate::model::{ColumnDef, DataType, Row, TableSchema, ViewDef};
use crate::sql::analyzer::{Analyzer, CatalogSnapshot};
use crate::sql::executor::table_utils::create_sequence_state_table_schema;
use crate::sql::names;
use crate::sql::table_functions::infer_system_virtual_table_function_schema;
use crate::storage::TikvStore;
use anyhow::Result;
use sqlparser::ast::{Expr, ObjectName, Query, Value as SqlValue};
use std::collections::{HashMap, HashSet};
use tikv_client::Transaction;

use super::extraction::{
    extract_scalar_function_names, extract_table_function_calls, extract_table_names,
    extract_type_names,
};

fn literal_expr_to_text(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Value(SqlValue::SingleQuotedString(s))
        | Expr::Value(SqlValue::DoubleQuotedString(s)) => Some(s.clone()),
        Expr::Cast { expr, .. } => literal_expr_to_text(expr),
        _ => None,
    }
}

fn literal_expr_to_bool(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Value(SqlValue::Boolean(b)) => Some(*b),
        Expr::Value(SqlValue::SingleQuotedString(s))
        | Expr::Value(SqlValue::DoubleQuotedString(s)) => match s.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" => Some(true),
            "false" | "f" | "0" | "no" | "n" => Some(false),
            _ => None,
        },
        Expr::Cast { expr, .. } => literal_expr_to_bool(expr),
        _ => None,
    }
}

fn literal_expr_to_char(expr: &Expr) -> Option<char> {
    let s = literal_expr_to_text(expr)?;
    let mut chars = s.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(ch)
}

#[cfg(feature = "parquet")]
fn parquet_fallback_table_function_schema() -> TableSchema {
    // Keep planning deterministic even when prefetch-time schema inference cannot
    // reach/inspect the parquet source. Runtime execution still enforces extension
    // install state and emits the canonical user-facing error.
    TableSchema::new("read_parquet".to_string(), 0, Vec::new(), Vec::new())
}

/// Try to resolve a table name through the search path and fetch its schema.
///
/// Returns `(bare_or_qualified_name, fully_qualified_name, schema)` on success.
pub(super) async fn try_resolve_table(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &str,
) -> Result<Option<(String, String, TableSchema)>> {
    // Parse the name into optional schema + table.
    let (schema_part, table_part) = if let Some(dot_pos) = name.find('.') {
        (Some(&name[..dot_pos]), &name[dot_pos + 1..])
    } else {
        (None, name)
    };

    if let Some(schema_name) = schema_part {
        // Schema-qualified: try "schema.table" directly.
        let full = format!("{}.{}", schema_name, table_part);
        if let Some(table_schema) = store.get_schema(txn, db_id, &full).await? {
            return Ok(Some((full.clone(), full, table_schema)));
        }
    } else {
        // Unqualified: try bare name first, then search path.
        if let Some(table_schema) = store.get_schema(txn, db_id, table_part).await? {
            return Ok(Some((
                table_part.to_string(),
                table_part.to_string(),
                table_schema,
            )));
        }
        for sp in search_path {
            let full = format!("{}.{}", sp, table_part);
            if let Some(table_schema) = store.get_schema(txn, db_id, &full).await? {
                return Ok(Some((table_part.to_string(), full, table_schema)));
            }
        }
    }

    Ok(None)
}

/// Try to resolve a view name through the search path.
///
/// Returns `(fully_qualified_name, ViewDef)` on success.
async fn try_resolve_view(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &str,
) -> Result<Option<(String, ViewDef)>> {
    let (schema_part, table_part) = if let Some(dot_pos) = name.find('.') {
        (Some(&name[..dot_pos]), &name[dot_pos + 1..])
    } else {
        (None, name)
    };

    if let Some(schema_name) = schema_part {
        let full = format!("{}.{}", schema_name, table_part);
        if let Some(view_def) = store.get_view(txn, db_id, &full).await? {
            return Ok(Some((full, view_def)));
        }
    } else {
        // Unqualified: try bare name first, then search path.
        if let Some(view_def) = store.get_view(txn, db_id, table_part).await? {
            return Ok(Some((table_part.to_string(), view_def)));
        }
        for sp in search_path {
            let full = format!("{}.{}", sp, table_part);
            if let Some(view_def) = store.get_view(txn, db_id, &full).await? {
                return Ok(Some((full, view_def)));
            }
        }
    }

    Ok(None)
}

/// Try to resolve a view and derive its output schema as a synthetic TableSchema.
///
/// This parses the view's SQL, recursively builds a catalog snapshot for the
/// view's dependencies, and uses the Analyzer to derive the output schema.
pub(super) async fn try_resolve_view_as_table(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    tenant_keyspace: &str,
    name: &str,
    ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    expanding_views: &mut HashSet<String>,
) -> Result<Option<(String, TableSchema)>> {
    let Some((resolved_full, view_def)) =
        try_resolve_view(store, txn, db_id, search_path, name).await?
    else {
        return Ok(None);
    };

    // Cycle detection: if we're already expanding this view, skip it.
    if !expanding_views.insert(resolved_full.clone()) {
        return Err(anyhow::anyhow!(
            "recursive view definition: \"{}\"",
            resolved_full
        ));
    }

    let result = resolve_view_output_schema(
        store,
        txn,
        db_id,
        search_path,
        tenant_keyspace,
        &resolved_full,
        &view_def,
        ctes,
        expanding_views,
    )
    .await;

    // Always remove from expanding set (even on error) to clean up state.
    expanding_views.remove(&resolved_full);

    Ok(Some((resolved_full, result?)))
}

/// Parse a view's SQL query and derive its output schema using the Analyzer.
async fn resolve_view_output_schema(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    tenant_keyspace: &str,
    view_full_name: &str,
    view_def: &ViewDef,
    ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    expanding_views: &mut HashSet<String>,
) -> Result<TableSchema> {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    // Parse the view's stored query.
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, &view_def.query)
        .map_err(|e| anyhow::anyhow!("failed to parse view '{}' query: {}", view_full_name, e))?;

    let query = match stmts.into_iter().next() {
        Some(sqlparser::ast::Statement::Query(q)) => q,
        _ => {
            return Err(anyhow::anyhow!(
                "view '{}' does not contain a SELECT query",
                view_full_name
            ));
        }
    };

    // Recursively build a catalog snapshot for the view's dependencies.
    // Box::pin breaks the infinite-size future from async recursion.
    let inner_snapshot = Box::pin(build_catalog_snapshot_inner(
        store,
        txn,
        db_id,
        search_path,
        tenant_keyspace,
        &query,
        ctes,
        expanding_views,
    ))
    .await?;

    // Run the Analyzer to derive the output schema.
    let mut analyzer = Analyzer::new(&inner_snapshot);
    let analyzed = analyzer
        .analyze_query(&query)
        .map_err(|e| anyhow::anyhow!("failed to analyze view '{}': {}", view_full_name, e))?;

    // Build a synthetic TableSchema from the analyzed output schema.
    let columns: Vec<ColumnDef> = analyzed
        .output_schema
        .iter()
        .map(|(col_name, data_type, _coll)| ColumnDef {
            name: col_name.clone(),
            data_type: data_type.clone(),
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        })
        .collect();

    Ok(TableSchema {
        name: view_full_name.to_string(),
        table_id: 0, // synthetic -- views have no storage table_id
        columns,
        version: 0,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    })
}

pub(super) async fn try_resolve_function_def(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
) -> Result<Option<(String, crate::model::FunctionDef)>> {
    let resolved =
        match names::resolve_existing_function_name(store, txn, db_id, name, search_path).await {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
    let Some(resolved) = resolved else {
        return Ok(None);
    };

    let Some(func_def) = store.get_function(txn, db_id, &resolved.full).await? else {
        return Ok(None);
    };

    Ok(Some((resolved.full, func_def)))
}

async fn infer_setof_table_schema(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    return_type: &str,
) -> Result<Option<TableSchema>> {
    let ret_lower = return_type.to_lowercase();

    if let Some(schema) = infer_returns_table_schema(&ret_lower) {
        return Ok(Some(schema));
    }

    let Some(target) = ret_lower.strip_prefix("setof").map(str::trim) else {
        return Ok(None);
    };

    if target.is_empty() || target == "record" {
        return Ok(None);
    }

    let schema = try_resolve_table(store, txn, db_id, search_path, target)
        .await?
        .map(|(_, _, schema)| schema);
    Ok(schema)
}

fn infer_returns_table_schema(ret_lower: &str) -> Option<TableSchema> {
    use crate::model::DataType;

    let inner = ret_lower
        .strip_prefix("table")
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))?;

    let mut cols = Vec::new();
    for part in inner.split(',') {
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() < 2 {
            return None;
        }
        let col_name = tokens[0].to_string();
        let type_str = tokens[1..].join(" ").to_uppercase();
        let dt = match type_str.as_str() {
            "BOOL" | "BOOLEAN" => DataType::Boolean,
            "INT" | "INTEGER" | "INT4" | "SMALLINT" | "INT2" => DataType::Int32,
            "BIGINT" | "INT8" => DataType::Int64,
            "REAL" | "FLOAT4" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" | "FLOAT" => {
                DataType::Float64
            }
            "TEXT" | "CHAR" | "CHARACTER" => DataType::Text,
            "VARCHAR" | "CHARACTER VARYING" => DataType::Varchar(0),
            "NUMERIC" | "DECIMAL" => DataType::Numeric {
                precision: None,
                scale: None,
            },
            "DATE" => DataType::Date,
            "TIME" => DataType::Time,
            "TIMESTAMP" | "TIMESTAMP WITHOUT TIME ZONE" => DataType::Timestamp,
            "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" => DataType::TimestampTz,
            "INTERVAL" => DataType::Interval,
            "UUID" => DataType::Uuid,
            "BYTEA" => DataType::Bytes,
            "JSON" => DataType::Json,
            "JSONB" => DataType::Jsonb,
            "TSVECTOR" => DataType::Tsvector,
            "TSQUERY" => DataType::Tsquery,
            s if s.starts_with("VECTOR") => {
                let dim = s
                    .strip_prefix("VECTOR")
                    .and_then(|r| r.trim().strip_prefix('('))
                    .and_then(|r| r.strip_suffix(')'))
                    .and_then(|r| r.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                DataType::Vector(dim)
            }
            _ => DataType::Text,
        };
        cols.push(ColumnDef {
            name: col_name,
            data_type: dt,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        });
    }

    if cols.is_empty() {
        return None;
    }

    Some(TableSchema {
        table_id: 0,
        name: String::new(),
        columns: cols,
        indexes: vec![],
        ..Default::default()
    })
}

pub(super) async fn prefetch_scalar_functions(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    function_names: &[ObjectName],
    snapshot: &mut CatalogSnapshot,
) -> Result<()> {
    // Track schema qualifiers we've already checked to avoid redundant lookups.
    let mut checked_schemas: HashSet<String> = HashSet::new();

    for func_name in function_names {
        // For schema-qualified function calls (e.g. s1.my_func), record
        // whether the schema exists so the Analyzer can distinguish
        // "function not found in existing schema" (42883) from
        // "schema does not exist" (3F000).
        if func_name.0.len() >= 2 {
            let schema = names::normalize_ident(&func_name.0[0]);
            if checked_schemas.insert(schema.clone())
                && store.schema_exists(txn, db_id, &schema).await?
            {
                snapshot.add_schema(&schema);
            }
        }

        let Some((resolved_full, func_def)) =
            try_resolve_function_def(store, txn, db_id, search_path, func_name).await?
        else {
            continue;
        };

        // Populate both qualified and bare aliases. The Analyzer currently
        // resolves function identifiers as unqualified names.
        snapshot.add_function(&resolved_full, func_def.clone());
        let bare_name = resolved_full
            .rsplit('.')
            .next()
            .unwrap_or(&resolved_full)
            .to_string();
        snapshot.add_function(&bare_name, func_def);
    }

    Ok(())
}

pub(super) async fn prefetch_type_references(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    type_names: &[ObjectName],
    snapshot: &mut CatalogSnapshot,
) -> Result<()> {
    let mut seen: HashSet<String> = HashSet::new();

    for type_name in type_names {
        let resolved = match names::resolve_existing_type_name(
            store,
            txn,
            db_id,
            type_name,
            search_path,
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(_) => continue,
        };
        let Some(resolved) = resolved else {
            continue;
        };
        if resolved.is_builtin() {
            continue;
        }

        let full_name = resolved.resolved_name().full.clone();
        if !seen.insert(full_name.clone()) {
            continue;
        }

        let Some(def) = store.get_type(txn, db_id, &full_name).await? else {
            continue;
        };
        snapshot.add_type(&full_name, def);
    }

    Ok(())
}

pub(super) async fn prefetch_table_function_schemas(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    tenant_keyspace: &str,
    query: &Query,
    snapshot: &mut CatalogSnapshot,
) -> Result<()> {
    use crate::extensions::{embedding, fs, http, EXTENSIONS_SCHEMA};
    use sqlparser::ast::{FunctionArg, FunctionArgExpr};

    let mut seen: HashSet<String> = HashSet::new();
    for call in extract_table_function_calls(query) {
        if !seen.insert(call.key.clone()) {
            continue;
        }

        if let Some(schema) = infer_system_virtual_table_function_schema(&call.name, &call.args) {
            snapshot.add_table_function(&call.key, schema);
            continue;
        }

        // Determine schema/function name.
        let (schema_opt, func_name) = match call.name_parts.as_slice() {
            [name] => (None, name.as_str()),
            [schema, name] => (Some(schema.as_str()), name.as_str()),
            _ => continue, // deeper qualification not supported
        };

        let is_extensions_schema = match schema_opt {
            Some(schema) => schema.eq_ignore_ascii_case(EXTENSIONS_SCHEMA),
            None => search_path
                .iter()
                .any(|s| s.eq_ignore_ascii_case(EXTENSIONS_SCHEMA)),
        };

        let func_lower = func_name.to_ascii_lowercase();

        if is_extensions_schema {
            // Prefetch schemas for known extension table functions.
            if func_lower == "embedding_usage" {
                snapshot.add_table_function(&call.key, embedding::embedding_usage_table_schema());
                continue;
            }

            if let Some(schema) = http::table_function_schema(&func_lower) {
                snapshot.add_table_function(&call.key, schema);
                continue;
            }

            if func_lower == "fs9" {
                // Require fs9 extension installed+enabled to expose schema.
                let installed = store.get_extension(txn, db_id, "fs9").await?;
                let Some(installed) = installed else {
                    continue;
                };
                if !installed.enabled {
                    continue;
                }

                // Parse args (constants only).
                let path = match call.args.first() {
                    Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => literal_expr_to_text(e),
                    _ => None,
                };
                let Some(path) = path else {
                    continue;
                };

                let mut format: Option<String> = None;
                let mut delimiter: Option<char> = None;
                let mut header: Option<bool> = None;
                let mut recursive: Option<bool> = None;
                let mut exclude: Option<String> = None;

                for arg in call.args.iter().skip(1) {
                    let FunctionArg::Named {
                        name,
                        arg: FunctionArgExpr::Expr(e),
                        ..
                    } = arg
                    else {
                        continue;
                    };
                    let param = names::normalize_ident(name).to_ascii_lowercase();
                    match param.as_str() {
                        "format" => {
                            format = literal_expr_to_text(e);
                        }
                        "delimiter" => {
                            delimiter = literal_expr_to_char(e);
                        }
                        "header" => {
                            header = literal_expr_to_bool(e);
                        }
                        "recursive" => {
                            recursive = literal_expr_to_bool(e);
                        }
                        "exclude" => {
                            exclude = literal_expr_to_text(e);
                        }
                        _ => {}
                    }
                }

                let has_glob = path.contains('*') || path.contains('?') || path.contains('[');
                let mode = if has_glob {
                    fs::Fs9Mode::Glob {
                        pattern: path,
                        format,
                        delimiter,
                        header,
                        exclude,
                    }
                } else if recursive == Some(true) {
                    fs::Fs9Mode::Directory {
                        path,
                        recursive: true,
                        exclude,
                    }
                } else {
                    fs::Fs9Mode::File {
                        path,
                        format,
                        delimiter,
                        header,
                    }
                };

                let schema = fs::infer_table_function_schema(tenant_keyspace, &mode).await?;
                snapshot.add_table_function(&call.key, schema);
                continue;
            }

            #[cfg(feature = "parquet")]
            if func_lower == "read_parquet" {
                let fallback_schema = parquet_fallback_table_function_schema();
                let installed = store.get_extension(txn, db_id, "parquet").await?;
                let Some(installed) = installed else {
                    snapshot.add_table_function(&call.key, fallback_schema);
                    continue;
                };
                if !installed.enabled {
                    snapshot.add_table_function(&call.key, fallback_schema);
                    continue;
                }
                let url = match call.args.first() {
                    Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => literal_expr_to_text(e),
                    _ => None,
                };
                let Some(url) = url else {
                    snapshot.add_table_function(&call.key, fallback_schema);
                    continue;
                };
                match crate::extensions::parquet::reader::infer_schema(&url).await {
                    Ok(schema) => {
                        snapshot.add_table_function(&call.key, schema);
                    }
                    Err(e) => {
                        tracing::warn!("read_parquet schema inference failed for {url}: {e}");
                        snapshot.add_table_function(&call.key, fallback_schema);
                    }
                }
                continue;
            }
        }

        // User-defined table functions: support RETURNS SETOF <table>.
        let Some((_resolved_full, func_def)) =
            try_resolve_function_def(store, txn, db_id, search_path, &call.name).await?
        else {
            continue;
        };
        if let Some(schema) =
            infer_setof_table_schema(store, txn, db_id, search_path, &func_def.return_type).await?
        {
            snapshot.add_table_function(&call.key, schema);
        }
    }

    Ok(())
}

/// Extract `UserDefined` type names from a table schema's column definitions.
fn extract_udt_names_from_schema(schema: &TableSchema) -> Vec<String> {
    schema
        .columns
        .iter()
        .filter_map(|col| match &col.data_type {
            DataType::UserDefined(name) => Some(name.clone()),
            DataType::Array(inner) => {
                if let DataType::UserDefined(name) = inner.as_ref() {
                    Some(name.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect()
}

/// Fetch and add user-defined types referenced by table columns to the snapshot.
async fn prefetch_column_udts(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    snapshot: &mut CatalogSnapshot,
) -> Result<()> {
    let mut seen = HashSet::new();
    for type_name in extract_udt_names_from_schema(schema) {
        let lower = type_name.to_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        if let Some(def) = store.get_type(txn, db_id, &lower).await? {
            snapshot.add_type(&lower, def);
        }
    }
    Ok(())
}

/// Inner implementation with cycle detection for recursive view expansion.
pub(super) async fn build_catalog_snapshot_inner(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    tenant_keyspace: &str,
    query: &Query,
    ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    expanding_views: &mut HashSet<String>,
) -> Result<CatalogSnapshot> {
    let mut snapshot = CatalogSnapshot::new(search_path.to_vec(), db_id);

    // 1. Add CTEs -- they shadow real tables during analysis.
    //    Mark as non-base so privilege checks don't try to validate them.
    for (cte_name, (cte_schema, _rows)) in ctes {
        snapshot.add_table(cte_name, cte_name.clone(), cte_schema.clone());
        snapshot.mark_non_base(cte_name);
    }

    // 2. Extract all table names referenced in FROM/JOIN/subquery.
    let table_names = extract_table_names(query);

    // 3. Resolve and fetch each table schema (skip CTEs -- already added).
    for raw_name in &table_names {
        let lower = raw_name.to_lowercase();
        if ctes.contains_key(&lower) || ctes.contains_key(raw_name) {
            continue;
        }

        // Resolve table/view/sequence in search_path order (PG relation semantics).
        let relation_candidates: Vec<String> = if raw_name.contains('.') {
            vec![raw_name.to_string()]
        } else if search_path.is_empty() {
            vec![format!("public.{}", raw_name)]
        } else {
            search_path
                .iter()
                .map(|schema| format!("{}.{}", schema, raw_name))
                .collect()
        };

        let mut resolved_relation = false;
        for candidate in &relation_candidates {
            if let Some(table_schema) = store.get_schema(txn, db_id, candidate).await? {
                let alias_name = raw_name.rsplit('.').next().unwrap_or(raw_name).to_string();
                prefetch_column_udts(store, txn, db_id, &table_schema, &mut snapshot).await?;
                snapshot.add_table(&alias_name, candidate.clone(), table_schema.clone());
                snapshot.add_table(raw_name, candidate.clone(), table_schema);
                resolved_relation = true;
                break;
            }

            if let Some((resolved_full, view_schema)) = try_resolve_view_as_table(
                store,
                txn,
                db_id,
                search_path,
                tenant_keyspace,
                candidate,
                ctes,
                expanding_views,
            )
            .await?
            {
                // View resolved: expose to the Analyzer as a table with synthetic schema.
                let alias_name = raw_name.rsplit('.').next().unwrap_or(raw_name).to_string();
                snapshot.add_table(&alias_name, resolved_full.clone(), view_schema.clone());
                snapshot.add_table(raw_name, resolved_full.clone(), view_schema);
                resolved_relation = true;
                break;
            }

            if let Some((def, _state)) = store
                .get_sequence_state_by_name(txn, db_id, &[], candidate)
                .await?
            {
                let resolved_full = def.full_name();
                let schema = create_sequence_state_table_schema(&resolved_full, &def);
                let alias_name = raw_name.rsplit('.').next().unwrap_or(raw_name).to_string();

                snapshot.add_table(&alias_name, resolved_full.clone(), schema.clone());
                snapshot.add_table(raw_name, resolved_full.clone(), schema);
                snapshot.mark_non_base(&resolved_full);
                resolved_relation = true;
                break;
            }
        }
        if resolved_relation {
            continue;
        }

        if let Some(virtual_schema) =
            crate::sql::information_schema::get_information_schema_schema(raw_name)
        {
            // Virtual table (pg_catalog.*, information_schema.*).
            // Mark as non-base: bare names like "pg_tables" lack the pg_catalog. prefix
            // and would otherwise leak into privilege checks.
            let qualified = raw_name.to_string();
            snapshot.add_table(raw_name, qualified.clone(), virtual_schema);
            snapshot.mark_non_base(&qualified);
        } else if let Some(virtual_schema) =
            crate::sql::catalog::virtual_tables::virtual_table_schema(raw_name)
        {
            // _DB9_SYS_* virtual tables (bare table name, no () suffix).
            let qualified = raw_name.to_uppercase();
            snapshot.add_table(raw_name, qualified.clone(), virtual_schema);
            snapshot.mark_non_base(&qualified);
        }
        // If not found, we don't error here -- the Analyzer will produce
        // a proper "table not found" error with context.
    }

    // 4. Prefetch dynamic schemas for table-valued functions in FROM.
    prefetch_table_function_schemas(
        store,
        txn,
        db_id,
        search_path,
        tenant_keyspace,
        query,
        &mut snapshot,
    )
    .await?;

    // 5. Prefetch scalar UDF metadata referenced by expressions.
    let scalar_function_names = extract_scalar_function_names(query);
    prefetch_scalar_functions(
        store,
        txn,
        db_id,
        search_path,
        &scalar_function_names,
        &mut snapshot,
    )
    .await?;

    let type_names = extract_type_names(query);
    prefetch_type_references(store, txn, db_id, search_path, &type_names, &mut snapshot).await?;

    // 6. Prefetch user-defined collations for Analyzer resolution.
    let collation_defs = store.list_collations(txn, db_id).await?;
    for def in collation_defs {
        let name = def.name.clone();
        snapshot.add_collation(&name, def);
    }

    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::{
        infer_returns_table_schema, literal_expr_to_bool, literal_expr_to_char,
        literal_expr_to_text,
    };
    use crate::model::DataType;
    use sqlparser::ast::{DataType as SqlDataType, Expr, Value as SqlValue};

    #[test]
    fn infer_returns_table_schema_parses_common_types() {
        let schema = infer_returns_table_schema(
            "table(id int, name text, created_at timestamp, active boolean)",
        )
        .expect("schema");
        assert_eq!(schema.columns.len(), 4);
        assert_eq!(schema.columns[0].name, "id");
        assert!(matches!(schema.columns[0].data_type, DataType::Int32));
        assert!(matches!(schema.columns[1].data_type, DataType::Text));
        assert!(matches!(schema.columns[2].data_type, DataType::Timestamp));
        assert!(matches!(schema.columns[3].data_type, DataType::Boolean));
    }

    #[test]
    fn infer_returns_table_schema_handles_numeric_temporal_and_vector() {
        let schema = infer_returns_table_schema(
            "table(n numeric, d date, tm time, tz timestamptz, i interval, v vector(3))",
        )
        .expect("schema");
        assert_eq!(schema.columns.len(), 6);
        assert!(matches!(
            schema.columns[0].data_type,
            DataType::Numeric {
                precision: None,
                scale: None
            }
        ));
        assert!(matches!(schema.columns[1].data_type, DataType::Date));
        assert!(matches!(schema.columns[2].data_type, DataType::Time));
        assert!(matches!(schema.columns[3].data_type, DataType::TimestampTz));
        assert!(matches!(schema.columns[4].data_type, DataType::Interval));
        assert!(matches!(schema.columns[5].data_type, DataType::Vector(3)));
    }

    #[test]
    fn infer_returns_table_schema_handles_json_uuid_bytes_fts_and_unknown() {
        let schema = infer_returns_table_schema(
            "table(u uuid, b bytea, j json, jb jsonb, tv tsvector, tq tsquery, x customtype)",
        )
        .expect("schema");
        assert_eq!(schema.columns.len(), 7);
        assert!(matches!(schema.columns[0].data_type, DataType::Uuid));
        assert!(matches!(schema.columns[1].data_type, DataType::Bytes));
        assert!(matches!(schema.columns[2].data_type, DataType::Json));
        assert!(matches!(schema.columns[3].data_type, DataType::Jsonb));
        assert!(matches!(schema.columns[4].data_type, DataType::Tsvector));
        assert!(matches!(schema.columns[5].data_type, DataType::Tsquery));
        // Unknown type currently falls back to TEXT in this inference path.
        assert!(matches!(schema.columns[6].data_type, DataType::Text));
    }

    #[test]
    fn infer_returns_table_schema_rejects_non_table_signature_or_bad_columns() {
        assert!(infer_returns_table_schema("setof public.t").is_none());
        assert!(infer_returns_table_schema("table").is_none());
        assert!(infer_returns_table_schema("table()").is_none());
        assert!(infer_returns_table_schema("table(id)").is_none());
    }

    #[test]
    fn infer_returns_table_schema_covers_type_aliases_and_defaults() {
        let schema = infer_returns_table_schema(
            "table(a integer, b int4, c smallint, d int2, e bigint, f int8, g float4, h float8, i double precision, j varchar, k character varying, l char, m character)",
        )
        .expect("schema");
        assert_eq!(schema.columns.len(), 13);
        for idx in [0usize, 1, 2, 3] {
            assert!(matches!(schema.columns[idx].data_type, DataType::Int32));
        }
        assert!(matches!(schema.columns[4].data_type, DataType::Int64));
        assert!(matches!(schema.columns[5].data_type, DataType::Int64));
        for idx in [6usize, 7, 8] {
            assert!(matches!(schema.columns[idx].data_type, DataType::Float64));
        }
        for idx in [9usize, 10] {
            assert!(matches!(
                schema.columns[idx].data_type,
                DataType::Varchar(0)
            ));
        }
        for idx in [11usize, 12] {
            assert!(matches!(schema.columns[idx].data_type, DataType::Text));
        }
    }

    #[test]
    fn infer_returns_table_schema_vector_dimension_and_invalid_fallback() {
        let schema = infer_returns_table_schema("table(v1 vector(8), v2 vector, v3 vector(x))")
            .expect("schema");
        assert_eq!(schema.columns.len(), 3);
        assert!(matches!(schema.columns[0].data_type, DataType::Vector(8)));
        assert!(matches!(schema.columns[1].data_type, DataType::Vector(0)));
        assert!(matches!(schema.columns[2].data_type, DataType::Vector(0)));
    }

    #[test]
    fn infer_returns_table_schema_expects_lowercase_input_contract() {
        let schema = infer_returns_table_schema("table(id bool, n numeric, ts timestamptz)")
            .expect("schema");
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(schema.columns[0].name, "id");
        assert!(matches!(schema.columns[0].data_type, DataType::Boolean));
        assert!(matches!(
            schema.columns[1].data_type,
            DataType::Numeric {
                precision: None,
                scale: None
            }
        ));
        assert!(matches!(schema.columns[2].data_type, DataType::TimestampTz));

        // Caller contract: input is already normalized to lowercase.
        assert!(infer_returns_table_schema("TABLE(id bool)").is_none());
        assert!(infer_returns_table_schema(" TABLE (id int)").is_none());
    }

    #[test]
    fn literal_expr_to_text_supports_quotes_and_casts() {
        let direct = Expr::Value(SqlValue::SingleQuotedString("abc".to_string()));
        assert_eq!(literal_expr_to_text(&direct), Some("abc".to_string()));

        let casted = Expr::Cast {
            expr: Box::new(Expr::Value(SqlValue::DoubleQuotedString("xyz".to_string()))),
            data_type: SqlDataType::Text,
            format: None,
        };
        assert_eq!(literal_expr_to_text(&casted), Some("xyz".to_string()));

        let non_literal = Expr::Identifier("v".into());
        assert_eq!(literal_expr_to_text(&non_literal), None);
    }

    #[test]
    fn literal_expr_to_bool_accepts_pg_like_literals_and_rejects_others() {
        for (expr, expected) in [
            (Expr::Value(SqlValue::Boolean(true)), Some(true)),
            (
                Expr::Value(SqlValue::SingleQuotedString("YES".to_string())),
                Some(true),
            ),
            (
                Expr::Value(SqlValue::SingleQuotedString("0".to_string())),
                Some(false),
            ),
            (
                Expr::Cast {
                    expr: Box::new(Expr::Value(SqlValue::DoubleQuotedString("n".to_string()))),
                    data_type: SqlDataType::Boolean,
                    format: None,
                },
                Some(false),
            ),
            (
                Expr::Value(SqlValue::SingleQuotedString("maybe".to_string())),
                None,
            ),
        ] {
            assert_eq!(literal_expr_to_bool(&expr), expected, "expr={expr:?}");
        }
    }

    #[test]
    fn literal_expr_to_char_requires_single_character() {
        assert_eq!(
            literal_expr_to_char(&Expr::Value(SqlValue::SingleQuotedString(",".to_string()))),
            Some(',')
        );
        assert_eq!(
            literal_expr_to_char(&Expr::Cast {
                expr: Box::new(Expr::Value(SqlValue::SingleQuotedString("|".to_string()))),
                data_type: SqlDataType::Char(None),
                format: None,
            }),
            Some('|')
        );
        assert_eq!(
            literal_expr_to_char(&Expr::Value(SqlValue::SingleQuotedString("".to_string()))),
            None
        );
        assert_eq!(
            literal_expr_to_char(&Expr::Value(SqlValue::SingleQuotedString("ab".to_string()))),
            None
        );
    }
}
