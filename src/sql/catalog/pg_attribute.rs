use super::helpers::{bool_col, int_col, int_val, name_col, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::sql::catalog_oids;
use crate::sql::pg_types;
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgAttribute;

#[async_trait]
impl VirtualTable for PgAttribute {
    fn name(&self) -> &str {
        "pg_attribute"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_attribute".to_string(),
            columns: vec![
                int_col("attrelid"),
                name_col("attname"),
                int_col("atttypid"),
                int_col("attnum"),
                int_col("attlen"),
                bool_col("attnotnull"),
                bool_col("atthasdef"),
                bool_col("attisdropped"),
                bool_col("attislocal"),
                int_col("atttypmod"),
                text_col("attgenerated"),
                text_col("attidentity"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut rows = Vec::new();

        for table_name in ctx.user_tables {
            if let Some(schema) = ctx.store.get_schema(ctx.txn, ctx.db_id, table_name).await? {
                let base_table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;

                for (i, col) in schema.columns.iter().enumerate() {
                    let (type_oid, attlen) = if let DataType::UserDefined(udt_name) = &col.data_type
                    {
                        let oid = ctx
                            .store
                            .get_type(ctx.txn, ctx.db_id, udt_name)
                            .await?
                            .map(|t| t.oid as i64)
                            .unwrap_or(25);
                        (oid, 4)
                    } else {
                        let (oid, typlen) = pg_types::oid_and_typlen_for_datatype(&col.data_type);
                        (oid, typlen as i64)
                    };

                    rows.push(Row::new(vec![
                        int_val(base_table_oid),
                        text_val(&col.name),
                        int_val(type_oid),
                        int_val((i + 1) as i64),
                        int_val(attlen),
                        Value::Boolean(!col.nullable),
                        Value::Boolean(col.is_serial || col.default_expr.is_some()),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        int_val(-1),
                        text_val(""),
                        text_val(""),
                    ]));
                }
            }
        }

        Ok(rows)
    }
}
