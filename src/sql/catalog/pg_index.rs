use super::helpers::{
    bool_col, int2vector_col, int_col, int_val, null_val, split_schema_and_name, text_col,
};
use super::{ScanContext, VirtualTable};
use crate::sql::catalog_oids;
use crate::types::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgIndex;

#[async_trait]
impl VirtualTable for PgIndex {
    fn name(&self) -> &str {
        "pg_index"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_index".to_string(),
            columns: vec![
                int_col("indexrelid"),
                int_col("indrelid"),
                int_col("indnatts"),
                bool_col("indisunique"),
                bool_col("indisprimary"),
                bool_col("indisexclusion"),
                bool_col("indimmediate"),
                bool_col("indisclustered"),
                bool_col("indisvalid"),
                int2vector_col("indkey"),
                text_col("indexprs"),
                text_col("indpred"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut rows = Vec::new();

        for full_table_name in ctx.user_tables {
            let (_table_schema, _table_name) = split_schema_and_name(full_table_name);
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                let base_table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;

                for idx in &schema.indexes {
                    let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;

                    let index_col_count = idx.columns.len() + idx.expressions.len();
                    let mut col_indices: Vec<i64> = Vec::new();
                    for col_name in &idx.columns {
                        if let Some(pos) = schema.columns.iter().position(|c| &c.name == col_name) {
                            col_indices.push((pos + 1) as i64);
                        }
                    }
                    for _ in &idx.expressions {
                        col_indices.push(0);
                    }
                    let indkey =
                        Value::Array(col_indices.iter().map(|i| Value::Int64(*i)).collect());

                    // indexprs: expression text for expression indexes, NULL for plain column indexes
                    let indexprs = if idx.expressions.is_empty() {
                        null_val()
                    } else {
                        Value::Text(idx.expressions.join(", "))
                    };

                    rows.push(Row::new(vec![
                        int_val(index_oid),
                        int_val(base_table_oid),
                        int_val(index_col_count as i64),
                        Value::Boolean(idx.unique),
                        Value::Boolean(false),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        indkey,
                        indexprs,
                        null_val(),
                    ]));
                }

                if !schema.pk_indices.is_empty() {
                    let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;

                    let indkey = schema
                        .pk_indices
                        .iter()
                        .map(|idx| (idx + 1) as i64)
                        .collect::<Vec<_>>();
                    rows.push(Row::new(vec![
                        int_val(pk_oid),
                        int_val(base_table_oid),
                        int_val(schema.pk_indices.len() as i64),
                        Value::Boolean(true),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        Value::Array(indkey.iter().map(|i| Value::Int64(*i)).collect()),
                        null_val(), // indexprs
                        null_val(), // indpred
                    ]));
                }
            }
        }

        Ok(rows)
    }
}
