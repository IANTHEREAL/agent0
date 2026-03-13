use super::helpers::{bool_col, null_val, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

pub struct PgTables;

fn normalize_relation_name(schema: &str, relation: &str) -> String {
    if crate::sql::names::parse_full_name(relation).is_ok() {
        relation.to_string()
    } else {
        format!("{}.{}", schema, relation)
    }
}

fn tables_with_fk_internal_triggers(
    table_schemas: &HashMap<String, TableSchema>,
) -> HashSet<String> {
    let mut tables = HashSet::new();
    for (source_full_name, schema) in table_schemas {
        let (source_schema, _) = split_schema_and_name(source_full_name);
        for fk in &schema.foreign_keys {
            tables.insert(source_full_name.clone());
            let qualified = normalize_relation_name(&source_schema, &fk.ref_table);
            if table_schemas.contains_key(&qualified) {
                tables.insert(qualified);
            } else if table_schemas.contains_key(&fk.ref_table) {
                tables.insert(fk.ref_table.clone());
            }
        }
    }
    tables
}

#[async_trait]
impl VirtualTable for PgTables {
    fn name(&self) -> &str {
        "pg_tables"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_tables".to_string(),
            columns: vec![
                text_col("schemaname"),
                text_col("tablename"),
                text_col("tableowner"),
                text_col("tablespace"),
                bool_col("hasindexes"),
                bool_col("hasrules"),
                bool_col("hastriggers"),
                bool_col("rowsecurity"),
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
        let mut table_schemas = HashMap::new();
        for full_table_name in ctx.user_tables {
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                table_schemas.insert(full_table_name.to_string(), schema);
            }
        }

        let mut tables_with_user_triggers = HashSet::new();
        for trigger in ctx.store.list_triggers(ctx.txn, ctx.db_id).await? {
            tables_with_user_triggers
                .insert(normalize_relation_name(&trigger.schema, &trigger.table));
        }
        let tables_with_fk_internal_triggers = tables_with_fk_internal_triggers(&table_schemas);

        let mut rows = Vec::new();
        for full_table_name in ctx.user_tables {
            let (table_schema, table_name) = split_schema_and_name(full_table_name);
            let Some(schema) = table_schemas.get(full_table_name.as_str()) else {
                continue;
            };
            let hasindexes = !schema.pk_indices.is_empty() || !schema.indexes.is_empty();
            let hastriggers = tables_with_user_triggers.contains(full_table_name)
                || tables_with_fk_internal_triggers.contains(full_table_name);

            rows.push(Row::new(vec![
                text_val(&table_schema),
                text_val(&table_name),
                text_val(&schema.owner),
                null_val(),
                Value::Boolean(hasindexes),
                Value::Boolean(false), // hasrules
                Value::Boolean(hastriggers),
                Value::Boolean(schema.rls_enabled), // rowsecurity
            ]));
        }

        Ok(rows)
    }
}
