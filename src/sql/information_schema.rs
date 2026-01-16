use super::{names, sequences};
use crate::storage::TikvStore;
use crate::types::{ColumnDef, DataType, ForeignKeyAction, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

#[allow(dead_code)]
pub fn is_information_schema_table(table_name: &str) -> bool {
    let lower = table_name.to_lowercase();
    lower.starts_with("information_schema.")
        || lower.starts_with("pg_catalog.")
        || matches!(
            lower.as_str(),
            "tables"
                | "columns"
                | "schemata"
                | "table_constraints"
                | "key_column_usage"
                | "referential_constraints"
                | "constraint_column_usage"
                | "check_constraints"
                | "pg_range"
                | "pg_type"
                | "pg_enum"
                | "pg_class"
                | "pg_index"
                | "pg_attribute"
                | "pg_namespace"
                | "pg_proc"
                | "pg_description"
                | "pg_constraint"
                | "pg_am"
                | "pg_indexes"
        )
}

#[allow(dead_code)]
pub fn parse_information_schema_table(table_name: &str) -> Option<&str> {
    let lower = table_name.to_lowercase();
    if let Some(name) = lower.strip_prefix("information_schema.") {
        return Some(match name {
            "tables" => "tables",
            "columns" => "columns",
            "schemata" => "schemata",
            "table_constraints" => "table_constraints",
            "key_column_usage" => "key_column_usage",
            "referential_constraints" => "referential_constraints",
            "constraint_column_usage" => "constraint_column_usage",
            "check_constraints" => "check_constraints",
            _ => return None,
        });
    }
    if let Some(name) = lower.strip_prefix("pg_catalog.") {
        return Some(match name {
            "pg_range" => "pg_range",
            "pg_type" => "pg_type",
            "pg_enum" => "pg_enum",
            "pg_class" => "pg_class",
            "pg_index" => "pg_index",
            "pg_attribute" => "pg_attribute",
            "pg_namespace" => "pg_namespace",
            "pg_proc" => "pg_proc",
            "pg_description" => "pg_description",
            "pg_constraint" => "pg_constraint",
            "pg_am" => "pg_am",
            "pg_indexes" => "pg_indexes",
            _ => return None,
        });
    }

    // Handle unqualified catalog table names
    match lower.as_str() {
        "pg_class" => Some("pg_class"),
        "pg_index" => Some("pg_index"),
        "pg_attribute" => Some("pg_attribute"),
        "pg_namespace" => Some("pg_namespace"),
        "pg_proc" => Some("pg_proc"),
        "pg_description" => Some("pg_description"),
        "pg_constraint" => Some("pg_constraint"),
        "pg_am" => Some("pg_am"),
        "pg_indexes" => Some("pg_indexes"),
        "pg_enum" => Some("pg_enum"),
        _ => None,
    }
}

fn text_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Text,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
    }
}

fn int_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Int64,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
    }
}

fn float_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Float64,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
    }
}

fn bool_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Boolean,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
    }
}

fn int_array_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: DataType::Array(Box::new(DataType::Int64)),
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
    }
}

fn tables_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "tables".to_string(),
        columns: vec![
            text_col("table_catalog"),
            text_col("table_schema"),
            text_col("table_name"),
            text_col("table_type"),
            text_col("self_referencing_column_name"),
            text_col("reference_generation"),
            text_col("user_defined_type_catalog"),
            text_col("user_defined_type_schema"),
            text_col("user_defined_type_name"),
            text_col("is_insertable_into"),
            text_col("is_typed"),
            text_col("commit_action"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn columns_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "columns".to_string(),
        columns: vec![
            text_col("table_catalog"),
            text_col("table_schema"),
            text_col("table_name"),
            text_col("column_name"),
            int_col("ordinal_position"),
            text_col("column_default"),
            text_col("is_nullable"),
            text_col("data_type"),
            int_col("character_maximum_length"),
            int_col("character_octet_length"),
            int_col("numeric_precision"),
            int_col("numeric_precision_radix"),
            int_col("numeric_scale"),
            int_col("datetime_precision"),
            text_col("interval_type"),
            int_col("interval_precision"),
            text_col("character_set_catalog"),
            text_col("character_set_schema"),
            text_col("character_set_name"),
            text_col("collation_catalog"),
            text_col("collation_schema"),
            text_col("collation_name"),
            text_col("domain_catalog"),
            text_col("domain_schema"),
            text_col("domain_name"),
            text_col("udt_catalog"),
            text_col("udt_schema"),
            text_col("udt_name"),
            text_col("scope_catalog"),
            text_col("scope_schema"),
            text_col("scope_name"),
            int_col("maximum_cardinality"),
            text_col("dtd_identifier"),
            text_col("is_self_referencing"),
            text_col("is_identity"),
            text_col("identity_generation"),
            text_col("identity_start"),
            text_col("identity_increment"),
            text_col("identity_maximum"),
            text_col("identity_minimum"),
            text_col("identity_cycle"),
            text_col("is_generated"),
            text_col("generation_expression"),
            text_col("is_updatable"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn schemata_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "schemata".to_string(),
        columns: vec![
            text_col("catalog_name"),
            text_col("schema_name"),
            text_col("schema_owner"),
            text_col("default_character_set_catalog"),
            text_col("default_character_set_schema"),
            text_col("default_character_set_name"),
            text_col("sql_path"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn table_constraints_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "table_constraints".to_string(),
        columns: vec![
            text_col("constraint_catalog"),
            text_col("constraint_schema"),
            text_col("constraint_name"),
            text_col("table_catalog"),
            text_col("table_schema"),
            text_col("table_name"),
            text_col("constraint_type"),
            text_col("is_deferrable"),
            text_col("initially_deferred"),
            text_col("enforced"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn key_column_usage_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "key_column_usage".to_string(),
        columns: vec![
            text_col("constraint_catalog"),
            text_col("constraint_schema"),
            text_col("constraint_name"),
            text_col("table_catalog"),
            text_col("table_schema"),
            text_col("table_name"),
            text_col("column_name"),
            int_col("ordinal_position"),
            int_col("position_in_unique_constraint"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn referential_constraints_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "referential_constraints".to_string(),
        columns: vec![
            text_col("constraint_catalog"),
            text_col("constraint_schema"),
            text_col("constraint_name"),
            text_col("unique_constraint_catalog"),
            text_col("unique_constraint_schema"),
            text_col("unique_constraint_name"),
            text_col("match_option"),
            text_col("update_rule"),
            text_col("delete_rule"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn constraint_column_usage_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "constraint_column_usage".to_string(),
        columns: vec![
            text_col("table_catalog"),
            text_col("table_schema"),
            text_col("table_name"),
            text_col("column_name"),
            text_col("constraint_catalog"),
            text_col("constraint_schema"),
            text_col("constraint_name"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn check_constraints_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "check_constraints".to_string(),
        columns: vec![
            text_col("constraint_catalog"),
            text_col("constraint_schema"),
            text_col("constraint_name"),
            text_col("check_clause"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_range_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_range".to_string(),
        columns: vec![
            int_col("rngtypid"),
            int_col("rngsubtype"),
            int_col("rngcollation"),
            int_col("rngsubopc"),
            text_col("rngcanonical"),
            text_col("rngsubdiff"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_type_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_type".to_string(),
        columns: vec![
            int_col("oid"),
            text_col("typname"),
            int_col("typnamespace"),
            int_col("typowner"),
            int_col("typlen"),
            text_col("typbyval"),
            text_col("typtype"),
            text_col("typcategory"),
            text_col("typispreferred"),
            text_col("typisdefined"),
            text_col("typdelim"),
            int_col("typrelid"),
            int_col("typelem"),
            int_col("typarray"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_enum_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_enum".to_string(),
        columns: vec![
            int_col("oid"),
            int_col("enumtypid"),
            float_col("enumsortorder"),
            text_col("enumlabel"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_class_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_class".to_string(),
        columns: vec![
            int_col("oid"),
            text_col("relname"),
            int_col("relnamespace"),
            text_col("relkind"),
            int_col("relowner"),
            int_col("relam"),
            int_col("reltuples"),
            int_col("relpages"),
            text_col("relhasindex"),
            text_col("relispopulated"),
            text_col("relreplident"),
            text_col("relispartition"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_index_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_index".to_string(),
        columns: vec![
            int_col("indexrelid"),
            int_col("indrelid"),
            int_col("indnatts"),
            text_col("indisunique"),
            text_col("indisprimary"),
            text_col("indisexclusion"),
            text_col("indimmediate"),
            text_col("indisclustered"),
            text_col("indisvalid"),
            int_array_col("indkey"),
            text_col("indpred"),
            text_col("indexdef"), // Pre-computed index definition for pg_get_indexdef()
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_attribute_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_attribute".to_string(),
        columns: vec![
            int_col("attrelid"),
            text_col("attname"),
            int_col("atttypid"),
            int_col("attnum"),
            int_col("attlen"),
            text_col("attnotnull"),
            text_col("atthasdef"),
            text_col("attisdropped"),
            text_col("attislocal"),
            int_col("atttypmod"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_namespace_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_namespace".to_string(),
        columns: vec![int_col("oid"), text_col("nspname"), int_col("nspowner")],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_proc_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_proc".to_string(),
        columns: vec![
            int_col("oid"),
            text_col("proname"),
            int_col("pronamespace"),
            int_col("proowner"),
            int_col("prorettype"),
            text_col("prokind"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_constraint_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_constraint".to_string(),
        columns: vec![
            int_col("oid"),
            text_col("conname"),
            int_col("connamespace"),
            text_col("contype"),
            int_col("conrelid"),
            int_col("confrelid"),
            int_array_col("conkey"),
            int_array_col("confkey"),
            text_col("confdeltype"),
            text_col("confupdtype"),
            bool_col("condeferrable"),
            bool_col("condeferred"),
            text_col("constraintdef"), // Pre-computed definition for pg_get_constraintdef()
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_am_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_am".to_string(),
        columns: vec![int_col("oid"), text_col("amname")],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_description_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_description".to_string(),
        columns: vec![
            int_col("objoid"),
            int_col("classoid"),
            int_col("objsubid"),
            text_col("description"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

fn pg_indexes_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "pg_indexes".to_string(),
        columns: vec![
            text_col("schemaname"),
            text_col("tablename"),
            text_col("indexname"),
            text_col("tablespace"),
            text_col("indexdef"),
        ],
        version: 1,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    }
}

pub fn get_information_schema_schema(table_name: &str) -> Option<TableSchema> {
    let lower = table_name.to_lowercase();
    let name = lower
        .strip_prefix("information_schema.")
        .or_else(|| lower.strip_prefix("pg_catalog."))
        .unwrap_or(&lower);

    match name {
        "tables" => Some(tables_schema()),
        "columns" => Some(columns_schema()),
        "schemata" => Some(schemata_schema()),
        "table_constraints" => Some(table_constraints_schema()),
        "key_column_usage" => Some(key_column_usage_schema()),
        "referential_constraints" => Some(referential_constraints_schema()),
        "constraint_column_usage" => Some(constraint_column_usage_schema()),
        "check_constraints" => Some(check_constraints_schema()),
        "pg_range" => Some(pg_range_schema()),
        "pg_type" => Some(pg_type_schema()),
        "pg_enum" => Some(pg_enum_schema()),
        "pg_class" => Some(pg_class_schema()),
        "pg_index" => Some(pg_index_schema()),
        "pg_attribute" => Some(pg_attribute_schema()),
        "pg_namespace" => Some(pg_namespace_schema()),
        "pg_proc" => Some(pg_proc_schema()),
        "pg_description" => Some(pg_description_schema()),
        "pg_constraint" => Some(pg_constraint_schema()),
        "pg_am" => Some(pg_am_schema()),
        "pg_indexes" => Some(pg_indexes_schema()),
        _ => None,
    }
}

fn data_type_to_pg_type(dt: &DataType) -> &'static str {
    match dt {
        DataType::Boolean => "boolean",
        DataType::Int32 => "integer",
        DataType::Int64 => "bigint",
        DataType::Float64 => "double precision",
        DataType::Text => "character varying",
        DataType::Bytes => "bytea",
        DataType::Timestamp => "timestamp without time zone",
        DataType::Date => "date",
        DataType::Interval => "interval",
        DataType::Uuid => "uuid",
        DataType::Array(inner) => match inner.as_ref() {
            DataType::Int32 => "integer[]",
            DataType::Int64 => "bigint[]",
            DataType::Text => "character varying[]",
            _ => "anyarray",
        },
        DataType::Json => "json",
        DataType::Jsonb => "jsonb",
        DataType::Vector(_) => "vector",
        DataType::Time => "time without time zone",
        DataType::UserDefined(_) => "character varying",
    }
}

fn text_val(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn null_val() -> Value {
    Value::Null
}

fn int_val(i: i64) -> Value {
    Value::Int64(i)
}

fn float_val(f: f64) -> Value {
    Value::Float64(f)
}

fn split_schema_and_name(full: &str) -> (String, String) {
    match names::parse_full_name(full) {
        Ok((schema, name)) => (schema, name),
        Err(_) => ("public".to_string(), full.to_string()),
    }
}

fn build_schema_oid_map(schemas: &[String]) -> HashMap<String, i64> {
    let mut map: HashMap<String, i64> = HashMap::new();
    map.insert("pg_catalog".to_string(), 11);
    map.insert("public".to_string(), 2200);
    map.insert("information_schema".to_string(), 13222);

    let mut next_oid: i64 = 20000;
    let mut schemas_sorted = schemas.to_vec();
    schemas_sorted.sort();
    schemas_sorted.dedup();
    for schema in schemas_sorted {
        if map.contains_key(&schema) {
            continue;
        }
        map.insert(schema, next_oid);
        next_oid += 1;
    }
    map
}

fn schema_oid(schema_oids: &HashMap<String, i64>, schema: &str) -> i64 {
    schema_oids.get(schema).copied().unwrap_or(2200)
}

pub async fn get_information_schema_data(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    table_name: &str,
) -> Result<(TableSchema, Vec<Row>)> {
    let lower = table_name.to_lowercase();
    let name = lower
        .strip_prefix("information_schema.")
        .or_else(|| lower.strip_prefix("pg_catalog."))
        .unwrap_or(&lower);

    let schema =
        get_information_schema_schema(table_name).ok_or_else(|| anyhow!("Unknown table"))?;

    let user_tables = store.list_tables(txn).await?;
    let schemas = store.list_schemas(txn).await?;
    let schema_oids = build_schema_oid_map(&schemas);

    let rows = match name {
        "schemata" => get_schemata_rows(&schemas),
        "tables" => get_tables_rows(store, txn, &user_tables).await?,
        "columns" => get_columns_rows(store, txn, &user_tables).await?,
        "table_constraints" => get_table_constraints_rows(store, txn, &user_tables).await?,
        "key_column_usage" => get_key_column_usage_rows(store, txn, &user_tables).await?,
        "referential_constraints" => {
            get_referential_constraints_rows(store, txn, &user_tables).await?
        }
        "constraint_column_usage" => {
            get_constraint_column_usage_rows(store, txn, &user_tables).await?
        }
        "check_constraints" => get_check_constraints_rows(store, txn, &user_tables).await?,
        "pg_range" => vec![], // No range types defined
        "pg_type" => get_pg_type_rows(store, txn, &schema_oids).await?,
        "pg_enum" => get_pg_enum_rows(store, txn).await?,
        "pg_namespace" => get_pg_namespace_rows(&schemas, &schema_oids),
        "pg_class" => get_pg_class_rows(store, txn, &user_tables, &schema_oids).await?,
        "pg_index" => get_pg_index_rows(store, txn, &user_tables, &schema_oids).await?,
        "pg_attribute" => get_pg_attribute_rows(store, txn, &user_tables).await?,
        "pg_proc" => get_pg_proc_rows(),
        "pg_description" => get_pg_description_rows(),
        "pg_constraint" => get_pg_constraint_rows(store, txn, &user_tables, &schema_oids).await?,
        "pg_am" => get_pg_am_rows(),
        "pg_indexes" => get_pg_indexes_rows(store, txn, &user_tables).await?,
        _ => vec![],
    };

    Ok((schema, rows))
}

fn get_schemata_rows(schemas: &[String]) -> Vec<Row> {
    schemas
        .iter()
        .map(|schema| {
            Row::new(vec![
                text_val("postgres"),
                text_val(schema),
                text_val("postgres"),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
            ])
        })
        .collect()
}

async fn get_tables_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        rows.push(Row::new(vec![
            text_val("postgres"),
            text_val(&table_schema),
            text_val(&table_name),
            text_val("BASE TABLE"),
            null_val(),
            null_val(),
            null_val(),
            null_val(),
            null_val(),
            text_val("YES"),
            text_val("NO"),
            null_val(),
        ]));
    }

    let views = store.list_views(txn).await.unwrap_or_default();
    for full_view_name in views {
        let (view_schema, view_name) = split_schema_and_name(&full_view_name);
        rows.push(Row::new(vec![
            text_val("postgres"),
            text_val(&view_schema),
            text_val(&view_name),
            text_val("VIEW"),
            null_val(),
            null_val(),
            null_val(),
            null_val(),
            null_val(),
            text_val("NO"),
            text_val("NO"),
            null_val(),
        ]));
    }

    Ok(rows)
}

async fn get_columns_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        if let Some(schema) = store.get_schema(txn, full_table_name).await? {
            for (i, col) in schema.columns.iter().enumerate() {
                let (data_type_str, udt_schema, udt_name) = match &col.data_type {
                    DataType::UserDefined(full_udt) => {
                        let (schema_name, type_name) = full_udt
                            .rsplit_once('.')
                            .unwrap_or(("public", full_udt.as_str()));
                        ("USER-DEFINED", schema_name, type_name)
                    }
                    _ => {
                        let pg_type = data_type_to_pg_type(&col.data_type);
                        (pg_type, "pg_catalog", pg_type)
                    }
                };
                let is_nullable = if col.nullable { "YES" } else { "NO" };
                let ordinal = (i + 1) as i64;

                let (char_max_len, num_precision, num_scale) = match col.data_type {
                    DataType::Int32 => (null_val(), int_val(32), int_val(0)),
                    DataType::Int64 => (null_val(), int_val(64), int_val(0)),
                    DataType::Float64 => (null_val(), int_val(53), null_val()),
                    DataType::Text => (null_val(), null_val(), null_val()),
                    _ => (null_val(), null_val(), null_val()),
                };

                let column_default = if col.is_serial {
                    let seq_full_name = format!(
                        "{}.{}",
                        table_schema,
                        sequences::implicit_sequence_name(&table_name, &col.name)
                    );
                    text_val(&format!("nextval('{}'::regclass)", seq_full_name))
                } else {
                    col.default_expr
                        .as_ref()
                        .map(|s| text_val(s))
                        .unwrap_or(null_val())
                };

                rows.push(Row::new(vec![
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&table_name),
                    text_val(&col.name),
                    int_val(ordinal),
                    column_default,
                    text_val(is_nullable),
                    text_val(data_type_str),
                    char_max_len,
                    null_val(),
                    num_precision,
                    int_val(2),
                    num_scale,
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    text_val("postgres"),
                    text_val(udt_schema),
                    text_val(udt_name),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    text_val(&ordinal.to_string()),
                    text_val("NO"),
                    text_val("NO"),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                    text_val("NEVER"),
                    null_val(),
                    text_val("YES"),
                ]));
            }
        }
    }

    Ok(rows)
}

async fn get_table_constraints_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        if let Some(table_def) = store.get_schema(txn, full_table_name).await? {
            if !table_def.pk_indices.is_empty() {
                let pk_name = format!("{}_pkey", table_name);
                rows.push(Row::new(vec![
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&pk_name),
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&table_name),
                    text_val("PRIMARY KEY"),
                    text_val("NO"),
                    text_val("NO"),
                    text_val("YES"),
                ]));
            }

            for idx in &table_def.indexes {
                if idx.unique {
                    rows.push(Row::new(vec![
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&idx.name),
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val("UNIQUE"),
                        text_val("NO"),
                        text_val("NO"),
                        text_val("YES"),
                    ]));
                }
            }

            for col in &table_def.columns {
                if col.unique && !col.primary_key {
                    let constraint_name = format!("{}_{}_key", table_name, col.name);
                    rows.push(Row::new(vec![
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&constraint_name),
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val("UNIQUE"),
                        text_val("NO"),
                        text_val("NO"),
                        text_val("YES"),
                    ]));
                }
            }

            for fk in &table_def.foreign_keys {
                rows.push(Row::new(vec![
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&fk.name),
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&table_name),
                    text_val("FOREIGN KEY"),
                    text_val("NO"),
                    text_val("NO"),
                    text_val("YES"),
                ]));
            }

            for (i, check) in table_def.check_constraints.iter().enumerate() {
                let name = check
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_check{}", table_name, i + 1));
                rows.push(Row::new(vec![
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&name),
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&table_name),
                    text_val("CHECK"),
                    text_val("NO"),
                    text_val("NO"),
                    text_val("YES"),
                ]));
            }
        }
    }

    Ok(rows)
}

async fn get_key_column_usage_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        if let Some(table_def) = store.get_schema(txn, full_table_name).await? {
            if !table_def.pk_indices.is_empty() {
                let pk_name = format!("{}_pkey", table_name);
                for (i, &col_idx) in table_def.pk_indices.iter().enumerate() {
                    let col_name = &table_def.columns[col_idx].name;
                    rows.push(Row::new(vec![
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&pk_name),
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val(col_name),
                        int_val((i + 1) as i64),
                        null_val(),
                    ]));
                }
            }

            for idx in &table_def.indexes {
                if idx.unique {
                    for (i, col_name) in idx.columns.iter().enumerate() {
                        rows.push(Row::new(vec![
                            text_val("postgres"),
                            text_val(&table_schema),
                            text_val(&idx.name),
                            text_val("postgres"),
                            text_val(&table_schema),
                            text_val(&table_name),
                            text_val(col_name),
                            int_val((i + 1) as i64),
                            null_val(),
                        ]));
                    }
                }
            }

            for fk in &table_def.foreign_keys {
                for (i, col_name) in fk.columns.iter().enumerate() {
                    rows.push(Row::new(vec![
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&fk.name),
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val(col_name),
                        int_val((i + 1) as i64),
                        int_val((i + 1) as i64),
                    ]));
                }
            }
        }
    }

    Ok(rows)
}

async fn get_referential_constraints_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, _) = split_schema_and_name(full_table_name);
        if let Some(table_def) = store.get_schema(txn, full_table_name).await? {
            for fk in &table_def.foreign_keys {
                let (ref_schema, ref_table_name) = split_schema_and_name(&fk.ref_table);
                let ref_pk_name = format!("{}_pkey", ref_table_name);
                let update_rule = match fk.on_update {
                    crate::types::ForeignKeyAction::Cascade => "CASCADE",
                    crate::types::ForeignKeyAction::SetNull => "SET NULL",
                    crate::types::ForeignKeyAction::SetDefault => "SET DEFAULT",
                    crate::types::ForeignKeyAction::Restrict => "RESTRICT",
                    crate::types::ForeignKeyAction::NoAction => "NO ACTION",
                };
                let delete_rule = match fk.on_delete {
                    crate::types::ForeignKeyAction::Cascade => "CASCADE",
                    crate::types::ForeignKeyAction::SetNull => "SET NULL",
                    crate::types::ForeignKeyAction::SetDefault => "SET DEFAULT",
                    crate::types::ForeignKeyAction::Restrict => "RESTRICT",
                    crate::types::ForeignKeyAction::NoAction => "NO ACTION",
                };
                rows.push(Row::new(vec![
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&fk.name),
                    text_val("postgres"),
                    text_val(&ref_schema),
                    text_val(&ref_pk_name),
                    text_val("NONE"),
                    text_val(update_rule),
                    text_val(delete_rule),
                ]));
            }
        }
    }

    Ok(rows)
}

async fn get_constraint_column_usage_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        if let Some(table_def) = store.get_schema(txn, full_table_name).await? {
            if !table_def.pk_indices.is_empty() {
                let pk_name = format!("{}_pkey", table_name);
                for &col_idx in &table_def.pk_indices {
                    let col_name = &table_def.columns[col_idx].name;
                    rows.push(Row::new(vec![
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val(col_name),
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&pk_name),
                    ]));
                }
            }

            for idx in &table_def.indexes {
                if idx.unique {
                    for col_name in &idx.columns {
                        rows.push(Row::new(vec![
                            text_val("postgres"),
                            text_val(&table_schema),
                            text_val(&table_name),
                            text_val(col_name),
                            text_val("postgres"),
                            text_val(&table_schema),
                            text_val(&idx.name),
                        ]));
                    }
                }
            }

            for fk in &table_def.foreign_keys {
                let (ref_schema, ref_table_name) = split_schema_and_name(&fk.ref_table);
                for col_name in &fk.ref_columns {
                    rows.push(Row::new(vec![
                        text_val("postgres"),
                        text_val(&ref_schema),
                        text_val(&ref_table_name),
                        text_val(col_name),
                        text_val("postgres"),
                        text_val(&table_schema),
                        text_val(&fk.name),
                    ]));
                }
            }
        }
    }

    Ok(rows)
}

async fn get_check_constraints_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        if let Some(table_def) = store.get_schema(txn, full_table_name).await? {
            for (i, check) in table_def.check_constraints.iter().enumerate() {
                let name = check
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_check{}", table_name, i + 1));
                rows.push(Row::new(vec![
                    text_val("postgres"),
                    text_val(&table_schema),
                    text_val(&name),
                    text_val(&check.expr),
                ]));
            }
        }
    }

    Ok(rows)
}

fn get_pg_namespace_rows(schemas: &[String], schema_oids: &HashMap<String, i64>) -> Vec<Row> {
    schemas
        .iter()
        .map(|schema| {
            Row::new(vec![
                int_val(schema_oid(schema_oids, schema)),
                text_val(schema),
                int_val(10),
            ])
        })
        .collect()
}

async fn get_pg_class_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
    schema_oids: &HashMap<String, i64>,
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut oid_counter = 16384; // Start from standard PostgreSQL user object OID

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        let namespace_oid = schema_oid(schema_oids, &table_schema);
        if let Some(schema) = store.get_schema(txn, full_table_name).await? {
            let table_oid = oid_counter;
            oid_counter += 1;

            // Add the table itself
            rows.push(Row::new(vec![
                int_val(table_oid),
                text_val(&table_name),
                int_val(namespace_oid),
                text_val("r"), // r = ordinary table
                int_val(10),   // owner
                int_val(0),    // access method
                int_val(0),    // tuples (unknown)
                int_val(0),    // pages (unknown)
                text_val("t"), // has index (true if any indexes)
                text_val("t"), // is populated
                text_val("d"), // replica identity (default)
                text_val("f"), // is partition (false)
            ]));

            // Add indexes as separate entries
            for idx in &schema.indexes {
                let index_oid = oid_counter;
                oid_counter += 1;
                rows.push(Row::new(vec![
                    int_val(index_oid),
                    text_val(&idx.name),
                    int_val(namespace_oid),
                    text_val("i"), // i = index
                    int_val(10),   // owner
                    int_val(403),  // btree access method
                    int_val(0),    // tuples
                    int_val(0),    // pages
                    text_val("f"), // has index (false)
                    text_val("t"), // is populated
                    text_val("d"), // replica identity
                    text_val("f"), // is partition
                ]));
            }

            // Add primary key index if exists
            if !schema.pk_indices.is_empty() {
                let pk_name = format!("{}_pkey", table_name);
                let pk_oid = oid_counter;
                oid_counter += 1;
                rows.push(Row::new(vec![
                    int_val(pk_oid),
                    text_val(&pk_name),
                    int_val(namespace_oid),
                    text_val("i"),
                    int_val(10),
                    int_val(403),
                    int_val(0),
                    int_val(0),
                    text_val("f"),
                    text_val("t"),
                    text_val("d"),
                    text_val("f"),
                ]));
            }
        }
    }

    let sequences = store.list_sequences(txn).await?;
    for seq in sequences {
        let seq_oid = oid_counter;
        oid_counter += 1;
        let namespace_oid = schema_oid(schema_oids, &seq.schema);
        rows.push(Row::new(vec![
            int_val(seq_oid),
            text_val(&seq.name),
            int_val(namespace_oid),
            text_val("S"), // S = sequence
            int_val(10),   // owner
            int_val(0),    // access method
            int_val(0),    // tuples (unknown)
            int_val(0),    // pages (unknown)
            text_val("f"), // has index
            text_val("t"), // is populated
            text_val("d"), // replica identity
            text_val("f"), // is partition
        ]));
    }

    Ok(rows)
}

async fn get_pg_index_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
    _schema_oids: &HashMap<String, i64>,
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut oid_counter = 16384;

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        if let Some(schema) = store.get_schema(txn, full_table_name).await? {
            let base_table_oid = oid_counter;
            oid_counter += 1;

            for idx in &schema.indexes {
                let index_oid = oid_counter;
                oid_counter += 1;

                let mut col_indices = Vec::new();
                for col_name in &idx.columns {
                    if let Some(pos) = schema.columns.iter().position(|c| &c.name == col_name) {
                        col_indices.push(Value::Int64((pos + 1) as i64));
                    }
                }
                let indkey = Value::Array(col_indices);

                let indexdef = format!(
                    "CREATE {}INDEX {} ON {}.{} USING btree ({})",
                    if idx.unique { "UNIQUE " } else { "" },
                    idx.name,
                    table_schema,
                    table_name,
                    idx.columns.join(", ")
                );

                rows.push(Row::new(vec![
                    int_val(index_oid),
                    int_val(base_table_oid),
                    int_val(idx.columns.len() as i64),
                    text_val(if idx.unique { "t" } else { "f" }),
                    text_val("f"),
                    text_val("f"),
                    text_val("t"),
                    text_val("f"),
                    text_val("t"),
                    indkey,
                    null_val(),
                    text_val(&indexdef),
                ]));
            }

            if !schema.pk_indices.is_empty() {
                let pk_oid = oid_counter;
                oid_counter += 1;

                let indkey = schema
                    .pk_indices
                    .iter()
                    .map(|idx| Value::Int64((idx + 1) as i64))
                    .collect::<Vec<_>>();

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

                rows.push(Row::new(vec![
                    int_val(pk_oid),
                    int_val(base_table_oid),
                    int_val(schema.pk_indices.len() as i64),
                    text_val("t"),
                    text_val("t"),
                    text_val("f"),
                    text_val("t"),
                    text_val("f"),
                    text_val("t"),
                    Value::Array(indkey),
                    null_val(),
                    text_val(&indexdef),
                ]));
            }
        }
    }

    Ok(rows)
}

async fn get_pg_indexes_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    for full_table_name in user_tables {
        let (table_schema, table_name) = split_schema_and_name(full_table_name);
        if let Some(schema) = store.get_schema(txn, full_table_name).await? {
            if !schema.pk_indices.is_empty() {
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
                rows.push(Row::new(vec![
                    text_val(&table_schema),
                    text_val(&table_name),
                    text_val(&format!("{}_pkey", table_name)),
                    null_val(),
                    text_val(&indexdef),
                ]));
            }

            for idx in &schema.indexes {
                let indexdef = format!(
                    "CREATE {}INDEX {} ON {}.{} USING btree ({})",
                    if idx.unique { "UNIQUE " } else { "" },
                    idx.name,
                    table_schema,
                    table_name,
                    idx.columns.join(", ")
                );
                rows.push(Row::new(vec![
                    text_val(&table_schema),
                    text_val(&table_name),
                    text_val(&idx.name),
                    null_val(),
                    text_val(&indexdef),
                ]));
            }
        }
    }

    Ok(rows)
}

async fn get_pg_attribute_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut table_oid: i64 = 16384;

    for table_name in user_tables {
        if let Some(schema) = store.get_schema(txn, table_name).await? {
            let base_table_oid = table_oid;

            // Calculate how many OIDs this table uses (1 for table + indexes)
            let num_indexes =
                schema.indexes.len() + if !schema.pk_indices.is_empty() { 1 } else { 0 };
            table_oid += 1 + num_indexes as i64;

            for (i, col) in schema.columns.iter().enumerate() {
                let (type_oid, attlen) = if let DataType::UserDefined(udt_name) = &col.data_type {
                    let oid = store
                        .get_type(txn, udt_name)
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
                        DataType::Date => 1082,
                        DataType::Uuid => 2950,
                        DataType::Json => 114,
                        DataType::Jsonb => 3802,
                        DataType::Vector(_) => 16385, // Custom OID for vector
                        _ => 25,                      // Default to text
                    };

                    let len = match col.data_type {
                        DataType::Boolean => 1,
                        DataType::Int32 => 4,
                        DataType::Int64 => 8,
                        DataType::Float64 => 8,
                        DataType::Date => 4,
                        _ => -1, // Variable length
                    };
                    (oid, len)
                };

                rows.push(Row::new(vec![
                    int_val(base_table_oid),
                    text_val(&col.name),
                    int_val(type_oid),
                    int_val((i + 1) as i64),
                    int_val(attlen),
                    text_val(if !col.nullable { "t" } else { "f" }),
                    text_val(if col.is_serial || col.default_expr.is_some() {
                        "t"
                    } else {
                        "f"
                    }),
                    text_val("f"), // not dropped
                    text_val("t"), // is local
                    int_val(-1),   // type modifier
                ]));
            }
        }
    }

    Ok(rows)
}

fn get_pg_am_rows() -> Vec<Row> {
    vec![Row::new(vec![int_val(403), text_val("btree")])]
}

async fn get_pg_constraint_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    user_tables: &[String],
    schema_oids: &HashMap<String, i64>,
) -> Result<Vec<Row>> {
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

    let mut table_oids: HashMap<String, i64> = HashMap::new();
    let mut table_schemas: HashMap<String, TableSchema> = HashMap::new();

    let mut table_oid: i64 = 16384;
    for table_name in user_tables {
        if let Some(schema) = store.get_schema(txn, table_name).await? {
            let base_table_oid = table_oid;
            let num_indexes =
                schema.indexes.len() + if !schema.pk_indices.is_empty() { 1 } else { 0 };
            table_oid += 1 + num_indexes as i64;

            table_oids.insert(table_name.to_string(), base_table_oid);
            table_schemas.insert(table_name.to_string(), schema);
        }
    }

    let mut rows = Vec::new();
    let mut constraint_oid: i64 = 50000;

    for table_name in user_tables {
        let (table_schema, table_short_name) = split_schema_and_name(table_name);
        let connamespace_oid = schema_oid(schema_oids, &table_schema);
        let Some(schema) = table_schemas.get(table_name) else {
            continue;
        };
        let Some(&conrelid) = table_oids.get(table_name) else {
            continue;
        };

        if !schema.pk_indices.is_empty() {
            let conname = format!("{}_pkey", table_short_name);
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
            let conindid = conrelid + 1 + schema.indexes.len() as i64;

            rows.push(Row::new(vec![
                int_val(constraint_oid),
                text_val(&conname),
                int_val(connamespace_oid),
                text_val("p"),
                int_val(conrelid),
                int_val(0), // confrelid
                Value::Array(conkey),
                Value::Array(vec![]),
                null_val(),
                null_val(),
                Value::Boolean(false),
                Value::Boolean(false),
                text_val(&constraintdef),
            ]));
            constraint_oid += 1;

            // Keep primary key index discoverable via conindid in case ORMs query it.
            let _ = conindid;
        }

        for (idx_pos, idx) in schema.indexes.iter().enumerate() {
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
            let _conindid = conrelid + 1 + idx_pos as i64;

            rows.push(Row::new(vec![
                int_val(constraint_oid),
                text_val(&idx.name),
                int_val(connamespace_oid),
                text_val("u"),
                int_val(conrelid),
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
                    if let Some(pos) = ref_schema.columns.iter().position(|c| &c.name == col_name) {
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

async fn get_pg_type_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    schema_oids: &HashMap<String, i64>,
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    // Return the vector type so ORMs can discover it
    rows.push(Row::new(vec![
        int_val(16385),     // oid: custom type OID for vector
        text_val("vector"), // typname
        int_val(schema_oid(schema_oids, "pg_catalog")),
        int_val(10),   // typowner: system user
        int_val(-1),   // typlen: variable length
        text_val("f"), // typbyval: false (not passed by value)
        text_val("b"), // typtype: base type
        text_val("A"), // typcategory: Array type
        text_val("f"), // typispreferred: false
        text_val("t"), // typisdefined: true
        text_val(","), // typdelim: comma delimiter
        int_val(0),    // typrelid: not a composite type
        int_val(0),    // typelem: not an array
        int_val(0),    // typarray: no array type
    ]));

    rows.push(Row::new(vec![
        int_val(1082), // oid: built-in date
        text_val("date"),
        int_val(schema_oid(schema_oids, "pg_catalog")),
        int_val(10),
        int_val(4),
        text_val("t"),
        text_val("b"),
        text_val("D"),
        text_val("t"),
        text_val("t"),
        text_val(","),
        int_val(0),
        int_val(0),
        int_val(0),
    ]));

    let mut user_types = store.list_types(txn).await?;
    user_types.sort_by_key(|t| t.oid);
    for def in user_types {
        let (typlen, typbyval, typtype, typcategory) = match def.kind {
            crate::types::UserTypeKind::Enum { .. } => (4, "t", "e", "E"),
            crate::types::UserTypeKind::Composite { .. } => (-1, "f", "c", "C"),
        };

        rows.push(Row::new(vec![
            int_val(def.oid as i64),
            text_val(&def.name),
            int_val(schema_oid(schema_oids, &def.schema)),
            int_val(10),
            int_val(typlen),
            text_val(typbyval),
            text_val(typtype),
            text_val(typcategory),
            text_val("f"),
            text_val("t"),
            text_val(","),
            int_val(0),
            int_val(0),
            int_val(0),
        ]));
    }

    Ok(rows)
}

async fn get_pg_enum_rows(store: &Arc<TikvStore>, txn: &mut Transaction) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    let mut user_types = store.list_types(txn).await?;
    user_types.sort_by_key(|t| t.oid);

    for def in user_types {
        let crate::types::UserTypeKind::Enum { labels } = def.kind else {
            continue;
        };

        for (i, label) in labels.iter().enumerate() {
            let enum_oid = (def.oid as i64)
                .checked_mul(1_000_000)
                .and_then(|v| v.checked_add(i as i64 + 1))
                .ok_or_else(|| anyhow!("pg_enum oid overflow"))?;

            rows.push(Row::new(vec![
                int_val(enum_oid),
                int_val(def.oid as i64),
                float_val((i + 1) as f64),
                text_val(label),
            ]));
        }
    }

    Ok(rows)
}

fn get_pg_proc_rows() -> Vec<Row> {
    // Return empty for now - ORMs mostly just check if the table exists
    vec![]
}

fn get_pg_description_rows() -> Vec<Row> {
    // Return empty for now - ORMs mostly just check if the table exists
    vec![]
}
