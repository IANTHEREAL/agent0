use super::helpers::{bool_col, int_col, int_val, name_col, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::sql::catalog_oids;
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
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, table_name)
                .await?
            {
                let base_table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;

                for (i, col) in schema.columns.iter().enumerate() {
                    let (type_oid, attlen) =
                        if let DataType::UserDefined(udt_name) = &col.data_type {
                            let oid = ctx
                                .store
                                .get_type(ctx.txn, ctx.db_id, udt_name)
                                .await?
                                .map(|t| t.oid as i64)
                                .unwrap_or(25);
                            (oid, 4)
                        } else {
                            let oid = match col.data_type {
                                DataType::Boolean => 16,
                                DataType::Int32 => 23,
                                DataType::Int64 => 20,
                                DataType::Float64 => 701,
                                DataType::Text => 25,
                                DataType::Bytes => 17,
                                DataType::Timestamp => 1114,
                                DataType::TimestampTz => 1184,
                                DataType::Date => 1082,
                                DataType::Uuid => 2950,
                                DataType::Json => 114,
                                DataType::Jsonb => 3802,
                                DataType::Vector(_) => 16385,
                                _ => 25,
                            };

                            let len = match col.data_type {
                                DataType::Boolean => 1,
                                DataType::Int32 => 4,
                                DataType::Int64 => 8,
                                DataType::Float64 => 8,
                                DataType::Timestamp => 8,
                                DataType::TimestampTz => 8,
                                DataType::Date => 4,
                                _ => -1,
                            };
                            (oid, len)
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
