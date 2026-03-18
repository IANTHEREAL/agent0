use super::helpers::{
    data_type_to_pg_type, data_type_to_udt_name, int_col, int_val, null_val, split_schema_and_name,
    text_col, text_val,
};
use super::{ScanContext, VirtualTable};
use crate::model::{DataType, Row, TableSchema};
use crate::sql::sequences;
use anyhow::Result;
use async_trait::async_trait;

pub struct Columns;

#[async_trait]
impl VirtualTable for Columns {
    fn name(&self) -> &str {
        "columns"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
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
        let sequence_defs = ctx.store.list_sequences(ctx.txn, ctx.db_id).await?;
        let schemas = ctx
            .store
            .list_table_schemas(ctx.txn, ctx.db_id, ctx.user_tables)
            .await?;

        for schema in &schemas {
            let full_table_name = &schema.name;
            let (table_schema, table_name) = split_schema_and_name(full_table_name);
            for (i, col) in schema.columns.iter().enumerate() {
                // PostgreSQL reports enum/UDT columns as data_type='USER-DEFINED'
                // with udt_schema/udt_name pointing to the actual type.
                // For enum[] (Array(UserDefined(...))), PostgreSQL reports
                // data_type='ARRAY' and udt_name='_<type_name>' (underscore-
                // prefixed element type name).
                let (data_type_str, udt_schema_owned, udt_name_owned);
                match &col.data_type {
                    DataType::UserDefined(full_udt) => {
                        let (schema_name, type_name) = full_udt
                            .rsplit_once('.')
                            .unwrap_or(("public", full_udt.as_str()));
                        data_type_str = "USER-DEFINED";
                        udt_schema_owned = schema_name.to_string();
                        udt_name_owned = type_name.to_string();
                    }
                    DataType::Array(inner)
                        if matches!(inner.as_ref(), DataType::UserDefined(_)) =>
                    {
                        if let DataType::UserDefined(full_udt) = inner.as_ref() {
                            let (schema_name, type_name) = full_udt
                                .rsplit_once('.')
                                .unwrap_or(("public", full_udt.as_str()));
                            data_type_str = "ARRAY";
                            udt_schema_owned = schema_name.to_string();
                            udt_name_owned = format!("_{}", type_name);
                        } else {
                            unreachable!()
                        }
                    }
                    _ => {
                        let pg_type = data_type_to_pg_type(&col.data_type);
                        let udt = data_type_to_udt_name(&col.data_type);
                        data_type_str = pg_type;
                        udt_schema_owned = "pg_catalog".to_string();
                        udt_name_owned = udt.to_string();
                    }
                }
                let is_nullable = if col.nullable { "YES" } else { "NO" };
                let ordinal = (i + 1) as i64;

                let (char_max_len, num_precision, num_scale) = match &col.data_type {
                    DataType::Int32 => (null_val(), int_val(32), int_val(0)),
                    DataType::Int64 => (null_val(), int_val(64), int_val(0)),
                    DataType::Float64 => (null_val(), int_val(53), null_val()),
                    DataType::Text => (null_val(), null_val(), null_val()),
                    DataType::Varchar(0) => (null_val(), null_val(), null_val()),
                    DataType::Varchar(n) => (int_val(*n as i64), null_val(), null_val()),
                    DataType::Numeric { precision, scale } => {
                        let p = precision.map(|v| int_val(v as i64)).unwrap_or(null_val());
                        let s = scale.map(|v| int_val(v as i64)).unwrap_or(null_val());
                        (null_val(), p, s)
                    }
                    _ => (null_val(), null_val(), null_val()),
                };

                let column_default = match sequences::resolve_serial_display_default(
                    col,
                    &sequence_defs,
                    full_table_name,
                    &table_schema,
                    &table_name,
                )? {
                    Some(s) => text_val(&s),
                    None => null_val(),
                };

                rows.push(Row::new(vec![
                    text_val(ctx.database_name),
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
                    text_val(ctx.database_name),
                    text_val(&udt_schema_owned),
                    text_val(&udt_name_owned),
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
                    if col.generation_expr.is_some() {
                        text_val("ALWAYS")
                    } else {
                        text_val("NEVER")
                    },
                    col.generation_expr
                        .as_ref()
                        .map(|s| text_val(&format!("({})", s)))
                        .unwrap_or(null_val()),
                    text_val("YES"),
                ]));
            }
        }

        Ok(rows)
    }
}
