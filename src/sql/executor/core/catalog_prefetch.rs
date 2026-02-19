//! CatalogSnapshot builder: walks a raw sqlparser AST to extract all referenced
//! table names, then batch-fetches schemas from the store to populate a
//! `CatalogSnapshot` for the Analyzer.

use crate::sql::analyzer::{Analyzer, CatalogSnapshot};
use crate::sql::names;
use crate::sql::table_functions::table_function_key;
use crate::storage::TikvStore;
use crate::types::{ColumnDef, Row, TableSchema, ViewDef};
use anyhow::Result;
use sqlparser::ast::{ObjectName, Query, Statement, TableFactor, Visit, Visitor};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use tikv_client::Transaction;

/// Build a `CatalogSnapshot` for the given query by pre-fetching all referenced
/// table schemas from the store.
///
/// This is called before `Analyzer::analyze_query()` to provide the catalog
/// context needed for name resolution and type checking.
pub(crate) async fn build_catalog_snapshot(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    tenant_keyspace: &str,
    query: &Query,
    ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
) -> Result<CatalogSnapshot> {
    let mut expanding_views = HashSet::new();
    build_catalog_snapshot_inner(
        store,
        txn,
        db_id,
        search_path,
        tenant_keyspace,
        query,
        ctes,
        &mut expanding_views,
    )
    .await
}

/// Build a `CatalogSnapshot` for a DML statement (INSERT, UPDATE, DELETE).
///
/// Extracts all table names referenced in the statement and pre-fetches their
/// schemas. For queries, delegates to `build_catalog_snapshot`.
pub(crate) async fn build_catalog_snapshot_for_statement(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    tenant_keyspace: &str,
    stmt: &Statement,
) -> Result<CatalogSnapshot> {
    let table_names = extract_dml_table_names(stmt);
    let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();

    let mut snapshot = CatalogSnapshot::new(search_path.to_vec(), db_id);

    for raw_name in &table_names {
        let lower = raw_name.to_lowercase();
        if snapshot.has_table(&lower) || snapshot.has_table(raw_name) {
            continue;
        }

        if let Some((schema_name, resolved_full, table_schema)) =
            try_resolve_table(store, txn, db_id, search_path, raw_name).await?
        {
            snapshot.add_table(&schema_name, resolved_full.clone(), table_schema.clone());
            snapshot.add_table(raw_name, resolved_full, table_schema);
        } else if let Some(virtual_schema) =
            crate::sql::information_schema::get_information_schema_schema(raw_name)
        {
            snapshot.add_table(raw_name, raw_name.to_string(), virtual_schema);
        }
    }

    // For INSERT ... SELECT, also build snapshot for the subquery.
    if let Statement::Insert {
        source: Some(query),
        ..
    } = stmt
    {
        let sub_snapshot = build_catalog_snapshot(
            store,
            txn,
            db_id,
            search_path,
            tenant_keyspace,
            query,
            &empty_ctes,
        )
        .await?;
        snapshot.merge_from(&sub_snapshot);
    }

    Ok(snapshot)
}

/// Extract all table names referenced in a DML statement.
///
/// Uses the sqlparser Visitor to walk the entire statement AST, capturing
/// table references from FROM, USING, WHERE, SET, VALUES, and subqueries.
fn extract_dml_table_names(stmt: &Statement) -> HashSet<String> {
    let mut collector = TableNameCollector {
        names: HashSet::new(),
    };
    let _ = stmt.visit(&mut collector);
    collector.names
}

/// Inner implementation with cycle detection for recursive view expansion.
async fn build_catalog_snapshot_inner(
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

    // 1. Add CTEs — they shadow real tables during analysis.
    //    Mark as non-base so privilege checks don't try to validate them.
    for (cte_name, (cte_schema, _rows)) in ctes {
        snapshot.add_table(cte_name, cte_name.clone(), cte_schema.clone());
        snapshot.mark_non_base(cte_name);
    }

    // 2. Extract all table names referenced in FROM/JOIN/subquery.
    let table_names = extract_table_names(query);

    // 3. Resolve and fetch each table schema (skip CTEs — already added).
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
        // If not found, we don't error here — the Analyzer will produce
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

    Ok(snapshot)
}

/// Extract all table names from a sqlparser Query AST using the Visitor pattern.
///
/// Returns a deduplicated set of table names as they appear in FROM/JOIN clauses.
/// Names may be bare (`users`) or schema-qualified (`public.users`).
fn extract_table_names(query: &Query) -> HashSet<String> {
    let mut collector = TableNameCollector {
        names: HashSet::new(),
    };
    let _ = query.visit(&mut collector);
    collector.names
}

/// Visitor that collects table names from all relation `ObjectName` nodes.
///
/// Uses `pre_visit_relation` instead of `pre_visit_table_factor` because
/// DML target tables (INSERT INTO t, UPDATE t, DELETE FROM t) are stored as
/// bare `ObjectName` in the AST, not wrapped in `TableFactor::Table`.
/// `pre_visit_relation` visits ALL relation references uniformly.
struct TableNameCollector {
    names: HashSet<String>,
}

impl Visitor for TableNameCollector {
    type Break = ();

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<()> {
        let parts: Vec<String> = relation.0.iter().map(names::normalize_ident).collect();
        let full_name = parts.join(".");
        self.names.insert(full_name);
        ControlFlow::Continue(())
    }
}

#[derive(Debug, Clone)]
struct TableFunctionCall {
    key: String,
    name_parts: Vec<String>,
    args: Vec<sqlparser::ast::FunctionArg>,
}

/// Extract table-valued function calls from a sqlparser Query AST.
fn extract_table_function_calls(query: &Query) -> Vec<TableFunctionCall> {
    let mut collector = TableFunctionCollector { calls: Vec::new() };
    let _ = query.visit(&mut collector);
    collector.calls
}

/// Visitor that collects `TableFactor::Table` nodes that have `args` (i.e. function-in-FROM).
struct TableFunctionCollector {
    calls: Vec<TableFunctionCall>,
}

impl Visitor for TableFunctionCollector {
    type Break = ();

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table {
            name,
            args: Some(args),
            ..
        } = table_factor
        {
            let key = table_function_key(name, args);
            let name_parts: Vec<String> = name.0.iter().map(names::normalize_ident).collect();
            self.calls.push(TableFunctionCall {
                key,
                name_parts,
                args: args.clone(),
            });
        }
        ControlFlow::Continue(())
    }
}

async fn prefetch_table_function_schemas(
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

        if !is_extensions_schema {
            continue;
        }

        // Only prefetch schemas for known extension table functions.
        let func_lower = func_name.to_ascii_lowercase();

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
async fn try_resolve_view_as_table(
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
        .map(|(col_name, data_type)| ColumnDef {
            name: col_name.clone(),
            data_type: data_type.clone(),
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        })
        .collect();

    Ok(TableSchema {
        name: view_full_name.to_string(),
        table_id: 0, // synthetic — views have no storage table_id
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

/// Try to resolve a table name through the search path and fetch its schema.
///
/// Returns `(bare_or_qualified_name, fully_qualified_name, schema)` on success.
async fn try_resolve_table(
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_stmt(sql: &str) -> Statement {
        let dialect = PostgreSqlDialect {};
        let mut stmts = Parser::parse_sql(&dialect, sql).unwrap();
        stmts.remove(0)
    }

    fn parse_query(sql: &str) -> Query {
        match parse_stmt(sql) {
            Statement::Query(q) => *q,
            other => panic!("expected Query, got {:?}", other),
        }
    }

    // ── extract_dml_table_names (Bug A regression tests) ──────────

    #[test]
    fn test_insert_target_table_is_collected() {
        let stmt = parse_stmt("INSERT INTO users(id, name) VALUES (1, 'alice')");
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("users"),
            "INSERT target 'users' not found in {:?}",
            names
        );
    }

    #[test]
    fn test_insert_schema_qualified_target() {
        let stmt = parse_stmt("INSERT INTO public.users(id) VALUES (1)");
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("public.users"),
            "INSERT target 'public.users' not found in {:?}",
            names
        );
    }

    #[test]
    fn test_insert_select_collects_both_tables() {
        let stmt = parse_stmt("INSERT INTO target SELECT * FROM source");
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("target"),
            "missing INSERT target in {:?}",
            names
        );
        assert!(
            names.contains("source"),
            "missing SELECT source in {:?}",
            names
        );
    }

    #[test]
    fn test_update_target_table_is_collected() {
        let stmt = parse_stmt("UPDATE orders SET status = 'done' WHERE id = 1");
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("orders"),
            "UPDATE target 'orders' not found in {:?}",
            names
        );
    }

    #[test]
    fn test_update_from_collects_both_tables() {
        let stmt = parse_stmt(
            "UPDATE orders SET total = s.amount FROM summaries s WHERE orders.id = s.order_id",
        );
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("orders"),
            "missing UPDATE target in {:?}",
            names
        );
        assert!(
            names.contains("summaries"),
            "missing FROM table in {:?}",
            names
        );
    }

    #[test]
    fn test_delete_target_table_is_collected() {
        let stmt = parse_stmt("DELETE FROM logs WHERE created_at < '2020-01-01'");
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("logs"),
            "DELETE target 'logs' not found in {:?}",
            names
        );
    }

    #[test]
    fn test_delete_using_collects_both_tables() {
        let stmt =
            parse_stmt("DELETE FROM orders USING customers WHERE orders.cust_id = customers.id");
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("orders"),
            "missing DELETE target in {:?}",
            names
        );
        assert!(
            names.contains("customers"),
            "missing USING table in {:?}",
            names
        );
    }

    // ── extract_table_names (SELECT queries) ──────────────────────

    #[test]
    fn test_select_from_single_table() {
        let query = parse_query("SELECT * FROM users");
        let names = extract_table_names(&query);
        assert!(names.contains("users"), "missing table in {:?}", names);
    }

    #[test]
    fn test_select_join_collects_both_tables() {
        let query = parse_query("SELECT * FROM orders o JOIN customers c ON o.cust_id = c.id");
        let names = extract_table_names(&query);
        assert!(
            names.contains("orders"),
            "missing left table in {:?}",
            names
        );
        assert!(
            names.contains("customers"),
            "missing right table in {:?}",
            names
        );
    }

    #[test]
    fn test_select_subquery_collects_inner_table() {
        let query = parse_query("SELECT * FROM (SELECT id FROM items) sub");
        let names = extract_table_names(&query);
        assert!(
            names.contains("items"),
            "missing subquery table in {:?}",
            names
        );
    }

    // ── INSERT with ON CONFLICT (test 119 scenario) ──────────────

    #[test]
    fn test_insert_on_conflict_collects_target() {
        let stmt = parse_stmt(
            "INSERT INTO t_child(id, parent_id) VALUES (1, 1) \
             ON CONFLICT (id) DO UPDATE SET parent_id = EXCLUDED.parent_id + 1",
        );
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("t_child"),
            "INSERT ON CONFLICT target not found in {:?}",
            names
        );
    }

    // ── INSERT with type coercion (test 155 scenario) ─────────────

    #[test]
    fn test_insert_with_cast_collects_target() {
        let stmt = parse_stmt("INSERT INTO t_int(id, i) VALUES (1, '{\"a\":1}'::jsonb)");
        let names = extract_dml_table_names(&stmt);
        assert!(
            names.contains("t_int"),
            "INSERT with CAST target not found in {:?}",
            names
        );
    }
}
