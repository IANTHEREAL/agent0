use std::sync::Arc;

use anyhow::Result;
use tikv_client::Transaction;

use super::index_helpers;
use super::projection::fill_row_defaults;
use crate::model::{DataType, IndexDef, TableSchema, Value};
use crate::storage::TikvStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UniqueConflictResolution {
    /// The existing unique entry already points to `new_pk_values`.
    Idempotent,
    /// Stale entry was removed and target entry was created.
    StaleReplaced,
    /// Conflict is real and should be surfaced to caller.
    RealConflict,
}

pub(crate) fn is_unique_duplicate_error(err: &anyhow::Error) -> bool {
    if crate::storage::is_unique_index_duplicate_error(err) {
        return true;
    }
    // Backward-compatible fallback for older error sites that still stringify.
    let msg = err.to_string();
    msg.contains("Duplicate entry for unique index")
}

pub(crate) fn pk_types_for_schema(schema: &TableSchema) -> Vec<DataType> {
    if schema.pk_indices.is_empty() {
        vec![DataType::Uuid]
    } else {
        schema
            .pk_indices
            .iter()
            .map(|&idx| schema.columns[idx].data_type.clone())
            .collect()
    }
}

pub(crate) async fn resolve_unique_index_conflict(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    index: &IndexDef,
    idx_values: &[Value],
    new_pk_values: &[Value],
) -> Result<UniqueConflictResolution> {
    if !index.unique {
        return Ok(UniqueConflictResolution::RealConflict);
    }
    let pk_types = pk_types_for_schema(schema);

    // Bounded retries handle transient races with concurrent DML.
    for _ in 0..2 {
        let existing_pk = store
            .get_unique_index_pk_for_update(
                txn,
                db_id,
                schema.table_id,
                index.id,
                idx_values,
                &pk_types,
            )
            .await?;

        let Some(existing_pk) = existing_pk else {
            // Entry disappeared between duplicate error and validation; retry insert.
            match store
                .create_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    idx_values,
                    new_pk_values,
                    true,
                )
                .await
            {
                Ok(_) => return Ok(UniqueConflictResolution::StaleReplaced),
                Err(e) if is_unique_duplicate_error(&e) => continue,
                Err(e) => return Err(e),
            }
        };

        if existing_pk == new_pk_values {
            return Ok(UniqueConflictResolution::Idempotent);
        }

        let existing_row = store
            .lock_row_current_and_get_not_newer_than(txn, db_id, schema.table_id, &existing_pk)
            .await?;

        let stale = if let Some(mut existing_row) = existing_row {
            fill_row_defaults(&mut existing_row, schema)?;
            if !index_helpers::eval_index_predicate(index, schema, &existing_row)? {
                true
            } else {
                let existing_values =
                    index_helpers::get_index_values_with_expressions(index, schema, &existing_row)?;
                existing_values != idx_values
            }
        } else {
            true
        };

        if !stale {
            return Ok(UniqueConflictResolution::RealConflict);
        }

        store
            .delete_index_entry(
                txn,
                db_id,
                schema.table_id,
                index.id,
                idx_values,
                &existing_pk,
                true,
            )
            .await?;

        match store
            .recreate_deleted_index_entry(
                txn,
                db_id,
                schema.table_id,
                index.id,
                idx_values,
                new_pk_values,
                true,
            )
            .await
        {
            Ok(_) => return Ok(UniqueConflictResolution::StaleReplaced),
            Err(e) if is_unique_duplicate_error(&e) => continue,
            Err(e) => return Err(e),
        }
    }

    Ok(UniqueConflictResolution::RealConflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnDef;

    #[test]
    fn duplicate_error_matcher_detects_known_message() {
        let e = anyhow::anyhow!("Duplicate entry for unique index");
        assert!(is_unique_duplicate_error(&e));
    }

    #[test]
    fn duplicate_error_matcher_detects_structured_error() {
        let e = crate::storage::unique_index_duplicate_error();
        assert!(is_unique_duplicate_error(&e));
    }

    #[test]
    fn unique_conflict_resolution_uses_current_locked_reads() {
        let source = include_str!("index_consistency.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("index_consistency.rs must contain test module marker");
        let body = prod_source
            .split("pub(crate) async fn resolve_unique_index_conflict")
            .nth(1)
            .expect("resolve_unique_index_conflict must be present");

        assert!(
            body.contains(".get_unique_index_pk_for_update("),
            "unique conflict repair must lock-read the current unique index owner"
        );
        assert!(
            body.contains(".lock_row_current_and_get_not_newer_than("),
            "unique conflict repair must validate the current owner row under SI conflict checks"
        );
        assert!(
            !body.contains(".scan_index("),
            "unique conflict repair must not decide from a snapshot index scan"
        );
        assert!(
            !body.contains(".batch_get_rows("),
            "unique conflict repair must not decide from snapshot row reads"
        );
    }

    #[test]
    fn pk_types_for_schema_defaults_to_uuid_without_pk() {
        let schema =
            TableSchema::virtual_table("", vec![ColumnDef::new("c", DataType::Int32, true)]);
        assert_eq!(pk_types_for_schema(&schema), vec![DataType::Uuid]);
    }

    #[test]
    fn pk_types_for_schema_uses_pk_indices_order() {
        let schema = {
            let mut s = TableSchema::virtual_table(
                "",
                vec![
                    ColumnDef::new("a", DataType::Int32, false),
                    ColumnDef::new("b", DataType::Text, false),
                ],
            );
            s.pk_indices = vec![1, 0];
            s
        };
        assert_eq!(
            pk_types_for_schema(&schema),
            vec![DataType::Text, DataType::Int32]
        );
    }
}
