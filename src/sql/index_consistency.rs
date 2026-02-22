use std::sync::Arc;

use anyhow::Result;
use tikv_client::Transaction;

use super::index_helpers;
use super::projection::fill_row_defaults;
use crate::storage::TikvStore;
use crate::types::{DataType, IndexDef, TableSchema, Value};

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
            .scan_index(
                txn,
                db_id,
                schema.table_id,
                index.id,
                idx_values,
                true,
                &pk_types,
                Some(1),
            )
            .await?
            .into_iter()
            .next();

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
                Ok(()) => return Ok(UniqueConflictResolution::StaleReplaced),
                Err(e) if is_unique_duplicate_error(&e) => continue,
                Err(e) => return Err(e),
            }
        };

        if existing_pk == new_pk_values {
            return Ok(UniqueConflictResolution::Idempotent);
        }

        let existing_rows = store
            .batch_get_rows(
                txn,
                db_id,
                schema.table_id,
                vec![existing_pk.clone()],
                schema,
            )
            .await?;

        let stale = if let Some(mut existing_row) = existing_rows.into_iter().next() {
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
            Ok(()) => return Ok(UniqueConflictResolution::StaleReplaced),
            Err(e) if is_unique_duplicate_error(&e) => continue,
            Err(e) => return Err(e),
        }
    }

    Ok(UniqueConflictResolution::RealConflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ColumnDef;

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
    fn pk_types_for_schema_defaults_to_uuid_without_pk() {
        let schema = TableSchema {
            columns: vec![ColumnDef {
                name: "c".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            ..TableSchema::default()
        };
        assert_eq!(pk_types_for_schema(&schema), vec![DataType::Uuid]);
    }

    #[test]
    fn pk_types_for_schema_uses_pk_indices_order() {
        let schema = TableSchema {
            columns: vec![
                ColumnDef {
                    name: "a".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "b".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            pk_indices: vec![1, 0],
            ..TableSchema::default()
        };
        assert_eq!(
            pk_types_for_schema(&schema),
            vec![DataType::Text, DataType::Int32]
        );
    }
}
