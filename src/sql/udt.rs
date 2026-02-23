use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    visit_expressions_mut, BinaryOperator, DataType as SqlDataType, Expr as AstExpr, Ident,
    ObjectName, Query, SetExpr, Statement, TableFactor, TableWithJoins,
    UserDefinedTypeRepresentation, Value as SqlValue, VisitMut, VisitorMut,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tikv_client::Transaction;

use super::alter_type::AddValuePosition;
use super::analyzer::{Analyzer, CatalogSnapshot};
use super::names;
use super::names::normalize_ident;
use super::types::sql_datatype_to_internal_strict;
use super::ExecuteResult;
use crate::storage::TikvStore;
use crate::types::{ColumnDef, DataType, TableSchema, UserTypeDef, UserTypeKind};

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
                let field_type = sql_datatype_to_internal_strict(&attr.data_type)?;
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
                    .find(|c| matches!(&c.data_type, crate::types::DataType::UserDefined(t) if t == full_name))
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
    }
    Ok(ExecuteResult::CommandComplete { tag: "DROP TYPE" })
}

// ── ALTER TYPE helpers ──────────────────────────────────────────────

/// Fetch the enum type definition, returning an error if it doesn't exist or
/// is not an enum.
async fn get_enum_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    full_name: &str,
) -> Result<UserTypeDef> {
    let def = store
        .get_type(txn, db_id, full_name)
        .await?
        .ok_or_else(|| anyhow!("type \"{}\" does not exist", full_name))?;
    if !matches!(def.kind, UserTypeKind::Enum { .. }) {
        return Err(anyhow!(
            "\"{}\" is not an enum type",
            full_name.rsplit('.').next().unwrap_or(full_name)
        ));
    }
    Ok(def)
}

async fn can_match_unqualified_type_name(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    target_full: &str,
) -> Result<bool> {
    let (_, target_name) = names::parse_full_name(target_full)?;
    let mut found_target = false;

    for ty in store.list_types(txn, db_id).await? {
        if ty.name != target_name {
            continue;
        }
        let full = format!("{}.{}", ty.schema, ty.name);
        if full == target_full {
            found_target = true;
            continue;
        }
        return Ok(false);
    }

    Ok(found_target)
}

fn datatype_has_unqualified_type_name(data_type: &SqlDataType, target_name: &str) -> bool {
    match data_type {
        SqlDataType::Custom(type_name, _) => {
            type_name.0.len() == 1 && normalize_ident(&type_name.0[0]) == target_name
        }
        SqlDataType::Array(elem) => match elem {
            sqlparser::ast::ArrayElemTypeDef::None => false,
            sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner)
            | sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner) => {
                datatype_has_unqualified_type_name(inner, target_name)
            }
        },
        SqlDataType::Struct(fields) => fields
            .iter()
            .any(|f| datatype_has_unqualified_type_name(&f.field_type, target_name)),
        _ => false,
    }
}

fn expr_has_unqualified_type_cast(expr_sql: &str, target_name: &str) -> Result<bool> {
    let mut expr = parse_sql_expr(expr_sql)?;
    let mut found = false;
    let _ = visit_expressions_mut(&mut expr, |e| {
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                if datatype_has_unqualified_type_name(data_type, target_name) {
                    found = true;
                    return ControlFlow::Break(());
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    Ok(found)
}

fn query_has_unqualified_type_cast(query_sql: &str, target_name: &str) -> Result<bool> {
    let mut query = parse_stored_query(query_sql)?;
    let mut found = false;
    let _ = visit_expressions_mut(&mut query, |e| {
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                if datatype_has_unqualified_type_name(data_type, target_name) {
                    found = true;
                    return ControlFlow::Break(());
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    Ok(found)
}

async fn ensure_no_ambiguous_unqualified_casts_for_rename(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    old_full: &str,
) -> Result<()> {
    let (_, old_name) = names::parse_full_name(old_full)?;
    let ambiguous_err = |location: &str| -> anyhow::Error {
        anyhow!(
            "cannot rename type \"{}\" because unqualified cast \"::{}\" is ambiguous across schemas\nDETAIL:  Found in {}.\nHINT:  Qualify casts with schema (for example, \"::schema.{}\") before renaming.",
            old_full,
            old_name,
            location,
            old_name
        )
    };

    for table_name in store.list_tables(txn, db_id).await? {
        let Some(schema) = store.get_schema(txn, db_id, &table_name).await? else {
            continue;
        };

        for col in &schema.columns {
            if let Some(expr_sql) = &col.default_expr {
                if expr_has_unqualified_type_cast(expr_sql, &old_name)? {
                    return Err(ambiguous_err(&format!(
                        "DEFAULT expression for column '{}' of table '{}'",
                        col.name, table_name
                    )));
                }
            }
        }
        for check in &schema.check_constraints {
            if expr_has_unqualified_type_cast(&check.expr, &old_name)? {
                let check_name = check.name.as_deref().unwrap_or("<unnamed>");
                return Err(ambiguous_err(&format!(
                    "CHECK constraint '{}' of table '{}'",
                    check_name, table_name
                )));
            }
        }
        for idx in &schema.indexes {
            if let Some(pred) = &idx.predicate {
                if expr_has_unqualified_type_cast(pred, &old_name)? {
                    return Err(ambiguous_err(&format!(
                        "index predicate '{}' of table '{}'",
                        idx.name, table_name
                    )));
                }
            }
            for expr_sql in &idx.expressions {
                if expr_has_unqualified_type_cast(expr_sql, &old_name)? {
                    return Err(ambiguous_err(&format!(
                        "index expression '{}' of table '{}'",
                        idx.name, table_name
                    )));
                }
            }
        }
    }

    for view in store.list_views(txn, db_id).await? {
        let full_name = view.full_name();
        if query_has_unqualified_type_cast(&view.query, &old_name)? {
            return Err(ambiguous_err(&format!("view '{}'", full_name)));
        }
    }

    for matview in store.list_materialized_views(txn, db_id).await? {
        let full_name = matview.full_name();
        if query_has_unqualified_type_cast(&matview.query, &old_name)? {
            return Err(ambiguous_err(&format!("materialized view '{}'", full_name)));
        }
    }

    Ok(())
}

/// `ALTER TYPE <name> RENAME TO <new_name>`
///
/// Renames the type key in storage and updates all table column schemas that
/// reference the old name via `DataType::UserDefined(old_full)`.
pub async fn alter_type_rename(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    old_full: &str,
    new_name: &str,
) -> Result<ExecuteResult> {
    if new_name.is_empty() {
        return Err(anyhow!("zero-length delimited identifier"));
    }

    let def = store
        .get_type(txn, db_id, old_full)
        .await?
        .ok_or_else(|| anyhow!("type \"{}\" does not exist", old_full))?;

    let schema_part = old_full.splitn(2, '.').next().unwrap_or("public");
    let new_full = format!("{}.{}", schema_part, new_name);

    // Check that nothing else (table or type) already owns the new name.
    if store.get_schema(txn, db_id, &new_full).await?.is_some() {
        return Err(anyhow!("type \"{}\" already exists", new_name));
    }

    // Single-part type names (`::mood`) are only rewritten when that bare name
    // maps uniquely to the renamed type in this database. This prevents
    // cross-schema mis-rewrites when multiple schemas define the same type name.
    let allow_unqualified_type_match =
        can_match_unqualified_type_name(store, txn, db_id, old_full).await?;
    if !allow_unqualified_type_match {
        ensure_no_ambiguous_unqualified_casts_for_rename(store, txn, db_id, old_full).await?;
    }

    store
        .rename_type(txn, db_id, old_full, &new_full, def)
        .await?;

    // Update all table schemas that reference the old type name.
    // This covers DataType::UserDefined references AND SQL string artifacts
    // containing cast expressions like `::old_name` or `::schema.old_name`.
    let tables = store.list_tables(txn, db_id).await?;
    for table_name in tables {
        let Some(mut schema) = store.get_schema(txn, db_id, &table_name).await? else {
            continue;
        };
        let mut changed = false;
        for col in &mut schema.columns {
            if matches!(&col.data_type, DataType::UserDefined(t) if t == old_full) {
                col.data_type = DataType::UserDefined(new_full.clone());
                changed = true;
            }
        }
        // Update cast references (e.g., 'red'::color → 'red'::palette) in
        // defaults, check constraints, and index predicates/expressions.
        if update_schema_type_casts(
            &mut schema,
            old_full,
            &new_full,
            allow_unqualified_type_match,
        )? {
            changed = true;
        }
        if changed {
            schema.version += 1;
            store.update_schema(txn, db_id, schema).await?;
        }
    }

    // Views/matviews store query text and are re-parsed at query time. Keep
    // dependent definitions valid by rewriting cast targets there as well.
    for view in store.list_views(txn, db_id).await? {
        let full_name = view.full_name();
        if let Some(rewritten) = rewrite_query_type_casts(
            &view.query,
            old_full,
            &new_full,
            allow_unqualified_type_match,
        )? {
            store
                .update_view_query(txn, db_id, &full_name, &rewritten)
                .await?;
        }
    }
    for matview in store.list_materialized_views(txn, db_id).await? {
        let full_name = matview.full_name();
        if let Some(rewritten) = rewrite_query_type_casts(
            &matview.query,
            old_full,
            &new_full,
            allow_unqualified_type_match,
        )? {
            store
                .update_materialized_view_query(txn, db_id, &full_name, &rewritten)
                .await?;
        }
    }

    Ok(ExecuteResult::CommandComplete { tag: "ALTER TYPE" })
}

/// `ALTER TYPE <name> RENAME VALUE '<old>' TO '<new>'`
///
/// Updates the label list and rewrites stored row values in every table column
/// that uses this enum type, using the DML update path so that secondary,
/// unique, partial, expression, and GIN indexes are maintained correctly.
pub async fn alter_type_rename_value(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    full_name: &str,
    old_label: &str,
    new_label: &str,
) -> Result<ExecuteResult> {
    let mut def = get_enum_type(store, txn, db_id, full_name).await?;
    let allow_unqualified_type_match =
        can_match_unqualified_type_name(store, txn, db_id, full_name).await?;

    let labels = match &mut def.kind {
        UserTypeKind::Enum { labels } => labels,
        _ => unreachable!(),
    };

    let pos = labels
        .iter()
        .position(|l| l == old_label)
        .ok_or_else(|| anyhow!("\"{}\" is not an existing enum label", old_label))?;

    // P1-2: PostgreSQL 17.7 says "already exists" (no ", skipping").
    if labels.iter().any(|l| l == new_label) {
        return Err(anyhow!("enum label \"{}\" already exists", new_label));
    }

    labels[pos] = new_label.to_string();
    store.update_type(txn, db_id, def).await?;

    // Rewrite stored row values and schema expressions in all tables using
    // this enum type.
    let tables = store.list_tables(txn, db_id).await?;
    for table_name in tables {
        let Some(mut schema) = store.get_schema(txn, db_id, &table_name).await? else {
            continue;
        };
        // Rewrite SQL string artifacts regardless of whether this table has a
        // physical enum-typed column. Non-enum columns can still reference the
        // enum through explicit casts in defaults/checks/index expressions.
        if update_schema_enum_literal(
            &mut schema,
            full_name,
            old_label,
            new_label,
            allow_unqualified_type_match,
        )? {
            schema.version += 1;
            store.update_schema(txn, db_id, schema.clone()).await?;
        }

        // Find column indices that use this enum type.
        let enum_col_indices: Vec<usize> = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(&c.data_type, DataType::UserDefined(t) if t == full_name))
            .map(|(i, _)| i)
            .collect();

        if enum_col_indices.is_empty() {
            continue;
        }

        // Build enum label cache for *all* enum columns in this table (the
        // DML update path validates all enum columns, not just the ones we
        // are changing).
        let enum_cache =
            crate::sql::dml::build_enum_label_cache(store, txn, db_id, &schema).await?;

        // Paginated scan (same pattern as KvScanBatches in ddl/mod.rs).
        let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
        let mut next_start: Option<Vec<u8>> = Some(start);

        while let Some(scan_start) = next_start.take() {
            let range: tikv_client::BoundRange = (scan_start..end.clone()).into();
            let pairs: Vec<tikv_client::KvPair> =
                txn.scan(range, RENAME_SCAN_BATCH_SIZE).await?.collect();
            if pairs.is_empty() {
                break;
            }

            // Set up the next pagination cursor.
            let last_key: &[u8] = pairs.last().unwrap().key().as_ref().into();
            let mut cursor = last_key.to_vec();
            cursor.push(0x00);
            next_start = Some(cursor);

            for pair in pairs {
                let old_row = crate::storage::deserialize_row(pair.value())?;
                let mut needs_update = false;
                for &col_idx in &enum_col_indices {
                    if col_idx < old_row.values.len() {
                        if let crate::types::Value::Text(ref v) = old_row.values[col_idx] {
                            if v == old_label {
                                needs_update = true;
                                break;
                            }
                        }
                    }
                }

                if !needs_update {
                    continue;
                }

                // Build the new row with the renamed label.
                let mut new_values = old_row.values.clone();
                for &col_idx in &enum_col_indices {
                    if col_idx < new_values.len() {
                        if let crate::types::Value::Text(ref v) = new_values[col_idx] {
                            if v == old_label {
                                new_values[col_idx] =
                                    crate::types::Value::Text(new_label.to_string());
                            }
                        }
                    }
                }
                let new_row = crate::types::Row::new(new_values);

                // Use the DML update path for index maintenance, but skip FK
                // enforcement: enum label rename preserves value identity across
                // all tables, so FK cascades/violations are semantically wrong
                // (PostgreSQL 17.7 does not trigger FK checks on RENAME VALUE).
                crate::sql::dml::execute_update_row_without_fk_update(
                    store,
                    txn,
                    db_id,
                    &table_name,
                    &schema,
                    &old_row,
                    new_row,
                    &enum_cache,
                    None,
                )
                .await?;
            }
        }
    }

    // Keep view/matview definitions valid for enum-literal references in
    // stored SQL. Rewrite using query-scope qualifier-aware column context to
    // avoid cross-relation false positives when column names overlap.
    for view in store.list_views(txn, db_id).await? {
        let full = view.full_name();
        let relation_bindings = store
            .get_view_relation_bindings(txn, db_id, &full)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "missing persisted relation bindings for view '{}' during enum rewrite",
                    full
                )
            })?;
        if let Some(rewritten) = rewrite_query_enum_literals_for_view(
            store,
            txn,
            db_id,
            &view.query,
            &relation_bindings,
            full_name,
            old_label,
            new_label,
            allow_unqualified_type_match,
        )
        .await?
        {
            store
                .update_view_query(txn, db_id, &full, &rewritten)
                .await?;
        }
    }
    for matview in store.list_materialized_views(txn, db_id).await? {
        let full = matview.full_name();
        let relation_bindings = store
            .get_materialized_view_relation_bindings(txn, db_id, &full)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "missing persisted relation bindings for materialized view '{}' during enum rewrite",
                    full
                )
            })?;
        if let Some(rewritten) = rewrite_query_enum_literals_for_view(
            store,
            txn,
            db_id,
            &matview.query,
            &relation_bindings,
            full_name,
            old_label,
            new_label,
            allow_unqualified_type_match,
        )
        .await?
        {
            store
                .update_materialized_view_query(txn, db_id, &full, &rewritten)
                .await?;
        }
    }

    Ok(ExecuteResult::CommandComplete { tag: "ALTER TYPE" })
}

/// Batch size for the paginated table scan in `alter_type_rename_value`.
const RENAME_SCAN_BATCH_SIZE: u32 = 1024;

// ── Schema SQL string rewriting helpers ───────────────────────────

/// Update SQL string artifacts in a table schema when an enum label is renamed.
///
/// Rewrites `default_expr` (scoped to columns of the given enum type), and
/// table-level expressions (`check_constraints`, `index.predicate`,
/// `index.expressions`) using AST context so unrelated literals are preserved.
/// Returns `true` if any changes were made.
fn update_schema_enum_literal(
    schema: &mut TableSchema,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_unqualified_type_match: bool,
) -> Result<bool> {
    let mut changed = false;
    let enum_columns = enum_column_names(schema, enum_full_name);

    // Default expressions for enum-typed columns: permit bare literal rewrite
    // (`DEFAULT 'old'`) in addition to cast-context rewrite.
    for col in &mut schema.columns {
        if let Some(ref expr) = col.default_expr {
            if matches!(&col.data_type, DataType::UserDefined(t) if t == enum_full_name) {
                if let Some(rewritten) = rewrite_expr_enum_default_for_column(
                    expr,
                    enum_full_name,
                    old_label,
                    new_label,
                    allow_unqualified_type_match,
                )? {
                    col.default_expr = Some(rewritten);
                    changed = true;
                }
            } else if let Some(rewritten) = rewrite_expr_enum_literals(
                expr,
                &enum_columns,
                enum_full_name,
                old_label,
                new_label,
                false,
                allow_unqualified_type_match,
            )? {
                // Non-enum columns may still reference enum labels via explicit casts.
                col.default_expr = Some(rewritten);
                changed = true;
            }
        }
    }

    for check in &mut schema.check_constraints {
        if let Some(rewritten) = rewrite_expr_enum_literals(
            &check.expr,
            &enum_columns,
            enum_full_name,
            old_label,
            new_label,
            true,
            allow_unqualified_type_match,
        )? {
            check.expr = rewritten;
            changed = true;
        }
    }

    for idx in &mut schema.indexes {
        if let Some(ref pred) = idx.predicate {
            if let Some(rewritten) = rewrite_expr_enum_literals(
                pred,
                &enum_columns,
                enum_full_name,
                old_label,
                new_label,
                true,
                allow_unqualified_type_match,
            )? {
                idx.predicate = Some(rewritten);
                changed = true;
            }
        }
        for expr_str in &mut idx.expressions {
            if let Some(rewritten) = rewrite_expr_enum_literals(
                expr_str,
                &enum_columns,
                enum_full_name,
                old_label,
                new_label,
                true,
                allow_unqualified_type_match,
            )? {
                *expr_str = rewritten;
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Update type cast references in SQL string artifacts when a type is renamed.
///
/// Rewrites cast target datatypes through SQL AST instead of string replacement.
/// Returns `true` if any changes were made.
fn update_schema_type_casts(
    schema: &mut TableSchema,
    old_full: &str,
    new_full: &str,
    allow_unqualified_type_match: bool,
) -> Result<bool> {
    let mut changed = false;

    for col in &mut schema.columns {
        if let Some(ref expr) = col.default_expr {
            if let Some(rewritten) =
                rewrite_expr_type_casts(expr, old_full, new_full, allow_unqualified_type_match)?
            {
                col.default_expr = Some(rewritten);
                changed = true;
            }
        }
    }
    for check in &mut schema.check_constraints {
        if let Some(rewritten) = rewrite_expr_type_casts(
            &check.expr,
            old_full,
            new_full,
            allow_unqualified_type_match,
        )? {
            check.expr = rewritten;
            changed = true;
        }
    }
    for idx in &mut schema.indexes {
        if let Some(ref pred) = idx.predicate {
            if let Some(rewritten) =
                rewrite_expr_type_casts(pred, old_full, new_full, allow_unqualified_type_match)?
            {
                idx.predicate = Some(rewritten);
                changed = true;
            }
        }
        for expr_str in &mut idx.expressions {
            if let Some(rewritten) =
                rewrite_expr_type_casts(expr_str, old_full, new_full, allow_unqualified_type_match)?
            {
                *expr_str = rewritten;
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn rewrite_query_type_casts(
    query_sql: &str,
    old_full: &str,
    new_full: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut query = parse_stored_query(query_sql)?;
    let (old_schema, old_name) = names::parse_full_name(old_full)?;
    let (new_schema, new_name) = names::parse_full_name(new_full)?;
    let mut changed = false;

    let _ = visit_expressions_mut(&mut query, |e| {
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                if rewrite_type_in_datatype(
                    data_type,
                    &old_schema,
                    &old_name,
                    &new_schema,
                    &new_name,
                    allow_unqualified_type_match,
                ) {
                    changed = true;
                }
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(query.to_string()))
    } else {
        Ok(None)
    }
}

fn rewrite_expr_type_casts(
    expr_sql: &str,
    old_full: &str,
    new_full: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut expr = parse_sql_expr(expr_sql)?;
    let (old_schema, old_name) = names::parse_full_name(old_full)?;
    let (new_schema, new_name) = names::parse_full_name(new_full)?;
    let mut changed = false;

    let _ = visit_expressions_mut(&mut expr, |e| {
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                if rewrite_type_in_datatype(
                    data_type,
                    &old_schema,
                    &old_name,
                    &new_schema,
                    &new_name,
                    allow_unqualified_type_match,
                ) {
                    changed = true;
                }
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(expr.to_string()))
    } else {
        Ok(None)
    }
}

#[derive(Default, Clone)]
struct RelationColumnInfo {
    all_columns: HashSet<String>,
    enum_columns: HashSet<String>,
}

#[derive(Default, Clone)]
struct RelationEnumCatalog {
    by_full: HashMap<String, RelationColumnInfo>,
}

impl RelationEnumCatalog {
    fn insert(&mut self, full_name: String, info: RelationColumnInfo) {
        self.by_full.insert(full_name, info);
    }
}

#[derive(Default, Clone)]
struct QueryEnumScope {
    qualified_enum_columns: HashMap<String, HashSet<String>>,
    unqualified_counts: HashMap<String, (u32, u32)>,
}

impl QueryEnumScope {
    fn register_relation(&mut self, qualifier: String, relation: &RelationColumnInfo) {
        if !relation.enum_columns.is_empty() {
            self.qualified_enum_columns
                .insert(qualifier, relation.enum_columns.clone());
        }

        for col in &relation.all_columns {
            let entry = self.unqualified_counts.entry(col.clone()).or_insert((0, 0));
            if relation.enum_columns.contains(col) {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
    }

    fn is_qualified_enum_column(&self, qualifier: &str, column: &str) -> bool {
        self.qualified_enum_columns
            .get(qualifier)
            .map(|cols| cols.contains(column))
            .unwrap_or(false)
    }

    fn is_unqualified_enum_column(&self, column: &str) -> bool {
        matches!(self.unqualified_counts.get(column), Some((1, 0)))
    }
}

async fn build_relation_enum_catalog_from_bindings(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    relation_bindings: &[String],
    enum_full_name: &str,
) -> Result<(RelationEnumCatalog, HashMap<String, TableSchema>)> {
    let mut catalog = RelationEnumCatalog::default();
    let mut relation_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut schema_cache: HashMap<String, TableSchema> = HashMap::new();
    let mut resolving: HashSet<String> = HashSet::new();

    for relation_full_name in relation_bindings {
        if !seen.insert(relation_full_name.clone()) {
            continue;
        }

        let Some(schema) = resolve_relation_schema_for_rewrite(
            store,
            txn,
            db_id,
            relation_full_name,
            &mut schema_cache,
            &mut resolving,
        )
        .await?
        else {
            continue;
        };

        let info = relation_column_info_from_schema(&schema, enum_full_name);
        catalog.insert(relation_full_name.clone(), info);
        relation_schemas.insert(relation_full_name.clone(), schema);
    }

    Ok((catalog, relation_schemas))
}

fn resolve_relation_schema_for_rewrite<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    relation_full_name: &'a str,
    schema_cache: &'a mut HashMap<String, TableSchema>,
    resolving: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = Result<Option<TableSchema>>> + Send + 'a>> {
    Box::pin(async move {
        if let Some(schema) = schema_cache.get(relation_full_name) {
            return Ok(Some(schema.clone()));
        }

        if !resolving.insert(relation_full_name.to_string()) {
            return Err(anyhow!(
                "recursive relation dependency while inferring schema for '{}'",
                relation_full_name
            ));
        }

        let resolved =
            if let Some(table_schema) = store.get_schema(txn, db_id, relation_full_name).await? {
                Some(table_schema)
            } else if let Some(view_def) = store.get_view(txn, db_id, relation_full_name).await? {
                let relation_bindings = store
                    .get_view_relation_bindings(txn, db_id, relation_full_name)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "missing persisted relation bindings for view '{}'",
                            relation_full_name
                        )
                    })?;
                Some(
                    infer_view_like_schema_for_rewrite(
                        store,
                        txn,
                        db_id,
                        relation_full_name,
                        &view_def.query,
                        &relation_bindings,
                        schema_cache,
                        resolving,
                    )
                    .await?,
                )
            } else if let Some(matview_def) = store
                .get_materialized_view(txn, db_id, relation_full_name)
                .await?
            {
                let relation_bindings = store
                    .get_materialized_view_relation_bindings(txn, db_id, relation_full_name)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "missing persisted relation bindings for materialized view '{}'",
                            relation_full_name
                        )
                    })?;
                Some(
                    infer_view_like_schema_for_rewrite(
                        store,
                        txn,
                        db_id,
                        relation_full_name,
                        &matview_def.query,
                        &relation_bindings,
                        schema_cache,
                        resolving,
                    )
                    .await?,
                )
            } else {
                None
            };

        resolving.remove(relation_full_name);

        if let Some(schema) = &resolved {
            schema_cache.insert(relation_full_name.to_string(), schema.clone());
        }

        Ok(resolved)
    })
}

async fn infer_view_like_schema_for_rewrite(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    relation_full_name: &str,
    query_sql: &str,
    relation_bindings: &[String],
    schema_cache: &mut HashMap<String, TableSchema>,
    resolving: &mut HashSet<String>,
) -> Result<TableSchema> {
    let query = parse_stored_query(query_sql)?;
    let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();

    for dep in relation_bindings {
        if !seen.insert(dep.clone()) {
            continue;
        }
        let Some(schema) =
            resolve_relation_schema_for_rewrite(store, txn, db_id, dep, schema_cache, resolving)
                .await?
        else {
            return Err(anyhow!(
                "failed to resolve relation schema for bound relation '{}'",
                dep
            ));
        };
        dep_schemas.insert(dep.clone(), schema);
    }

    let mut snapshot =
        build_catalog_snapshot_for_view_bindings(db_id, &query, relation_bindings, &dep_schemas)?;

    // Keep Analyzer behavior aligned with normal execution paths.
    for def in store.list_collations(txn, db_id).await? {
        let name = def.name.clone();
        snapshot.add_collation(&name, def);
    }

    let mut analyzer = Analyzer::new(&snapshot);
    let analyzed = analyzer.analyze_query(&query).map_err(|e| {
        anyhow!(
            "failed to analyze dependency relation '{}' for enum rewrite: {}",
            relation_full_name,
            e
        )
    })?;

    Ok(synthetic_schema_from_output(
        relation_full_name,
        analyzed.output_schema,
    ))
}

fn build_catalog_snapshot_for_view_bindings(
    db_id: u64,
    query: &Query,
    relation_bindings: &[String],
    relation_schemas: &HashMap<String, TableSchema>,
) -> Result<CatalogSnapshot> {
    use crate::sql::binder::RelationDep;

    let raw_refs = crate::sql::binder::extract_relation_references_from_query(query);
    if raw_refs.len() != relation_bindings.len() {
        return Err(anyhow!(
            "relation binding count mismatch while rebuilding view catalog: refs={}, bindings={}",
            raw_refs.len(),
            relation_bindings.len()
        ));
    }

    let mut snapshot = CatalogSnapshot::new(vec!["public".to_string()], db_id);
    let mut key_to_full: HashMap<String, String> = HashMap::new();

    let mut bind_key = |key: String, full: &String, schema: &TableSchema| -> Result<()> {
        if let Some(existing) = key_to_full.get(&key) {
            if existing != full {
                return Err(anyhow!(
                    "inconsistent relation binding for key '{}': '{}' vs '{}'",
                    key,
                    existing,
                    full
                ));
            }
            return Ok(());
        }
        key_to_full.insert(key.clone(), full.clone());
        snapshot.add_table(&key, full.clone(), schema.clone());
        Ok(())
    };

    for (raw_ref, resolved_full) in raw_refs.iter().zip(relation_bindings.iter()) {
        let schema = relation_schemas.get(resolved_full).ok_or_else(|| {
            anyhow!(
                "missing relation schema for bound relation '{}'",
                resolved_full
            )
        })?;

        // Always expose the fully-qualified name key.
        bind_key(resolved_full.clone(), resolved_full, schema)?;

        match raw_ref {
            RelationDep::Unqualified { name } => {
                bind_key(name.clone(), resolved_full, schema)?;
            }
            RelationDep::Qualified { schema: s, name } => {
                let key = format!("{}.{}", s, name);
                bind_key(key, resolved_full, schema)?;
            }
        }
    }

    Ok(snapshot)
}

fn synthetic_schema_from_output<C>(
    relation_full_name: &str,
    output_schema: Vec<(String, DataType, Option<C>)>,
) -> TableSchema {
    let columns = output_schema
        .into_iter()
        .map(|(name, data_type, _)| ColumnDef {
            name,
            data_type,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        })
        .collect();

    TableSchema {
        name: relation_full_name.to_string(),
        table_id: 0,
        columns,
        version: 0,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        from_alias: None,
    }
}

fn relation_column_info_from_schema(
    schema: &TableSchema,
    enum_full_name: &str,
) -> RelationColumnInfo {
    let mut info = RelationColumnInfo::default();
    for col in &schema.columns {
        let name = col.name.rsplit('.').next().unwrap_or(&col.name).to_string();
        info.all_columns.insert(name.clone());
        if matches!(&col.data_type, DataType::UserDefined(t) if t == enum_full_name) {
            info.enum_columns.insert(name);
        }
    }
    info
}

fn build_query_scope_snapshot(
    query: &Query,
    base_snapshot: &CatalogSnapshot,
) -> Result<CatalogSnapshot> {
    let mut snapshot = base_snapshot.clone();

    let Some(with) = &query.with else {
        return Ok(snapshot);
    };

    for cte in &with.cte_tables {
        let cte_name = normalize_ident(&cte.alias.name);
        let cte_schema_query =
            if with.recursive && crate::sql::executor::cte_is_recursive(&cte.query, &cte_name) {
                let (base_expr, _, _) =
                    crate::sql::executor::decompose_recursive_union(&cte.query, &cte_name)
                        .map_err(|e| {
                            anyhow!(
                    "failed to decompose recursive CTE '{}' while preparing enum rewrite scope: {}",
                    cte_name,
                    e
                )
                        })?;
                Query {
                    with: None,
                    body: base_expr,
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    limit_by: vec![],
                    for_clause: None,
                }
            } else {
                cte.query.as_ref().clone()
            };
        let mut analyzer = Analyzer::new(&snapshot);
        let analyzed = analyzer.analyze_query(&cte_schema_query).map_err(|e| {
            anyhow!(
                "failed to analyze CTE '{}' while preparing enum rewrite scope: {}",
                cte_name,
                e
            )
        })?;

        let mut output_schema = analyzed.output_schema;
        if !cte.alias.columns.is_empty() {
            if cte.alias.columns.len() != output_schema.len() {
                return Err(anyhow!(
                    "CTE '{}' output column count mismatch while preparing enum rewrite scope: expected {}, got {}",
                    cte_name,
                    cte.alias.columns.len(),
                    output_schema.len()
                ));
            }
            for (idx, alias_col) in cte.alias.columns.iter().enumerate() {
                output_schema[idx].0 = normalize_ident(alias_col);
            }
        }

        let cte_schema = synthetic_schema_from_output(&cte_name, output_schema);
        snapshot.add_table(&cte_name, cte_name.clone(), cte_schema);
        snapshot.mark_non_base(&cte_name);
    }

    Ok(snapshot)
}

fn analyze_query_output_enum_columns(
    query: &Query,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<RelationColumnInfo> {
    let mut analyzer = Analyzer::new(snapshot);
    let analyzed = analyzer.analyze_query(query).map_err(|e| {
        anyhow!(
            "failed to analyze derived-table query during enum rewrite: {}",
            e
        )
    })?;
    let mut info = RelationColumnInfo::default();
    for (name, data_type, _collation) in analyzed.output_schema {
        info.all_columns.insert(name.clone());
        if matches!(&data_type, DataType::UserDefined(t) if t == enum_full_name) {
            info.enum_columns.insert(name);
        }
    }
    Ok(info)
}

fn register_derived_aliases_from_table_factor(
    factor: &TableFactor,
    scope: &mut QueryEnumScope,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<()> {
    match factor {
        TableFactor::Derived {
            subquery, alias, ..
        } => {
            if let Some(alias) = alias {
                let info = analyze_query_output_enum_columns(subquery, snapshot, enum_full_name)?;
                scope.register_relation(normalize_ident(&alias.name), &info);
            }
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            register_derived_aliases_from_table_with_joins(
                table_with_joins,
                scope,
                snapshot,
                enum_full_name,
            )?;
        }
        TableFactor::Pivot { table, .. } | TableFactor::Unpivot { table, .. } => {
            register_derived_aliases_from_table_factor(table, scope, snapshot, enum_full_name)?;
        }
        _ => {}
    }
    Ok(())
}

fn register_derived_aliases_from_table_with_joins(
    table_with_joins: &TableWithJoins,
    scope: &mut QueryEnumScope,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<()> {
    register_derived_aliases_from_table_factor(
        &table_with_joins.relation,
        scope,
        snapshot,
        enum_full_name,
    )?;
    for join in &table_with_joins.joins {
        register_derived_aliases_from_table_factor(
            &join.relation,
            scope,
            snapshot,
            enum_full_name,
        )?;
    }
    Ok(())
}

fn register_derived_aliases_for_query(
    query: &Query,
    scope: &mut QueryEnumScope,
    snapshot: &CatalogSnapshot,
    enum_full_name: &str,
) -> Result<()> {
    let snapshot = build_query_scope_snapshot(query, snapshot)?;
    if let SetExpr::Select(select) = query.body.as_ref() {
        for table_with_joins in &select.from {
            register_derived_aliases_from_table_with_joins(
                table_with_joins,
                scope,
                &snapshot,
                enum_full_name,
            )?;
        }
    }
    Ok(())
}

struct QueryEnumLiteralRewriter<'a> {
    scope_stack: Vec<QueryEnumScope>,
    relation_bindings: &'a [String],
    bind_idx: usize,
    query_relation_refs: Vec<Vec<crate::sql::binder::QueryRelationRef>>,
    query_scope_idx: usize,
    catalog: &'a RelationEnumCatalog,
    snapshot: Option<&'a CatalogSnapshot>,
    enum_full_name: &'a str,
    old_label: &'a str,
    new_label: &'a str,
    allow_unqualified_type_match: bool,
    changed: bool,
    error: Option<anyhow::Error>,
}

impl VisitorMut for QueryEnumLiteralRewriter<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        let Some(local_refs) = self.query_relation_refs.get(self.query_scope_idx).cloned() else {
            self.error = Some(anyhow!(
                "query relation scope index {} out of bounds while rewriting enum literals (total scopes={})",
                self.query_scope_idx,
                self.query_relation_refs.len()
            ));
            return ControlFlow::Break(());
        };
        self.query_scope_idx += 1;

        let mut scope = QueryEnumScope::default();
        for query_ref in local_refs {
            let Some(bound_full) = self.relation_bindings.get(self.bind_idx) else {
                self.error = Some(anyhow!(
                    "missing relation binding at index {} while rewriting enum literals",
                    self.bind_idx
                ));
                return ControlFlow::Break(());
            };
            self.bind_idx += 1;

            let Some(info) = self.catalog.by_full.get(bound_full) else {
                self.error = Some(anyhow!(
                    "missing relation enum metadata for bound relation '{}'",
                    bound_full
                ));
                return ControlFlow::Break(());
            };

            if !query_ref.qualifier.is_empty() {
                scope.register_relation(query_ref.qualifier, info);
            }
        }

        if let Some(snapshot) = self.snapshot {
            if let Err(e) =
                register_derived_aliases_for_query(query, &mut scope, snapshot, self.enum_full_name)
            {
                self.error = Some(e);
                return ControlFlow::Break(());
            }
        }

        self.scope_stack.push(scope);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
        self.scope_stack.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut AstExpr) -> ControlFlow<Self::Break> {
        let scope = self.scope_stack.last().cloned().unwrap_or_default();
        if rewrite_enum_literal_in_node_with_scope(
            expr,
            &scope,
            self.enum_full_name,
            self.old_label,
            self.new_label,
            true,
            self.allow_unqualified_type_match,
        ) {
            self.changed = true;
        }
        ControlFlow::Continue(())
    }
}

fn rewrite_query_enum_literals_with_catalog(
    query_sql: &str,
    relation_bindings: &[String],
    catalog: &RelationEnumCatalog,
    snapshot: Option<&CatalogSnapshot>,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut query = parse_stored_query(query_sql)?;
    let query_relation_refs = crate::sql::binder::extract_query_relation_refs_from_query(&query);
    let mut rewriter = QueryEnumLiteralRewriter {
        scope_stack: Vec::new(),
        relation_bindings,
        bind_idx: 0,
        query_relation_refs,
        query_scope_idx: 0,
        catalog,
        snapshot,
        enum_full_name,
        old_label,
        new_label,
        allow_unqualified_type_match,
        changed: false,
        error: None,
    };

    let _ = query.visit(&mut rewriter);
    if let Some(err) = rewriter.error {
        return Err(err);
    }
    if rewriter.bind_idx != relation_bindings.len() {
        return Err(anyhow!(
            "relation bindings not fully consumed during enum rewrite: consumed={}, total={}",
            rewriter.bind_idx,
            relation_bindings.len()
        ));
    }
    if rewriter.query_scope_idx != rewriter.query_relation_refs.len() {
        return Err(anyhow!(
            "query relation scopes not fully consumed during enum rewrite: consumed={}, total={}",
            rewriter.query_scope_idx,
            rewriter.query_relation_refs.len()
        ));
    }

    if rewriter.changed {
        Ok(Some(query.to_string()))
    } else {
        Ok(None)
    }
}

async fn rewrite_query_enum_literals_for_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    query_sql: &str,
    relation_bindings: &[String],
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let query = parse_stored_query(query_sql)?;
    let (catalog, relation_schemas) = build_relation_enum_catalog_from_bindings(
        store,
        txn,
        db_id,
        relation_bindings,
        enum_full_name,
    )
    .await?;
    let mut snapshot = build_catalog_snapshot_for_view_bindings(
        db_id,
        &query,
        relation_bindings,
        &relation_schemas,
    )?;
    for def in store.list_collations(txn, db_id).await? {
        let name = def.name.clone();
        snapshot.add_collation(&name, def);
    }
    rewrite_query_enum_literals_with_catalog(
        query_sql,
        relation_bindings,
        &catalog,
        Some(&snapshot),
        enum_full_name,
        old_label,
        new_label,
        allow_unqualified_type_match,
    )
}

fn rewrite_expr_enum_literals(
    expr_sql: &str,
    enum_columns: &HashSet<String>,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_column_context: bool,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut expr = parse_sql_expr(expr_sql)?;
    let mut changed = false;

    let _ = visit_expressions_mut(&mut expr, |e| {
        if rewrite_enum_literal_in_node(
            e,
            enum_columns,
            enum_full_name,
            old_label,
            new_label,
            allow_column_context,
            allow_unqualified_type_match,
        ) {
            changed = true;
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(expr.to_string()))
    } else {
        Ok(None)
    }
}

fn rewrite_expr_enum_default_for_column(
    expr_sql: &str,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_unqualified_type_match: bool,
) -> Result<Option<String>> {
    let mut expr = parse_sql_expr(expr_sql)?;
    let mut changed = rewrite_bare_string_literal(&mut expr, old_label, new_label);
    let empty_cols: HashSet<String> = HashSet::new();

    let _ = visit_expressions_mut(&mut expr, |e| {
        if rewrite_enum_literal_in_node(
            e,
            &empty_cols,
            enum_full_name,
            old_label,
            new_label,
            false,
            allow_unqualified_type_match,
        ) {
            changed = true;
        }
        ControlFlow::<()>::Continue(())
    });

    if changed {
        Ok(Some(expr.to_string()))
    } else {
        Ok(None)
    }
}

fn rewrite_enum_literal_in_node(
    expr: &mut AstExpr,
    enum_columns: &HashSet<String>,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_column_context: bool,
    allow_unqualified_type_match: bool,
) -> bool {
    let mut changed = false;

    match expr {
        AstExpr::Cast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::TryCast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::SafeCast {
            expr: inner,
            data_type,
            ..
        } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match) {
                changed |= rewrite_string_literal_expr(inner, old_label, new_label);
            }
        }
        AstExpr::TypedString { data_type, value } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match)
                && value == old_label
            {
                *value = new_label.to_string();
                changed = true;
            }
        }
        AstExpr::BinaryOp { left, op, right } => {
            if is_comparison_op(op) {
                if allow_column_context
                    && expr_is_target_enum_context(
                        left,
                        enum_columns,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if allow_column_context
                    && expr_is_target_enum_context(
                        right,
                        enum_columns,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    left,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    right,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
            }
        }
        AstExpr::IsDistinctFrom(left, right) | AstExpr::IsNotDistinctFrom(left, right) => {
            if allow_column_context
                && expr_is_target_enum_context(
                    left,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if allow_column_context
                && expr_is_target_enum_context(
                    right,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(left, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(right, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
        }
        AstExpr::InList { expr, list, .. } => {
            if allow_column_context
                && expr_is_target_enum_context(
                    expr,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for item in list {
                    changed |= rewrite_string_literal_expr(item, old_label, new_label);
                }
            }
        }
        AstExpr::Between {
            expr, low, high, ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context(
                    expr,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(low, old_label, new_label);
                changed |= rewrite_string_literal_expr(high, old_label, new_label);
            }
        }
        AstExpr::Case {
            operand: Some(operand),
            conditions,
            ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context(
                    operand,
                    enum_columns,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    operand,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for cond in conditions {
                    changed |= rewrite_string_literal_expr(cond, old_label, new_label);
                }
            }
        }
        _ => {}
    }

    changed
}

fn rewrite_enum_literal_in_node_with_scope(
    expr: &mut AstExpr,
    scope: &QueryEnumScope,
    enum_full_name: &str,
    old_label: &str,
    new_label: &str,
    allow_column_context: bool,
    allow_unqualified_type_match: bool,
) -> bool {
    let mut changed = false;

    match expr {
        AstExpr::Cast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::TryCast {
            expr: inner,
            data_type,
            ..
        }
        | AstExpr::SafeCast {
            expr: inner,
            data_type,
            ..
        } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match) {
                changed |= rewrite_string_literal_expr(inner, old_label, new_label);
            }
        }
        AstExpr::TypedString { data_type, value } => {
            if data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match)
                && value == old_label
            {
                *value = new_label.to_string();
                changed = true;
            }
        }
        AstExpr::BinaryOp { left, op, right } => {
            if is_comparison_op(op) {
                if allow_column_context
                    && expr_is_target_enum_context_with_scope(
                        left,
                        scope,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if allow_column_context
                    && expr_is_target_enum_context_with_scope(
                        right,
                        scope,
                        enum_full_name,
                        allow_unqualified_type_match,
                    )
                {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    left,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(right, old_label, new_label);
                }
                if expr_is_target_enum_cast_context(
                    right,
                    enum_full_name,
                    allow_unqualified_type_match,
                ) {
                    changed |= rewrite_string_literal_expr(left, old_label, new_label);
                }
            }
        }
        AstExpr::IsDistinctFrom(left, right) | AstExpr::IsNotDistinctFrom(left, right) => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    left,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    right,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(left, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(right, old_label, new_label);
            }
            if expr_is_target_enum_cast_context(right, enum_full_name, allow_unqualified_type_match)
            {
                changed |= rewrite_string_literal_expr(left, old_label, new_label);
            }
        }
        AstExpr::InList { expr, list, .. } => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    expr,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for item in list {
                    changed |= rewrite_string_literal_expr(item, old_label, new_label);
                }
            }
        }
        AstExpr::Between {
            expr, low, high, ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    expr,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    expr,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                changed |= rewrite_string_literal_expr(low, old_label, new_label);
                changed |= rewrite_string_literal_expr(high, old_label, new_label);
            }
        }
        AstExpr::Case {
            operand: Some(operand),
            conditions,
            ..
        } => {
            if allow_column_context
                && expr_is_target_enum_context_with_scope(
                    operand,
                    scope,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
                || expr_is_target_enum_cast_context(
                    operand,
                    enum_full_name,
                    allow_unqualified_type_match,
                )
            {
                for cond in conditions {
                    changed |= rewrite_string_literal_expr(cond, old_label, new_label);
                }
            }
        }
        _ => {}
    }

    changed
}

fn expr_is_target_enum_context_with_scope(
    expr: &AstExpr,
    scope: &QueryEnumScope,
    enum_full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match expr {
        AstExpr::Identifier(ident) => scope.is_unqualified_enum_column(&normalize_ident(ident)),
        AstExpr::CompoundIdentifier(parts) => {
            if parts.len() >= 2 {
                let qualifier = normalize_ident(&parts[parts.len() - 2]);
                let column = normalize_ident(parts.last().expect("parts.len() >= 2"));
                if scope.is_qualified_enum_column(&qualifier, &column) {
                    return true;
                }
                return false;
            }
            parts
                .last()
                .map(normalize_ident)
                .map(|n| scope.is_unqualified_enum_column(&n))
                .unwrap_or(false)
        }
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            expr_is_target_enum_context_with_scope(
                inner,
                scope,
                enum_full_name,
                allow_unqualified_type_match,
            )
        }
        _ => expr_is_target_enum_cast_context(expr, enum_full_name, allow_unqualified_type_match),
    }
}

fn rewrite_bare_string_literal(expr: &mut AstExpr, old_label: &str, new_label: &str) -> bool {
    match expr {
        AstExpr::Value(SqlValue::SingleQuotedString(v)) if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::IntroducedString {
            value: SqlValue::SingleQuotedString(v),
            ..
        } if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::TypedString { value, .. } if value == old_label => {
            *value = new_label.to_string();
            true
        }
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            rewrite_bare_string_literal(inner, old_label, new_label)
        }
        _ => false,
    }
}

fn rewrite_string_literal_expr(expr: &mut AstExpr, old_label: &str, new_label: &str) -> bool {
    match expr {
        AstExpr::Value(SqlValue::SingleQuotedString(v)) if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::IntroducedString {
            value: SqlValue::SingleQuotedString(v),
            ..
        } if v == old_label => {
            *v = new_label.to_string();
            true
        }
        AstExpr::TypedString { value, .. } if value == old_label => {
            *value = new_label.to_string();
            true
        }
        AstExpr::Nested(inner)
        | AstExpr::Collate { expr: inner, .. }
        | AstExpr::UnaryOp { expr: inner, .. }
        | AstExpr::Cast { expr: inner, .. }
        | AstExpr::TryCast { expr: inner, .. }
        | AstExpr::SafeCast { expr: inner, .. } => {
            rewrite_string_literal_expr(inner, old_label, new_label)
        }
        _ => false,
    }
}

fn enum_column_names(schema: &TableSchema, enum_full_name: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for col in &schema.columns {
        if matches!(&col.data_type, DataType::UserDefined(t) if t == enum_full_name) {
            out.insert(col.name.clone());
            out.insert(col.name.rsplit('.').next().unwrap_or(&col.name).to_string());
        }
    }
    out
}

fn expr_is_target_enum_context(
    expr: &AstExpr,
    enum_columns: &HashSet<String>,
    enum_full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match expr {
        AstExpr::Identifier(ident) => enum_columns.contains(&normalize_ident(ident)),
        AstExpr::CompoundIdentifier(parts) => parts
            .last()
            .map(normalize_ident)
            .map(|n| enum_columns.contains(&n))
            .unwrap_or(false),
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            expr_is_target_enum_context(
                inner,
                enum_columns,
                enum_full_name,
                allow_unqualified_type_match,
            )
        }
        _ => expr_is_target_enum_cast_context(expr, enum_full_name, allow_unqualified_type_match),
    }
}

fn expr_is_target_enum_cast_context(
    expr: &AstExpr,
    enum_full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match expr {
        AstExpr::Cast { data_type, .. }
        | AstExpr::TryCast { data_type, .. }
        | AstExpr::SafeCast { data_type, .. }
        | AstExpr::TypedString { data_type, .. } => {
            data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match)
        }
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            expr_is_target_enum_cast_context(inner, enum_full_name, allow_unqualified_type_match)
        }
        _ => false,
    }
}

fn is_comparison_op(op: &BinaryOperator) -> bool {
    matches!(
        op,
        &BinaryOperator::Eq
            | &BinaryOperator::NotEq
            | &BinaryOperator::Lt
            | &BinaryOperator::LtEq
            | &BinaryOperator::Gt
            | &BinaryOperator::GtEq
    )
}

fn data_type_matches_target(
    data_type: &SqlDataType,
    full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    let Ok((schema, name)) = names::parse_full_name(full_name) else {
        return false;
    };
    match data_type {
        SqlDataType::Custom(type_name, _) => {
            object_name_matches_target(type_name, &schema, &name, allow_unqualified_type_match)
        }
        _ => false,
    }
}

fn object_name_matches_target(
    type_name: &ObjectName,
    target_schema: &str,
    target_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match type_name.0.as_slice() {
        [single] => allow_unqualified_type_match && normalize_ident(single) == target_name,
        parts if parts.len() >= 2 => {
            let schema = normalize_ident(&parts[parts.len() - 2]);
            let name = normalize_ident(parts.last().expect("parts.len() >= 2"));
            schema == target_schema && name == target_name
        }
        _ => false,
    }
}

fn rewrite_type_in_datatype(
    data_type: &mut SqlDataType,
    old_schema: &str,
    old_name: &str,
    new_schema: &str,
    new_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match data_type {
        SqlDataType::Custom(type_name, _) => rewrite_custom_type_name(
            type_name,
            old_schema,
            old_name,
            new_schema,
            new_name,
            allow_unqualified_type_match,
        ),
        SqlDataType::Array(elem) => match elem {
            sqlparser::ast::ArrayElemTypeDef::None => false,
            sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner)
            | sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner) => rewrite_type_in_datatype(
                inner,
                old_schema,
                old_name,
                new_schema,
                new_name,
                allow_unqualified_type_match,
            ),
        },
        SqlDataType::Struct(fields) => fields.iter_mut().any(|f| {
            rewrite_type_in_datatype(
                &mut f.field_type,
                old_schema,
                old_name,
                new_schema,
                new_name,
                allow_unqualified_type_match,
            )
        }),
        _ => false,
    }
}

fn rewrite_custom_type_name(
    type_name: &mut ObjectName,
    old_schema: &str,
    old_name: &str,
    new_schema: &str,
    new_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    if type_name.0.len() == 1 {
        if allow_unqualified_type_match && normalize_ident(&type_name.0[0]) == old_name {
            assign_ident(&mut type_name.0[0], new_name);
            return true;
        }
        return false;
    }

    if type_name.0.len() >= 2 {
        let schema_idx = type_name.0.len() - 2;
        let name_idx = type_name.0.len() - 1;
        if normalize_ident(&type_name.0[schema_idx]) == old_schema
            && normalize_ident(&type_name.0[name_idx]) == old_name
        {
            assign_ident(&mut type_name.0[schema_idx], new_schema);
            assign_ident(&mut type_name.0[name_idx], new_name);
            return true;
        }
    }

    false
}

fn assign_ident(ident: &mut Ident, new_value: &str) {
    ident.value = new_value.to_string();
    ident.quote_style = required_quote_style(new_value);
}

fn required_quote_style(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Some('"');
    };
    let valid_first = first.is_ascii_alphabetic() || first == '_' || first == '$';
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if valid_first && valid_rest && value == value.to_ascii_lowercase() {
        None
    } else {
        Some('"')
    }
}

fn parse_sql_expr(expr_sql: &str) -> Result<AstExpr> {
    let dialect = PostgreSqlDialect {};
    Parser::new(&dialect)
        .try_with_sql(expr_sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("failed to parse expression '{}': {}", expr_sql, e))
}

fn parse_stored_query(sql: &str) -> Result<Query> {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql)
        .map_err(|e| anyhow!("failed to parse stored query '{}': {}", sql, e))?;
    match stmts.as_slice() {
        [Statement::Query(q)] => Ok(q.as_ref().clone()),
        _ => Err(anyhow!("stored SQL is not a single query")),
    }
}

/// `ALTER TYPE <name> ADD VALUE [IF NOT EXISTS] '<label>' [BEFORE|AFTER '<ref>']`
pub async fn alter_type_add_value(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    full_name: &str,
    if_not_exists: bool,
    new_label: &str,
    position: &AddValuePosition,
) -> Result<ExecuteResult> {
    let mut def = get_enum_type(store, txn, db_id, full_name).await?;

    let labels = match &mut def.kind {
        UserTypeKind::Enum { labels } => labels,
        _ => unreachable!(),
    };

    if labels.iter().any(|l| l == new_label) {
        if if_not_exists {
            // P1-3: PostgreSQL 17.7 emits NOTICE for IF NOT EXISTS duplicates.
            return Ok(ExecuteResult::Notice {
                message: format!("enum label \"{}\" already exists, skipping", new_label),
                severity: "NOTICE".to_string(),
            });
        }
        return Err(anyhow!("enum label \"{}\" already exists", new_label));
    }

    match position {
        AddValuePosition::End => {
            labels.push(new_label.to_string());
        }
        AddValuePosition::Before(ref_label) => {
            let pos = labels
                .iter()
                .position(|l| l == ref_label)
                .ok_or_else(|| anyhow!("\"{}\" is not an existing enum label", ref_label))?;
            labels.insert(pos, new_label.to_string());
        }
        AddValuePosition::After(ref_label) => {
            let pos = labels
                .iter()
                .position(|l| l == ref_label)
                .ok_or_else(|| anyhow!("\"{}\" is not an existing enum label", ref_label))?;
            labels.insert(pos + 1, new_label.to_string());
        }
    }

    store.update_type(txn, db_id, def).await?;
    Ok(ExecuteResult::CommandComplete { tag: "ALTER TYPE" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::Catalog;
    use crate::types::{CheckConstraint, ColumnDef, IndexDef};
    use crate::worker::types::IndexState;

    fn col(name: &str, data_type: DataType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }
    }

    fn test_schema() -> TableSchema {
        TableSchema::new("public.t".to_string(), 1, vec![], vec![])
    }

    #[test]
    fn type_cast_rewrite_is_identifier_safe() {
        let no_change =
            rewrite_expr_type_casts("'x'::mood2", "public.mood", "public.feeling", true).unwrap();
        assert!(no_change.is_none());

        let rewritten =
            rewrite_expr_type_casts("'x'::mood", "public.mood", "public.feeling", true).unwrap();
        let rewritten = rewritten.expect("cast should be rewritten");
        assert!(rewritten.contains("feeling"));
        assert!(!rewritten.contains("mood2"));
    }

    #[test]
    fn type_cast_rewrite_requires_safe_unqualified_match() {
        let no_change =
            rewrite_expr_type_casts("'x'::mood", "public.mood", "public.feeling", false).unwrap();
        assert!(no_change.is_none());

        let qualified =
            rewrite_expr_type_casts("'x'::public.mood", "public.mood", "public.feeling", false)
                .unwrap()
                .expect("qualified cast should still be rewritten");
        assert!(qualified.contains("public.feeling"));
    }

    #[test]
    fn ambiguous_unqualified_cast_detection_is_precise() {
        assert!(expr_has_unqualified_type_cast("'x'::mood", "mood").unwrap());
        assert!(!expr_has_unqualified_type_cast("'x'::public.mood", "mood").unwrap());
        assert!(query_has_unqualified_type_cast("SELECT 'x'::mood", "mood").unwrap());
        assert!(!query_has_unqualified_type_cast("SELECT 'x'::public.mood", "mood").unwrap());
    }

    #[test]
    fn enum_literal_rewrite_preserves_unrelated_text_literals() {
        let mut schema = test_schema();
        schema.columns = vec![
            col("txt", DataType::Text),
            col("state", DataType::UserDefined("public.status".to_string())),
        ];
        schema.check_constraints = vec![CheckConstraint {
            name: Some("chk".to_string()),
            expr: "txt <> 'active' AND state <> 'active'".to_string(),
        }];
        schema.indexes = vec![IndexDef {
            name: "idx_partial".to_string(),
            id: 1,
            columns: vec![],
            unique: false,
            method: None,
            predicate: Some("state = 'active' AND txt <> 'active'".to_string()),
            expressions: vec!["CASE WHEN state = 'active' THEN 1 ELSE 0 END".to_string()],
            state: IndexState::Ready,
        }];

        let changed =
            update_schema_enum_literal(&mut schema, "public.status", "active", "enabled", true)
                .unwrap();
        assert!(changed);

        let check = &schema.check_constraints[0].expr;
        assert!(check.contains("txt <> 'active'"));
        assert!(check.contains("state <> 'enabled'"));

        let pred = schema.indexes[0].predicate.as_deref().unwrap();
        assert!(pred.contains("txt <> 'active'"));
        assert!(pred.contains("state = 'enabled'"));

        let expr = &schema.indexes[0].expressions[0];
        assert!(expr.contains("state = 'enabled'"));
    }

    #[test]
    fn enum_default_bare_literal_is_rewritten() {
        let mut schema = test_schema();
        let mut state_col = col("state", DataType::UserDefined("public.status".to_string()));
        state_col.default_expr = Some("'active'".to_string());
        schema.columns = vec![state_col];

        let changed =
            update_schema_enum_literal(&mut schema, "public.status", "active", "enabled", true)
                .unwrap();
        assert!(changed);
        assert_eq!(schema.columns[0].default_expr.as_deref(), Some("'enabled'"));
    }

    #[test]
    fn enum_casts_are_rewritten_even_without_enum_typed_columns() {
        let mut schema = test_schema();
        let mut txt_col = col("txt", DataType::Text);
        txt_col.default_expr = Some("('active'::status)::text".to_string());
        schema.columns = vec![txt_col];
        schema.check_constraints = vec![CheckConstraint {
            name: Some("chk_cast".to_string()),
            expr: "('active'::status)::text <> txt".to_string(),
        }];

        let changed =
            update_schema_enum_literal(&mut schema, "public.status", "active", "enabled", true)
                .unwrap();
        assert!(changed);
        let default_expr = schema.columns[0]
            .default_expr
            .as_deref()
            .expect("default expr should exist");
        assert!(default_expr.contains("enabled"));
        assert!(default_expr.contains("status"));
        assert!(
            schema.check_constraints[0].expr.contains("enabled")
                && schema.check_constraints[0].expr.contains("status")
        );
    }

    #[test]
    fn view_query_rewrite_supports_column_context_literals() {
        let mut catalog = RelationEnumCatalog::default();
        catalog.insert(
            "public.t".to_string(),
            RelationColumnInfo {
                all_columns: ["state", "txt"]
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
                enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            },
        );

        let rewritten = rewrite_query_enum_literals_with_catalog(
            "SELECT state, txt FROM t WHERE state = 'active' AND txt = 'active'",
            &["public.t".to_string()],
            &catalog,
            None,
            "public.status",
            "active",
            "enabled",
            true,
        )
        .unwrap()
        .expect("query should be rewritten");

        assert!(rewritten.contains("state = 'enabled'"));
        assert!(rewritten.contains("txt = 'active'"));
    }

    #[test]
    fn view_query_rewrite_avoids_non_enum_same_name_columns() {
        let mut catalog = RelationEnumCatalog::default();
        catalog.insert(
            "public.t_enum".to_string(),
            RelationColumnInfo {
                all_columns: ["state"].into_iter().map(ToString::to_string).collect(),
                enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            },
        );
        catalog.insert(
            "public.t_text".to_string(),
            RelationColumnInfo {
                all_columns: ["state"].into_iter().map(ToString::to_string).collect(),
                enum_columns: HashSet::new(),
            },
        );

        let rewritten = rewrite_query_enum_literals_with_catalog(
            "SELECT 1 FROM t_enum e JOIN t_text t ON e.state <> t.state WHERE e.state = 'active' AND t.state = 'active'",
            &["public.t_enum".to_string(), "public.t_text".to_string()],
            &catalog,
            None,
            "public.status",
            "active",
            "enabled",
            true,
        )
        .unwrap()
        .expect("query should be rewritten");

        assert!(rewritten.contains("e.state = 'enabled'"));
        assert!(rewritten.contains("t.state = 'active'"));
    }

    #[test]
    fn view_query_rewrite_mixed_qualified_unqualified_same_bare_name() {
        let mut catalog = RelationEnumCatalog::default();
        catalog.insert(
            "a.t".to_string(),
            RelationColumnInfo {
                all_columns: ["state", "id"]
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
                enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            },
        );
        catalog.insert(
            "z.t".to_string(),
            RelationColumnInfo {
                all_columns: ["state", "id"]
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
                enum_columns: HashSet::new(),
            },
        );

        let rewritten = rewrite_query_enum_literals_with_catalog(
            "SELECT 1 FROM a.t qa JOIN t qz ON qa.state <> qz.state WHERE qa.state = 'active' AND qz.state = 'active'",
            &["a.t".to_string(), "z.t".to_string()],
            &catalog,
            None,
            "public.status",
            "active",
            "enabled",
            true,
        )
        .unwrap()
        .expect("query should be rewritten");

        assert!(rewritten.contains("qa.state = 'enabled'"));
        assert!(rewritten.contains("qz.state = 'active'"));
    }

    #[test]
    fn view_query_rewrite_handles_derived_table_alias_columns() {
        let mut catalog = RelationEnumCatalog::default();
        catalog.insert(
            "public.t".to_string(),
            RelationColumnInfo {
                all_columns: ["state", "txt"]
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
                enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            },
        );

        let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
        let mut t = test_schema();
        t.name = "public.t".to_string();
        t.columns = vec![
            col("state", DataType::UserDefined("public.status".to_string())),
            col("txt", DataType::Text),
        ];
        dep_schemas.insert("public.t".to_string(), t);

        let query = parse_stored_query(
            "SELECT 1 FROM (SELECT state, txt FROM t) s WHERE s.state = 'active' AND s.txt = 'active'",
        )
        .expect("query should parse");
        let relation_bindings = vec!["public.t".to_string()];
        let snapshot =
            build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
                .expect("snapshot should build");

        let rewritten = rewrite_query_enum_literals_with_catalog(
            "SELECT 1 FROM (SELECT state, txt FROM t) s WHERE s.state = 'active' AND s.txt = 'active'",
            &relation_bindings,
            &catalog,
            Some(&snapshot),
            "public.status",
            "active",
            "enabled",
            true,
        )
        .unwrap()
        .expect("query should be rewritten");

        assert!(rewritten.contains("s.state = 'enabled'"));
        assert!(rewritten.contains("s.txt = 'active'"));
    }

    #[test]
    fn view_query_rewrite_handles_derived_alias_from_outer_cte() {
        let mut catalog = RelationEnumCatalog::default();
        catalog.insert(
            "public.t".to_string(),
            RelationColumnInfo {
                all_columns: ["state", "txt"]
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
                enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            },
        );

        let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
        let mut t = test_schema();
        t.name = "public.t".to_string();
        t.columns = vec![
            col("state", DataType::UserDefined("public.status".to_string())),
            col("txt", DataType::Text),
        ];
        dep_schemas.insert("public.t".to_string(), t);

        let sql = "WITH c AS (SELECT state, txt FROM t) SELECT 1 FROM (SELECT state, txt FROM c) s WHERE s.state = 'active' AND s.txt = 'active'";
        let query = parse_stored_query(sql).expect("query should parse");
        let relation_bindings = vec!["public.t".to_string()];
        let snapshot =
            build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
                .expect("snapshot should build");

        let rewritten = rewrite_query_enum_literals_with_catalog(
            sql,
            &relation_bindings,
            &catalog,
            Some(&snapshot),
            "public.status",
            "active",
            "enabled",
            true,
        )
        .unwrap()
        .expect("query should be rewritten");

        assert!(rewritten.contains("s.state = 'enabled'"));
        assert!(rewritten.contains("s.txt = 'active'"));
    }

    #[test]
    fn view_query_rewrite_handles_derived_alias_from_recursive_outer_cte() {
        let mut catalog = RelationEnumCatalog::default();
        catalog.insert(
            "public.t".to_string(),
            RelationColumnInfo {
                all_columns: ["state", "txt"]
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
                enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            },
        );

        let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
        let mut t = test_schema();
        t.name = "public.t".to_string();
        t.columns = vec![
            col("state", DataType::UserDefined("public.status".to_string())),
            col("txt", DataType::Text),
        ];
        dep_schemas.insert("public.t".to_string(), t);

        let sql = "WITH RECURSIVE c(state, txt) AS (SELECT state, txt FROM t UNION ALL SELECT c.state, c.txt FROM c WHERE false) SELECT 1 FROM (SELECT state, txt FROM c) s WHERE s.state = 'active' AND s.txt = 'active'";
        let query = parse_stored_query(sql).expect("query should parse");
        let relation_bindings = vec!["public.t".to_string()];
        let snapshot =
            build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
                .expect("snapshot should build");

        let rewritten = rewrite_query_enum_literals_with_catalog(
            sql,
            &relation_bindings,
            &catalog,
            Some(&snapshot),
            "public.status",
            "active",
            "enabled",
            true,
        )
        .unwrap()
        .expect("query should be rewritten");

        assert!(rewritten.contains("s.state = 'enabled'"));
        assert!(rewritten.contains("s.txt = 'active'"));
    }

    #[test]
    fn view_query_rewrite_handles_recursive_outer_cte_nested_join_self_reference() {
        let mut catalog = RelationEnumCatalog::default();
        catalog.insert(
            "public.t".to_string(),
            RelationColumnInfo {
                all_columns: ["state", "txt"]
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
                enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            },
        );

        let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
        let mut t = test_schema();
        t.name = "public.t".to_string();
        t.columns = vec![
            col("state", DataType::UserDefined("public.status".to_string())),
            col("txt", DataType::Text),
        ];
        dep_schemas.insert("public.t".to_string(), t);

        let sql = "WITH RECURSIVE c(state, txt) AS (SELECT state, txt FROM t UNION ALL SELECT cj.state, cj.txt FROM (c JOIN (SELECT 1) d ON true) cj WHERE false) SELECT 1 FROM (SELECT state, txt FROM c) s WHERE s.state = 'active' AND s.txt = 'active'";
        let query = parse_stored_query(sql).expect("query should parse");
        let relation_bindings = vec!["public.t".to_string()];
        let snapshot =
            build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
                .expect("snapshot should build");

        let rewritten = rewrite_query_enum_literals_with_catalog(
            sql,
            &relation_bindings,
            &catalog,
            Some(&snapshot),
            "public.status",
            "active",
            "enabled",
            true,
        )
        .unwrap()
        .expect("query should be rewritten");

        assert!(rewritten.contains("s.state = 'enabled'"));
        assert!(rewritten.contains("s.txt = 'active'"));
    }

    #[test]
    fn binding_snapshot_preserves_explicit_unqualified_resolution() {
        let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();

        let mut a_v = test_schema();
        a_v.name = "a.v".to_string();
        a_v.columns = vec![col(
            "state",
            DataType::UserDefined("public.status".to_string()),
        )];
        dep_schemas.insert("a.v".to_string(), a_v);

        let mut z_v = test_schema();
        z_v.name = "z.v".to_string();
        z_v.columns = vec![col("state", DataType::Text)];
        dep_schemas.insert("z.v".to_string(), z_v);

        let query = parse_stored_query("SELECT 1 FROM a.v qa JOIN v qz ON qa.state <> qz.state")
            .expect("query should parse");
        let relation_bindings = vec!["a.v".to_string(), "z.v".to_string()];
        let snapshot =
            build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
                .expect("snapshot should build");

        assert!(snapshot.resolve_table("v", None).unwrap().is_some());
        assert!(snapshot.resolve_table("v", Some("a")).unwrap().is_some());
        assert!(snapshot.resolve_table("v", Some("z")).unwrap().is_some());
    }
}
