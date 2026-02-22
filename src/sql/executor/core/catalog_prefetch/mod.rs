//! CatalogSnapshot builder: walks a raw sqlparser AST to extract all referenced
//! table names, then batch-fetches schemas from the store to populate a
//! `CatalogSnapshot` for the Analyzer.
//!
//! Sub-modules:
//! - `extraction`: AST visitors that collect table names, function calls, etc.
//! - `resolution`: Search-path resolution for tables, views, and functions.

mod extraction;
mod resolution;

#[cfg(test)]
mod tests;

use crate::sql::analyzer::CatalogSnapshot;
use crate::storage::TikvStore;
use crate::types::{Row, TableSchema};
use anyhow::Result;
use sqlparser::ast::{Query, Statement};
use std::collections::{HashMap, HashSet};
use tikv_client::Transaction;

use extraction::{extract_dml_table_names, extract_scalar_function_names_from_statement};
use resolution::{build_catalog_snapshot_inner, prefetch_scalar_functions, try_resolve_table};

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

    // Prefetch scalar UDFs used in DML expressions (SET/WHERE/RETURNING/VALUES).
    let scalar_function_names = extract_scalar_function_names_from_statement(stmt);
    prefetch_scalar_functions(
        store,
        txn,
        db_id,
        search_path,
        &scalar_function_names,
        &mut snapshot,
    )
    .await?;

    // Prefetch user-defined collations for Analyzer resolution.
    let collation_defs = store.list_collations(txn, db_id).await?;
    for def in collation_defs {
        let name = def.name.clone();
        snapshot.add_collation(&name, def);
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
