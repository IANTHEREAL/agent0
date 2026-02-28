use std::sync::Arc;

use anyhow::{anyhow, Result};
use tikv_client::Transaction;

use super::enum_rewrite::{
    rewrite_expr_enum_default_for_column, rewrite_expr_enum_literals,
    rewrite_query_enum_literals_for_view,
};
use super::helpers::enum_column_names;
use super::rename::{can_match_unqualified_type_name, get_enum_type};
use crate::model::{build_predicate_conjunct_cache, DataType, TableSchema, UserTypeKind};
use crate::sql::alter_type::AddValuePosition;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;

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
                        if let crate::model::Value::Text(ref v) = old_row.values[col_idx] {
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
                        if let crate::model::Value::Text(ref v) = new_values[col_idx] {
                            if v == old_label {
                                new_values[col_idx] =
                                    crate::model::Value::Text(new_label.to_string());
                            }
                        }
                    }
                }
                let new_row = crate::model::Row::new(new_values);

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
pub(super) fn update_schema_enum_literal(
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
                idx.cached_predicate_conjuncts =
                    build_predicate_conjunct_cache(idx.predicate.as_deref());
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
