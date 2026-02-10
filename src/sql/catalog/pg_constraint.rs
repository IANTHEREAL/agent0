use super::helpers::{
    bool_col, int_array_col, int_col, int_val, null_val, schema_oid, split_schema_and_name,
    text_col, text_val,
};
use super::{ScanContext, VirtualTable};
use crate::sql::catalog_oids;
use crate::types::{ForeignKeyAction, Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

pub struct PgConstraint;

fn fk_action_code(action: &ForeignKeyAction) -> &'static str {
    match action {
        ForeignKeyAction::NoAction => "a",
        ForeignKeyAction::Restrict => "r",
        ForeignKeyAction::Cascade => "c",
        ForeignKeyAction::SetNull => "n",
        ForeignKeyAction::SetDefault => "d",
    }
}

fn fk_action_sql(action: &ForeignKeyAction) -> Option<&'static str> {
    match action {
        ForeignKeyAction::NoAction => None,
        ForeignKeyAction::Restrict => Some("RESTRICT"),
        ForeignKeyAction::Cascade => Some("CASCADE"),
        ForeignKeyAction::SetNull => Some("SET NULL"),
        ForeignKeyAction::SetDefault => Some("SET DEFAULT"),
    }
}

#[async_trait]
impl VirtualTable for PgConstraint {
    fn name(&self) -> &str {
        "pg_constraint"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_constraint".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("conname"),
                int_col("connamespace"),
                text_col("contype"),
                int_col("conrelid"),
                int_col("contypid"),
                int_col("confrelid"),
                int_array_col("conkey"),
                int_array_col("confkey"),
                text_col("confdeltype"),
                text_col("confupdtype"),
                bool_col("condeferrable"),
                bool_col("condeferred"),
                text_col("constraintdef"),
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
        let mut table_oids: HashMap<String, i64> = HashMap::new();
        let mut table_schemas: HashMap<String, TableSchema> = HashMap::new();

        for table_name in ctx.user_tables {
            if let Some(schema) = ctx.store.get_schema(ctx.txn, ctx.db_id, table_name).await? {
                let base_table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
                table_oids.insert(table_name.to_string(), base_table_oid);
                table_schemas.insert(table_name.to_string(), schema);
            }
        }

        let mut rows = Vec::new();
        let mut constraint_oid: i64 = 50000;

        for table_name in ctx.user_tables {
            let (table_schema, table_short_name) = split_schema_and_name(table_name);
            let connamespace_oid = schema_oid(ctx.schema_oids, &table_schema);
            let Some(schema) = table_schemas.get(table_name.as_str()) else {
                continue;
            };
            let Some(&conrelid) = table_oids.get(table_name.as_str()) else {
                continue;
            };

            if !schema.pk_indices.is_empty() {
                let conname = schema
                    .pk_constraint_name
                    .clone()
                    .unwrap_or_else(|| format!("{}_pkey", table_short_name));
                let conkey: Vec<Value> = schema
                    .pk_indices
                    .iter()
                    .map(|idx| Value::Int64((idx + 1) as i64))
                    .collect();
                let pk_cols: Vec<String> = schema
                    .pk_indices
                    .iter()
                    .filter_map(|idx| schema.columns.get(*idx).map(|c| c.name.clone()))
                    .collect();
                let constraintdef = format!("PRIMARY KEY ({})", pk_cols.join(", "));

                rows.push(Row::new(vec![
                    int_val(constraint_oid),
                    text_val(&conname),
                    int_val(connamespace_oid),
                    text_val("p"),
                    int_val(conrelid),
                    int_val(0),
                    int_val(0),
                    Value::Array(conkey),
                    Value::Array(vec![]),
                    null_val(),
                    null_val(),
                    Value::Boolean(false),
                    Value::Boolean(false),
                    text_val(&constraintdef),
                ]));
                constraint_oid += 1;
            }

            for idx in &schema.indexes {
                if !idx.unique {
                    continue;
                }
                let mut conkey = Vec::new();
                for col_name in &idx.columns {
                    if let Some(pos) = schema.columns.iter().position(|c| &c.name == col_name) {
                        conkey.push(Value::Int64((pos + 1) as i64));
                    }
                }
                let constraintdef = format!("UNIQUE ({})", idx.columns.join(", "));

                rows.push(Row::new(vec![
                    int_val(constraint_oid),
                    text_val(&idx.name),
                    int_val(connamespace_oid),
                    text_val("u"),
                    int_val(conrelid),
                    int_val(0),
                    int_val(0),
                    Value::Array(conkey),
                    Value::Array(vec![]),
                    null_val(),
                    null_val(),
                    Value::Boolean(false),
                    Value::Boolean(false),
                    text_val(&constraintdef),
                ]));
                constraint_oid += 1;
            }

            for (i, check) in schema.check_constraints.iter().enumerate() {
                let name = check
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_check{}", table_short_name, i + 1));
                let constraintdef = if check.expr.trim().starts_with('(') {
                    format!("CHECK {}", check.expr.trim())
                } else {
                    format!("CHECK ({})", check.expr.trim())
                };

                rows.push(Row::new(vec![
                    int_val(constraint_oid),
                    text_val(&name),
                    int_val(connamespace_oid),
                    text_val("c"),
                    int_val(conrelid),
                    int_val(0),
                    int_val(0),
                    Value::Array(vec![]),
                    Value::Array(vec![]),
                    null_val(),
                    null_val(),
                    Value::Boolean(false),
                    Value::Boolean(false),
                    text_val(&constraintdef),
                ]));
                constraint_oid += 1;
            }

            for fk in &schema.foreign_keys {
                let ref_table = fk.ref_table.clone();
                let (confrelid, ref_schema) =
                    match (table_oids.get(&ref_table), table_schemas.get(&ref_table)) {
                        (Some(&oid), Some(schema)) => (oid, schema),
                        _ => (0, schema),
                    };

                let mut conkey = Vec::new();
                for col_name in &fk.columns {
                    if let Some(pos) = schema.columns.iter().position(|c| &c.name == col_name) {
                        conkey.push(Value::Int64((pos + 1) as i64));
                    }
                }

                let mut confkey = Vec::new();
                if confrelid != 0 {
                    for col_name in &fk.ref_columns {
                        if let Some(pos) =
                            ref_schema.columns.iter().position(|c| &c.name == col_name)
                        {
                            confkey.push(Value::Int64((pos + 1) as i64));
                        }
                    }
                }

                let mut constraintdef = format!(
                    "FOREIGN KEY ({}) REFERENCES {} ({})",
                    fk.columns.join(", "),
                    ref_table,
                    fk.ref_columns.join(", ")
                );
                if let Some(action) = fk_action_sql(&fk.on_delete) {
                    constraintdef.push_str(&format!(" ON DELETE {}", action));
                }
                if let Some(action) = fk_action_sql(&fk.on_update) {
                    constraintdef.push_str(&format!(" ON UPDATE {}", action));
                }

                rows.push(Row::new(vec![
                    int_val(constraint_oid),
                    text_val(&fk.name),
                    int_val(connamespace_oid),
                    text_val("f"),
                    int_val(conrelid),
                    int_val(0),
                    int_val(confrelid),
                    Value::Array(conkey),
                    Value::Array(confkey),
                    text_val(fk_action_code(&fk.on_delete)),
                    text_val(fk_action_code(&fk.on_update)),
                    Value::Boolean(false),
                    Value::Boolean(false),
                    text_val(&constraintdef),
                ]));
                constraint_oid += 1;
            }
        }

        Ok(rows)
    }
}
