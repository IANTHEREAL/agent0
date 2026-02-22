//! CREATE/DROP VIEW, CREATE/DROP/REFRESH MATERIALIZED VIEW operations.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{ObjectName, Query, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tikv_client::Transaction;

use crate::sql::analyzer::{Analyzer, CatalogSnapshot};
use crate::sql::dml;
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;
use crate::types::{DataType, Row, TableSchema};

use super::create_table::check_relation_name_available;
use super::{
    advance_implicit_sequences_for_seeded_rows, create_implicit_sequences_for_schema,
    drop_dependent_views, drop_owned_sequences_for_table, was_cascade_dropped,
};

/// Resolve raw `RelationDep`s into fully-qualified dependency names.
///
/// - `Qualified { schema, name }` -> `"{schema}.{name}"` directly.
/// - `Unqualified { name }` -> search each schema in `search_path` order,
///   checking tables, views, and materialized views.  First hit wins.
///   If no hit exists, returns PostgreSQL-style "relation does not exist".
async fn resolve_view_deps(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    create_stmt_text: &str,
    raw_deps: std::collections::HashSet<crate::sql::binder::RelationDep>,
) -> Result<Vec<String>> {
    use crate::sql::binder::RelationDep;

    fn missing_relation_err(relation: &str, create_stmt_text: &str) -> anyhow::Error {
        let pos = create_stmt_text
            .rfind(relation)
            .unwrap_or(create_stmt_text.len().saturating_sub(1));
        let caret = format!("{}^", " ".repeat("LINE 1: ".len() + pos));
        anyhow!(
            "relation \"{}\" does not exist\nLINE 1: {}\n{}",
            relation,
            create_stmt_text,
            caret
        )
    }

    let mut resolved = Vec::with_capacity(raw_deps.len());
    for dep in raw_deps {
        match dep {
            RelationDep::Qualified { schema, name } => {
                let full = format!("{}.{}", schema, name);
                if !(store.table_exists(txn, db_id, &full).await?
                    || store.get_view(txn, db_id, &full).await?.is_some()
                    || store
                        .get_materialized_view(txn, db_id, &full)
                        .await?
                        .is_some())
                {
                    return Err(missing_relation_err(&full, create_stmt_text));
                }
                resolved.push(full);
            }
            RelationDep::Unqualified { name } => {
                let mut found = false;
                for schema in search_path {
                    let full = format!("{}.{}", schema, name);
                    if store.table_exists(txn, db_id, &full).await?
                        || store.get_view(txn, db_id, &full).await?.is_some()
                        || store
                            .get_materialized_view(txn, db_id, &full)
                            .await?
                            .is_some()
                    {
                        resolved.push(full);
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Err(missing_relation_err(&name, create_stmt_text));
                }
            }
        }
    }
    resolved.sort();
    resolved.dedup();
    Ok(resolved)
}

async fn analyze_view_output_schema(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    query: &Query,
) -> Result<Vec<(String, DataType)>> {
    // Build a minimal catalog snapshot from resolved base-table dependencies.
    // (Sufficient for CREATE OR REPLACE compatibility checks.)
    let mut catalog = CatalogSnapshot::new(search_path.to_vec(), db_id);
    let query_str = query.to_string();
    let create_stmt_text = format!("CREATE VIEW _ddl_schema_check AS {};", query_str);
    let raw_deps = crate::sql::binder::extract_dependencies_from_query(query);
    let deps =
        resolve_view_deps(store, txn, db_id, search_path, &create_stmt_text, raw_deps).await?;
    for dep in deps {
        if let Some(schema) = store.get_schema(txn, db_id, &dep).await? {
            let bare = dep.rsplit('.').next().unwrap_or(&dep).to_string();
            let full = dep.clone();
            catalog.add_table(&bare, full.clone(), schema.clone());
            catalog.add_table(&full.clone(), full, schema);
        }
    }
    // Prefetch user-defined collations for Analyzer resolution.
    let collation_defs = store.list_collations(txn, db_id).await?;
    for def in collation_defs {
        let name = def.name.clone();
        catalog.add_collation(&name, def);
    }
    let mut analyzer = Analyzer::new(&catalog);
    let analyzed = analyzer.analyze_query(query).map_err(SqlError::from)?;
    Ok(analyzed
        .output_schema
        .into_iter()
        .map(|(name, dt, _collation)| (name, dt))
        .collect())
}

pub async fn execute_create_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    query: &Query,
    or_replace: bool,
) -> Result<ExecuteResult> {
    let resolved = names::resolve_ddl_object_name(name, search_path)?;
    if !store.schema_exists(txn, db_id, &resolved.schema).await? {
        return Err(anyhow!("schema '{}' does not exist", resolved.schema));
    }
    let view_name = resolved.full;

    let query_str = query.to_string();
    let create_stmt_text = format!("CREATE VIEW {} AS {};", name, query_str);

    // PostgreSQL parity: CREATE VIEW validates referenced relations up front.
    let raw_deps = crate::sql::binder::extract_dependencies_from_query(query);
    let deps =
        resolve_view_deps(store, txn, db_id, search_path, &create_stmt_text, raw_deps).await?;

    // PostgreSQL parity: CREATE OR REPLACE VIEW may append columns only.
    if or_replace {
        if let Some(existing) = store.get_view(txn, db_id, &view_name).await? {
            let new_output =
                analyze_view_output_schema(store, txn, db_id, search_path, query).await?;

            let dialect = PostgreSqlDialect {};
            let old_stmts = Parser::parse_sql(&dialect, &existing.query)
                .map_err(|e| anyhow!("failed to parse stored view '{}': {}", view_name, e))?;
            let old_query = match old_stmts.as_slice() {
                [Statement::Query(q)] => q.as_ref(),
                _ => {
                    return Err(anyhow!(
                        "stored view '{}' does not contain a SELECT query",
                        view_name
                    ));
                }
            };
            let old_output =
                analyze_view_output_schema(store, txn, db_id, search_path, old_query).await?;

            if new_output.len() < old_output.len() {
                return Err(anyhow!("cannot drop columns from view"));
            }
            for (idx, (old_name, old_ty)) in old_output.iter().enumerate() {
                let (new_name, new_ty) = &new_output[idx];
                if old_name != new_name {
                    return Err(anyhow!(
                        "cannot change name of view column \"{}\" to \"{}\"",
                        old_name,
                        new_name
                    ));
                }
                if old_ty != new_ty {
                    return Err(anyhow!(
                        "cannot change data type of view column \"{}\"",
                        old_name
                    ));
                }
            }
        }
    }
    store
        .create_view(txn, db_id, &view_name, &query_str, deps, or_replace)
        .await?;

    Ok(ExecuteResult::CreateView { view_name })
}

pub async fn execute_drop_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecuteResult> {
    // Track names already dropped by CASCADE dependency resolution so that
    // multi-name DROP statements (e.g. DROP VIEW v1, v2 CASCADE where v2
    // depends on v1) don't error when a later name was already removed.
    let mut cascade_dropped: HashSet<String> = HashSet::new();
    let mut last = String::new();
    for name in names {
        let resolved =
            match names::resolve_existing_view_name(store.as_ref(), txn, db_id, name, search_path)
                .await?
            {
                Some(resolved) => resolved,
                None => {
                    // Already dropped as a transitive dependent — not an error.
                    if cascade && was_cascade_dropped(name, search_path, &cascade_dropped) {
                        continue;
                    }
                    if !if_exists {
                        return Err(anyhow!("view \"{}\" does not exist", name));
                    }
                    continue;
                }
            };

        // CASCADE: drop views that depend on this view.
        if cascade {
            let dropped = drop_dependent_views(store, txn, db_id, &resolved.full).await?;
            cascade_dropped.extend(dropped);
        }

        if !store.drop_view(txn, db_id, &resolved.full).await? && !if_exists {
            return Err(anyhow!("view \"{}\" does not exist", resolved.full));
        }
        last = resolved.full;
    }
    Ok(ExecuteResult::DropView { view_name: last })
}

pub async fn execute_create_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    query: &Query,
    or_replace: bool,
    mut schema: TableSchema,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    let resolved = names::resolve_ddl_object_name(name, search_path)?;
    if !store.schema_exists(txn, db_id, &resolved.schema).await? {
        return Err(anyhow!("schema '{}' does not exist", resolved.schema));
    }
    let view_name = resolved.full;
    schema.name = view_name.clone();

    if store
        .get_materialized_view(txn, db_id, &view_name)
        .await?
        .is_some()
    {
        if or_replace {
            for trigger in store
                .list_triggers_for_table(txn, db_id, &view_name)
                .await?
            {
                let _ = store
                    .drop_trigger(txn, db_id, &view_name, &trigger.name)
                    .await?;
            }
            drop_owned_sequences_for_table(store, txn, db_id, &view_name).await?;
            store.drop_materialized_view(txn, db_id, &view_name).await?;
            store.drop_table(txn, db_id, &view_name).await?;
        } else {
            return Err(anyhow!("Materialized view '{}' already exists", view_name));
        }
    }

    let query_str = query.to_string();
    let create_stmt_text = format!("CREATE MATERIALIZED VIEW {} AS {};", name, query_str);
    let raw_deps = crate::sql::binder::extract_dependencies_from_query(query);
    let deps =
        resolve_view_deps(store, txn, db_id, search_path, &create_stmt_text, raw_deps).await?;
    store
        .create_materialized_view(txn, db_id, &view_name, &query_str, deps)
        .await?;

    let row_count = rows.len();
    store.create_table(txn, db_id, schema.clone()).await?;
    create_implicit_sequences_for_schema(store, txn, db_id, &schema).await?;
    if let Some(pk_name) = &schema.pk_constraint_name {
        check_relation_name_available(
            store,
            txn,
            db_id,
            &resolved.schema,
            pk_name,
            false,
            Some(&view_name),
        )
        .await?;
    }
    for row in rows {
        store.insert(txn, db_id, &view_name, row).await?;
    }
    advance_implicit_sequences_for_seeded_rows(store, txn, db_id, &schema, row_count).await?;

    Ok(ExecuteResult::CreateMaterializedView { view_name })
}

pub async fn execute_drop_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecuteResult> {
    let mut cascade_dropped: HashSet<String> = HashSet::new();
    let mut last = String::new();
    for name in names {
        let resolved = match names::resolve_existing_materialized_view_name(
            store.as_ref(),
            txn,
            db_id,
            name,
            search_path,
        )
        .await?
        {
            Some(resolved) => resolved,
            None => {
                if cascade && was_cascade_dropped(name, search_path, &cascade_dropped) {
                    continue;
                }
                if !if_exists {
                    return Err(anyhow!("Materialized view '{}' does not exist", name));
                }
                continue;
            }
        };

        // CASCADE: drop views/matviews that depend on this materialized view.
        if cascade {
            let dropped = drop_dependent_views(store, txn, db_id, &resolved.full).await?;
            cascade_dropped.extend(dropped);
        }

        let exists = store
            .drop_materialized_view(txn, db_id, &resolved.full)
            .await?;
        if !exists && !if_exists {
            return Err(anyhow!(
                "Materialized view '{}' does not exist",
                resolved.full
            ));
        }
        if exists {
            for trigger in store
                .list_triggers_for_table(txn, db_id, &resolved.full)
                .await?
            {
                let _ = store
                    .drop_trigger(txn, db_id, &resolved.full, &trigger.name)
                    .await?;
            }
            drop_owned_sequences_for_table(store, txn, db_id, &resolved.full).await?;
            store.drop_table(txn, db_id, &resolved.full).await?;
        }
        last = resolved.full;
    }
    Ok(ExecuteResult::DropMaterializedView { view_name: last })
}

pub async fn execute_refresh_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    name: &str,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    if store
        .get_materialized_view(txn, db_id, name)
        .await?
        .is_none()
    {
        return Err(anyhow!("Materialized view '{}' does not exist", name));
    }

    let schema = store
        .get_schema(txn, db_id, name)
        .await?
        .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", name))?;

    if !store.truncate_table(txn, db_id, name).await? {
        return Err(anyhow!("Materialized view '{}' does not exist", name));
    }
    let enum_cache = dml::build_enum_label_cache(store, txn, db_id, &schema).await?;
    let row_count = rows.len();
    for row in rows {
        dml::execute_insert_row(
            store,
            txn,
            db_id,
            name,
            &schema,
            row,
            dml::ConflictBehavior::Error,
            &enum_cache,
        )
        .await?;
    }
    advance_implicit_sequences_for_seeded_rows(store, txn, db_id, &schema, row_count).await?;

    Ok(ExecuteResult::RefreshMaterializedView {
        view_name: name.to_string(),
    })
}
