use super::helpers::{bool_col, int_col, int_val, name_col, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::catalog_oids;
use crate::sql::pg_types;
use anyhow::Result;
use async_trait::async_trait;

pub struct PgAttribute;

const SYSTEM_ATTRIBUTE_ROWS: [(&str, i64, i64, i64); 6] = [
    ("tableoid", pg_types::OID_OID, -6, 4),
    ("cmax", pg_types::OID_CID, -5, 4),
    ("xmax", pg_types::OID_XID, -4, 4),
    ("cmin", pg_types::OID_CID, -3, 4),
    ("xmin", pg_types::OID_XID, -2, 4),
    ("ctid", pg_types::OID_TID, -1, 6),
];

fn push_system_attribute_rows(rows: &mut Vec<Row>, attrelid: i64) {
    for (attname, type_oid, attnum, attlen) in SYSTEM_ATTRIBUTE_ROWS {
        rows.push(Row::new(vec![
            int_val(attrelid),
            text_val(attname),
            int_val(type_oid),
            int_val(attnum),
            int_val(attlen),
            int_val(0),            // attndims
            text_val("p"),         // attstorage: plain
            text_val(""),          // attcompression
            Value::Boolean(true),  // attnotnull
            Value::Boolean(false), // atthasdef
            Value::Boolean(false), // attisdropped
            Value::Boolean(true),  // attislocal
            int_val(-1),           // atttypmod
            int_val(0),            // attinhcount
            int_val(0),            // attcollation
            int_val(-1),           // attstattarget
            text_val(""),          // attgenerated
            text_val(""),          // attidentity
        ]));
    }
}

fn atttypmod_for_datatype(data_type: &DataType) -> i64 {
    match data_type {
        DataType::Varchar(0) => -1,
        DataType::Varchar(n) => *n as i64 + 4,
        DataType::Numeric {
            precision: Some(p),
            scale: Some(s),
        } => ((*p as i64) << 16) | ((*s as i64) + 4),
        _ => -1,
    }
}

fn attcollation_for_datatype(data_type: &DataType) -> i64 {
    match data_type {
        DataType::Text | DataType::Varchar(_) | DataType::Name => 100,
        DataType::Array(inner) => match inner.as_ref() {
            DataType::Text | DataType::Varchar(_) | DataType::Name => 100,
            _ => 0,
        },
        _ => 0,
    }
}

fn push_attribute_row(
    rows: &mut Vec<Row>,
    attrelid: i64,
    attname: &str,
    atttypid: i64,
    attnum: i64,
    attlen: i64,
    attnotnull: bool,
    atthasdef: bool,
    atttypmod: i64,
    attcollation: i64,
    attgenerated: &str,
) {
    rows.push(Row::new(vec![
        int_val(attrelid),
        text_val(attname),
        int_val(atttypid),
        int_val(attnum),
        int_val(attlen),
        int_val(0),    // attndims
        text_val("x"), // attstorage: extended
        text_val(""),  // attcompression
        Value::Boolean(attnotnull),
        Value::Boolean(atthasdef),
        Value::Boolean(false), // attisdropped
        Value::Boolean(true),  // attislocal
        int_val(atttypmod),
        int_val(0), // attinhcount
        int_val(attcollation),
        int_val(-1), // attstattarget
        text_val(attgenerated),
        text_val(""), // attidentity
    ]));
}

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
                int_col("attndims"),
                text_col("attstorage"),
                text_col("attcompression"),
                bool_col("attnotnull"),
                bool_col("atthasdef"),
                bool_col("attisdropped"),
                bool_col("attislocal"),
                int_col("atttypmod"),
                int_col("attinhcount"),
                int_col("attcollation"),
                int_col("attstattarget"),
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
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut rows = Vec::new();

        for table_name in ctx.user_tables {
            if let Some(schema) = ctx.store.get_schema(ctx.txn, ctx.db_id, table_name).await? {
                let base_table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
                push_system_attribute_rows(&mut rows, base_table_oid);

                for (i, col) in schema.columns.iter().enumerate() {
                    let (type_oid, attlen) = if let DataType::UserDefined(udt_name) = &col.data_type
                    {
                        if udt_name == "char" {
                            (pg_types::OID_CHAR, 1)
                        } else {
                            let oid = ctx
                                .store
                                .get_type(ctx.txn, ctx.db_id, udt_name)
                                .await?
                                .map(|t| t.oid as i64)
                                .unwrap_or(25);
                            (oid, 4)
                        }
                    } else {
                        let (oid, typlen) = pg_types::oid_and_typlen_for_datatype(&col.data_type);
                        (oid, typlen as i64)
                    };

                    push_attribute_row(
                        &mut rows,
                        base_table_oid,
                        &col.name,
                        type_oid,
                        (i + 1) as i64,
                        attlen,
                        !col.nullable,
                        col.is_serial || col.default_expr.is_some(),
                        atttypmod_for_datatype(&col.data_type),
                        attcollation_for_datatype(&col.data_type),
                        if col.generation_expr.is_some() {
                            "s"
                        } else {
                            ""
                        },
                    );
                }
            }
        }

        let mut virtual_tables: Vec<&dyn VirtualTable> =
            crate::sql::catalog::global_catalog().iter().collect();
        virtual_tables.sort_by(|lhs, rhs| {
            lhs.schema_name()
                .cmp(rhs.schema_name())
                .then(lhs.name().cmp(rhs.name()))
        });

        for table in virtual_tables {
            let Some(attrelid) =
                crate::sql::catalog::catalog_relation_oid(table.schema_name(), table.name())
            else {
                continue;
            };
            let relation_schema = table.schema();
            push_system_attribute_rows(&mut rows, attrelid);

            for (i, col) in relation_schema.columns.iter().enumerate() {
                let (type_oid, attlen) = pg_types::oid_and_typlen_for_datatype(&col.data_type);
                push_attribute_row(
                    &mut rows,
                    attrelid,
                    &col.name,
                    type_oid,
                    (i + 1) as i64,
                    attlen as i64,
                    !col.nullable,
                    false,
                    atttypmod_for_datatype(&col.data_type),
                    attcollation_for_datatype(&col.data_type),
                    "",
                );
            }
        }

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::atttypmod_for_datatype;
    use crate::model::DataType;

    #[test]
    fn bare_varchar_has_no_typmod() {
        assert_eq!(atttypmod_for_datatype(&DataType::Varchar(0)), -1);
        assert_eq!(atttypmod_for_datatype(&DataType::Varchar(5)), 9);
    }
}
