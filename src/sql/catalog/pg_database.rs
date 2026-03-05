use super::helpers::{bool_col, char_col, int_col, int_val, null_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;

pub struct PgDatabase;

#[async_trait]
impl VirtualTable for PgDatabase {
    fn name(&self) -> &str {
        "pg_database"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_database".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("datname"),
                int_col("datdba"),
                int_col("encoding"),
                char_col("datlocprovider"),
                bool_col("datistemplate"),
                bool_col("datallowconn"),
                bool_col("dathasloginevt"),
                int_col("datconnlimit"),
                int_col("datfrozenxid"),
                int_col("datminmxid"),
                int_col("dattablespace"),
                text_col("datcollate"),
                text_col("datctype"),
                text_col("datlocale"),
                text_col("daticurules"),
                text_col("datcollversion"),
                text_col("datacl"),
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
        let mut dbs = ctx.store.list_databases(ctx.txn).await?;
        dbs.sort_by(|a, b| a.name.cmp(&b.name));

        let mut rows = Vec::with_capacity(dbs.len());
        for db in dbs {
            rows.push(Row::new(vec![
                int_val(db.oid as i64),
                text_val(&db.name),
                int_val(catalog_oids::pg_role_oid(&db.owner)),
                int_val(6),
                text_val("c"),
                Value::Boolean(db.is_template),
                Value::Boolean(db.allow_conn),
                Value::Boolean(false),
                int_val(-1),
                int_val(0),
                int_val(0),
                int_val(0),
                text_val("C"),
                text_val("C"),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
            ]));
        }

        Ok(rows)
    }
}
