//! Table, view, and function resolution through the search path, plus view
//! schema inference via the Analyzer.

use crate::sql::analyzer::{Analyzer, CatalogSnapshot};
use crate::sql::names;
use crate::sql::table_functions::infer_system_virtual_table_function_schema;
use crate::storage::TikvStore;
use crate::types::{ColumnDef, Row, TableSchema, ViewDef};
use anyhow::Result;
use sqlparser::ast::{ObjectName, Query};
use std::collections::{HashMap, HashSet};
use tikv_client::Transaction;

use super::extraction::{
    extract_scalar_function_names, extract_table_function_calls, extract_table_names,
};

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
        from_alias: None,
    })
}

pub(super) async fn try_resolve_function_def(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
) -> Result<Option<(String, crate::types::FunctionDef)>> {
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
    use crate::types::DataType;

    let inner = ret_lower
        .strip_prefix("table")
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))?;

    let mut cols = Vec::new();
    for part in inner.split(',') {
        let tokens: Vec<&str> = part.trim().split_whitespace().collect();
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
            "TEXT" | "VARCHAR" | "CHARACTER VARYING" | "CHAR" | "CHARACTER" => DataType::Text,
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
    for func_name in function_names {
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

pub(super) async fn prefetch_table_function_schemas(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    tenant_keyspace: &str,
    query: &Query,
    snapshot: &mut CatalogSnapshot,
) -> Result<()> {
    use crate::extensions::{fs, http, EXTENSIONS_SCHEMA};
    use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, Value as SqlValue};

    fn expr_to_text(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Value(SqlValue::SingleQuotedString(s))
            | Expr::Value(SqlValue::DoubleQuotedString(s)) => Some(s.clone()),
            Expr::Cast { expr, .. } => expr_to_text(expr),
            _ => None,
        }
    }

    fn expr_to_bool(expr: &Expr) -> Option<bool> {
        match expr {
            Expr::Value(SqlValue::Boolean(b)) => Some(*b),
            Expr::Value(SqlValue::SingleQuotedString(s))
            | Expr::Value(SqlValue::DoubleQuotedString(s)) => match s.to_ascii_lowercase().as_str()
            {
                "true" | "t" | "1" | "yes" | "y" => Some(true),
                "false" | "f" | "0" | "no" | "n" => Some(false),
                _ => None,
            },
            Expr::Cast { expr, .. } => expr_to_bool(expr),
            _ => None,
        }
    }

    fn expr_to_char(expr: &Expr) -> Option<char> {
        let s = expr_to_text(expr)?;
        let mut chars = s.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        Some(ch)
    }

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
                    Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => expr_to_text(e),
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
                            format = expr_to_text(e);
                        }
                        "delimiter" => {
                            delimiter = expr_to_char(e);
                        }
                        "header" => {
                            header = expr_to_bool(e);
                        }
                        "recursive" => {
                            recursive = expr_to_bool(e);
                        }
                        "exclude" => {
                            exclude = expr_to_text(e);
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

        if func_lower == "fs9_events" {
            let installed = store.get_extension(txn, db_id, "fs9").await?;
            let Some(installed) = installed else {
                continue;
            };
            if !installed.enabled {
                continue;
            }
            if let Some(schema) = fs::table_function_schema("fs9_events") {
                snapshot.add_table_function(&call.key, schema);
            }
            continue;
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

        // Try to resolve as a real table first.
        if let Some((schema_name, resolved_full, table_schema)) =
            try_resolve_table(store, txn, db_id, search_path, raw_name).await?
        {
            // Add under both bare name and schema-qualified name so the
            // Analyzer can find it either way.
            snapshot.add_table(&schema_name, resolved_full.clone(), table_schema.clone());
            snapshot.add_table(raw_name, resolved_full, table_schema);
        } else if let Some((resolved_full, view_schema)) = try_resolve_view_as_table(
            store,
            txn,
            db_id,
            search_path,
            tenant_keyspace,
            raw_name,
            ctes,
            expanding_views,
        )
        .await?
        {
            // View resolved: expose to the Analyzer as a table with synthetic schema.
            let bare_name = raw_name.rsplit('.').next().unwrap_or(raw_name).to_string();
            snapshot.add_table(&bare_name, resolved_full.clone(), view_schema.clone());
            snapshot.add_table(raw_name, resolved_full, view_schema);
        } else if let Some(virtual_schema) =
            crate::sql::information_schema::get_information_schema_schema(raw_name)
        {
            // Virtual table (pg_catalog.*, information_schema.*).
            // Mark as non-base: bare names like "pg_tables" lack the pg_catalog. prefix
            // and would otherwise leak into privilege checks.
            let qualified = raw_name.to_string();
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

    // 6. Prefetch user-defined collations for Analyzer resolution.
    let collation_defs = store.list_collations(txn, db_id).await?;
    for def in collation_defs {
        let name = def.name.clone();
        snapshot.add_collation(&name, def);
    }

    Ok(snapshot)
}
