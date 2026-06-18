//! OID-based lookup for `pg_get_indexdef()` on the PL/pgSQL / sequence-expr
//! evaluation path.
//!
//! Index-def *formatting* has a single source of truth in
//! [`crate::sql::catalog::helpers::format_indexdef`] (the same formatter the
//! analyzed-SELECT / `pg_indexes` path uses). This module only does the OID →
//! `IndexDef` lookup and then delegates formatting, so the two paths can never
//! diverge again (e.g. dropping an operator class — #2694).

use crate::sql::catalog::helpers::format_indexdef;
use crate::sql::catalog_oids;
use crate::storage::TikvStore;
use anyhow::Result;
use std::sync::Arc;
use tikv_client::Transaction;

use super::split_schema_and_name;

pub(crate) async fn lookup_indexdef_by_oid(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    oid: i64,
) -> Result<Option<String>> {
    let user_tables = store.list_tables(txn, db_id).await?;

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(&full_table_name);
        let Some(schema) = store.get_schema(txn, db_id, &full_table_name).await? else {
            continue;
        };

        for idx in &schema.indexes {
            let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
            if index_oid == oid {
                return Ok(Some(format_indexdef(&table_schema, &table_name, idx)));
            }
        }

        if !schema.pk_indices.is_empty() {
            let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
            if pk_oid == oid {
                let pk_cols: Vec<String> = schema
                    .pk_indices
                    .iter()
                    .filter_map(|idx| schema.columns.get(*idx).map(|c| c.name.clone()))
                    .collect();
                let indexdef = format!(
                    "CREATE UNIQUE INDEX {}_pkey ON {}.{} USING btree ({})",
                    table_name,
                    table_schema,
                    table_name,
                    pk_cols.join(", ")
                );
                return Ok(Some(indexdef));
            }
        }
    }

    Ok(None)
}
