use super::helpers::{
    bool_col, int_array_col, int_col, int_val, is_unique_constraint_index, null_val, schema_oid,
    split_schema_and_name, text_col, text_val,
};
use super::{ScanContext, VirtualTable};
use crate::model::{ForeignKeyAction, Row, TableSchema, Value};
use crate::sql::catalog_oids;
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

fn build_not_null_constraint_rows(
    schema: &TableSchema,
    table_short_name: &str,
    connamespace_oid: i64,
    conrelid: i64,
    constraint_oid: &mut i64,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (idx, col) in schema.columns.iter().enumerate() {
        if col.nullable {
            continue;
        }
        let conname = format!("{}_{}_not_null", table_short_name, col.name);
        rows.push(Row::new(vec![
            int_val(*constraint_oid),
            text_val(&conname),
            int_val(connamespace_oid),
            text_val("n"),
            int_val(conrelid),
            int_val(0),
            int_val(0),
            Value::Array(vec![Value::Int64((idx + 1) as i64)]),
            Value::Array(vec![]),
            null_val(),
            null_val(),
            Value::Boolean(false),
            Value::Boolean(false),
            text_val("NOT NULL"),
            int_val(0),
            int_val(0),
            Value::Boolean(false),
            Value::Boolean(true),
            int_val(0),
            Value::Boolean(true),
        ]));
        *constraint_oid += 1;
    }
    rows
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
                int_col("conindid"),
                int_col("conparentid"),
                bool_col("connoinherit"),
                bool_col("conislocal"),
                int_col("coninhcount"),
                bool_col("convalidated"),
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
                let conindid = catalog_oids::pg_class_pk_index_oid(schema.table_id).unwrap_or(0);

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
                    int_val(conindid),
                    int_val(0),
                    Value::Boolean(true),
                    Value::Boolean(true),
                    int_val(0),
                    Value::Boolean(true),
                ]));
                constraint_oid += 1;
            }

            for idx in &schema.indexes {
                if !is_unique_constraint_index(idx) {
                    continue;
                }
                let mut conkey = Vec::new();
                for col_name in &idx.columns {
                    if let Some(pos) = schema.columns.iter().position(|c| &c.name == col_name) {
                        conkey.push(Value::Int64((pos + 1) as i64));
                    }
                }
                let constraintdef = format!("UNIQUE ({})", idx.columns.join(", "));
                let conindid =
                    catalog_oids::pg_class_index_oid(schema.table_id, idx.id).unwrap_or(0);

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
                    int_val(conindid),
                    int_val(0),
                    Value::Boolean(true),
                    Value::Boolean(true),
                    int_val(0),
                    Value::Boolean(true),
                ]));
                constraint_oid += 1;
            }

            rows.extend(build_not_null_constraint_rows(
                schema,
                &table_short_name,
                connamespace_oid,
                conrelid,
                &mut constraint_oid,
            ));

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
                    int_val(0),
                    int_val(0),
                    Value::Boolean(false),
                    Value::Boolean(true),
                    int_val(0),
                    Value::Boolean(true),
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

                // PostgreSQL format: no space before `(` after table name in REFERENCES.
                // SQLAlchemy FK regex requires `REFERENCES <table>(<cols>)`.
                // Preserve schema prefix for cross-schema FKs (PG behaviour).
                let (ref_schema_name, ref_short_name) = split_schema_and_name(&ref_table);
                let ref_display = if ref_schema_name != table_schema {
                    format!("{}.{}", ref_schema_name, ref_short_name)
                } else {
                    ref_short_name.to_string()
                };
                let mut constraintdef = format!(
                    "FOREIGN KEY ({}) REFERENCES {}({})",
                    fk.columns.join(", "),
                    ref_display,
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
                    int_val(0),
                    int_val(0),
                    Value::Boolean(true),
                    Value::Boolean(true),
                    int_val(0),
                    Value::Boolean(true),
                ]));
                constraint_oid += 1;
            }
        }

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::build_not_null_constraint_rows;
    use super::is_unique_constraint_index;
    use super::PgConstraint;
    use super::VirtualTable;
    use crate::model::DataType;
    use crate::model::IndexDef;
    use crate::model::TableSchema;
    use crate::model::Value;
    use crate::worker::types::IndexState;

    #[test]
    fn unique_indexes_only_surface_as_constraints_with_constraint_bit() {
        let mut idx = IndexDef {
            name: "uq_idx".to_string(),
            id: 1,
            columns: vec!["a".to_string()],
            unique: true,
            is_constraint: false,
            method: Some("btree".to_string()),
            predicate: None,
            expressions: vec![],
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        assert!(!is_unique_constraint_index(&idx));

        idx.is_constraint = true;
        assert!(is_unique_constraint_index(&idx));
    }

    #[test]
    fn pg_constraint_has_psql18_describe_columns() {
        let schema = PgConstraint.schema();
        let conparentid = schema
            .columns
            .iter()
            .find(|c| c.name == "conparentid")
            .expect("pg_constraint.conparentid column must exist");
        assert_eq!(conparentid.data_type, DataType::Int64);

        let connoinherit = schema
            .columns
            .iter()
            .find(|c| c.name == "connoinherit")
            .expect("pg_constraint.connoinherit column must exist");
        assert_eq!(connoinherit.data_type, DataType::Boolean);

        let conislocal = schema
            .columns
            .iter()
            .find(|c| c.name == "conislocal")
            .expect("pg_constraint.conislocal column must exist");
        assert_eq!(conislocal.data_type, DataType::Boolean);

        let coninhcount = schema
            .columns
            .iter()
            .find(|c| c.name == "coninhcount")
            .expect("pg_constraint.coninhcount column must exist");
        assert_eq!(coninhcount.data_type, DataType::Int64);

        let convalidated = schema
            .columns
            .iter()
            .find(|c| c.name == "convalidated")
            .expect("pg_constraint.convalidated column must exist");
        assert_eq!(convalidated.data_type, DataType::Boolean);
    }

    #[test]
    fn not_null_constraints_emit_contype_n_rows_with_psql18_flags() {
        let schema = TableSchema {
            table_id: 42,
            name: "t".to_string(),
            columns: vec![
                crate::model::ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                crate::model::ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                crate::model::ColumnDef {
                    name: "score".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let mut oid = 1;
        let rows = build_not_null_constraint_rows(&schema, "t", 2200, 12345, &mut oid);
        assert_eq!(rows.len(), 2);
        assert_eq!(oid, 3);

        // Row layout: oid, conname, connamespace, contype, conrelid, ...
        assert_eq!(rows[0].values[3], Value::Text("n".to_string()));
        assert_eq!(
            rows[0].values[7],
            Value::Array(vec![Value::Int64(1)]),
            "first NOT NULL row should reference attnum 1"
        );
        assert_eq!(rows[0].values[16], Value::Boolean(false)); // connoinherit
        assert_eq!(rows[0].values[17], Value::Boolean(true)); // conislocal
        assert_eq!(rows[0].values[18], Value::Int64(0)); // coninhcount
        assert_eq!(rows[0].values[19], Value::Boolean(true)); // convalidated

        assert_eq!(rows[1].values[3], Value::Text("n".to_string()));
        assert_eq!(
            rows[1].values[7],
            Value::Array(vec![Value::Int64(2)]),
            "second NOT NULL row should reference attnum 2"
        );
    }
}
