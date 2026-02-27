//! Index definition formatting and OID-based lookup for `pg_get_indexdef()`.

use crate::model::IndexDef;
use crate::sql::catalog_oids;
use crate::storage::TikvStore;
use anyhow::Result;
use std::sync::Arc;
use tikv_client::Transaction;

use super::split_schema_and_name;

fn access_method_name(method: Option<&str>) -> &str {
    method.unwrap_or("btree")
}

pub(crate) fn format_index_columns(idx: &IndexDef) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(idx.columns.iter().cloned());
    parts.extend(idx.expressions.iter().map(|e| format!("({})", e)));
    parts.join(", ")
}

pub(crate) fn format_indexdef(table_schema: &str, table_name: &str, idx: &IndexDef) -> String {
    let cols = format_index_columns(idx);
    let mut indexdef = format!(
        "CREATE {}INDEX {} ON {}.{} USING {} ({})",
        if idx.unique { "UNIQUE " } else { "" },
        idx.name,
        table_schema,
        table_name,
        access_method_name(idx.method.as_deref()),
        cols
    );
    if let Some(pred) = idx.predicate.as_ref() {
        indexdef.push_str(" WHERE ");
        indexdef.push_str(pred);
    }
    indexdef
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::types::IndexState;

    fn sample_index() -> IndexDef {
        IndexDef {
            name: "idx_t_a".to_string(),
            id: 1,
            columns: vec!["a".to_string(), "b".to_string()],
            unique: true,
            is_constraint: false,
            method: Some("hash".to_string()),
            predicate: Some("a > 0".to_string()),
            expressions: vec!["lower(c)".to_string()],
            state: IndexState::Ready,
        }
    }

    #[test]
    fn format_columns_includes_expressions() {
        let idx = sample_index();
        assert_eq!(format_index_columns(&idx), "a, b, (lower(c))");
    }

    #[test]
    fn format_indexdef_includes_unique_method_and_predicate() {
        let idx = sample_index();
        let ddl = format_indexdef("public", "t", &idx);
        assert_eq!(
            ddl,
            "CREATE UNIQUE INDEX idx_t_a ON public.t USING hash (a, b, (lower(c))) WHERE a > 0"
        );
    }

    #[test]
    fn format_indexdef_defaults_to_btree() {
        let mut idx = sample_index();
        idx.unique = false;
        idx.method = None;
        idx.predicate = None;
        idx.expressions.clear();
        let ddl = format_indexdef("s", "t", &idx);
        assert_eq!(ddl, "CREATE INDEX idx_t_a ON s.t USING btree (a, b)");
    }
}
