use super::helpers::{
    access_method_oid, bool_col, int_col, int_val, null_val, owner_role_oid, schema_oid,
    split_schema_and_name, text_array_col, text_col, text_val,
};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

pub struct PgClass;

fn normalize_fk_ref_table(source_schema: &str, ref_table: &str) -> String {
    if crate::sql::names::parse_full_name(ref_table).is_ok() {
        ref_table.to_string()
    } else {
        format!("{}.{}", source_schema, ref_table)
    }
}

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
            let normalized_ref = normalize_fk_ref_table(&source_schema, &fk.ref_table);
            if table_schemas.contains_key(&normalized_ref) {
                tables.insert(normalized_ref);
            } else if table_schemas.contains_key(&fk.ref_table) {
                // Legacy fallback: tolerate old schemas that persisted ref_table without schema.
                tables.insert(fk.ref_table.clone());
            }
        }
    }
    tables
}

#[async_trait]
impl VirtualTable for PgClass {
    fn name(&self) -> &str {
        "pg_class"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_class".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("relname"),
                int_col("relnamespace"),
                int_col("reltype"),
                int_col("reloftype"),
                text_col("relkind"),
                int_col("relowner"),
                int_col("relam"),
                int_col("reltuples"),
                int_col("relpages"),
                int_col("reltoastrelid"),
                bool_col("relhasindex"),
                bool_col("relispopulated"),
                text_col("relpersistence"),
                int_col("relnatts"),
                int_col("relchecks"),
                bool_col("relhasrules"),
                bool_col("relhastriggers"),
                bool_col("relhassubclass"),
                bool_col("relrowsecurity"),
                bool_col("relforcerowsecurity"),
                text_col("relreplident"),
                bool_col("relispartition"),
                text_col("relpartbound"),
                int_col("reltablespace"),
                text_array_col("reloptions"),
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
        let mut table_schemas: HashMap<String, TableSchema> = HashMap::new();

        for full_table_name in ctx.user_tables {
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                table_schemas.insert(full_table_name.to_string(), schema);
            }
        }
        let tables_with_fk_internal_triggers = tables_with_fk_internal_triggers(&table_schemas);
        let mut tables_with_user_triggers = HashSet::new();
        for trigger in ctx.store.list_triggers(ctx.txn, ctx.db_id).await? {
            tables_with_user_triggers
                .insert(normalize_relation_name(&trigger.schema, &trigger.table));
        }

        for full_table_name in ctx.user_tables {
            let (table_schema, table_name) = split_schema_and_name(full_table_name);
            let namespace_oid = schema_oid(ctx.schema_oids, &table_schema);
            if let Some(schema) = table_schemas.get(full_table_name.as_str()) {
                let table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
                let relhasindex = !schema.indexes.is_empty() || !schema.pk_indices.is_empty();
                let relhastriggers = tables_with_user_triggers.contains(full_table_name)
                    || tables_with_fk_internal_triggers.contains(full_table_name);
                let relnatts = schema.columns.len() as i64;
                let relchecks = schema.check_constraints.len() as i64;
                let relowner = owner_role_oid(Some(&schema.owner), ctx.current_user);
                rows.push(Row::new(vec![
                    int_val(table_oid),
                    text_val(&table_name),
                    int_val(namespace_oid),
                    int_val(0), // reltype
                    int_val(0), // reloftype
                    text_val("r"),
                    int_val(relowner),
                    int_val(0), // relam
                    int_val(0), // reltuples
                    int_val(0), // relpages
                    int_val(0), // reltoastrelid (no TOAST)
                    Value::Boolean(relhasindex),
                    Value::Boolean(true), // relispopulated
                    text_val("p"),        // relpersistence: permanent
                    Value::Int64(relnatts),
                    Value::Int64(relchecks),
                    Value::Boolean(false), // relhasrules
                    Value::Boolean(relhastriggers),
                    Value::Boolean(false), // relhassubclass
                    Value::Boolean(false), // relrowsecurity
                    Value::Boolean(false), // relforcerowsecurity
                    text_val("d"),         // relreplident
                    Value::Boolean(false), // relispartition
                    null_val(),            // relpartbound
                    int_val(0),            // reltablespace
                    null_val(),            // reloptions
                ]));

                for idx in &schema.indexes {
                    let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
                    rows.push(Row::new(vec![
                        int_val(index_oid),
                        text_val(&idx.name),
                        int_val(namespace_oid),
                        int_val(0), // reltype
                        int_val(0), // reloftype
                        text_val("i"),
                        int_val(relowner),
                        int_val(access_method_oid(idx.method.as_deref())),
                        int_val(0),            // reltuples
                        int_val(0),            // relpages
                        int_val(0),            // reltoastrelid
                        Value::Boolean(false), // relhasindex
                        Value::Boolean(true),  // relispopulated
                        text_val("p"),         // relpersistence
                        int_val(0),            // relnatts
                        int_val(0),            // relchecks
                        Value::Boolean(false), // relhasrules
                        Value::Boolean(false), // relhastriggers
                        Value::Boolean(false), // relhassubclass
                        Value::Boolean(false), // relrowsecurity
                        Value::Boolean(false), // relforcerowsecurity
                        text_val("d"),         // relreplident
                        Value::Boolean(false), // relispartition
                        null_val(),            // relpartbound
                        int_val(0),            // reltablespace
                        null_val(),            // reloptions
                    ]));
                }

                if !schema.pk_indices.is_empty() {
                    let pk_name = schema
                        .pk_constraint_name
                        .clone()
                        .unwrap_or_else(|| format!("{}_pkey", table_name));
                    let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
                    rows.push(Row::new(vec![
                        int_val(pk_oid),
                        text_val(&pk_name),
                        int_val(namespace_oid),
                        int_val(0), // reltype
                        int_val(0), // reloftype
                        text_val("i"),
                        int_val(relowner),
                        int_val(403),          // relam (btree)
                        int_val(0),            // reltuples
                        int_val(0),            // relpages
                        int_val(0),            // reltoastrelid
                        Value::Boolean(false), // relhasindex
                        Value::Boolean(true),  // relispopulated
                        text_val("p"),         // relpersistence
                        int_val(0),            // relnatts
                        int_val(0),            // relchecks
                        Value::Boolean(false), // relhasrules
                        Value::Boolean(false), // relhastriggers
                        Value::Boolean(false), // relhassubclass
                        Value::Boolean(false), // relrowsecurity
                        Value::Boolean(false), // relforcerowsecurity
                        text_val("d"),         // relreplident
                        Value::Boolean(false), // relispartition
                        null_val(),            // relpartbound
                        int_val(0),            // reltablespace
                        null_val(),            // reloptions
                    ]));
                }
            }
        }

        let sequences = ctx.store.list_sequences(ctx.txn, ctx.db_id).await?;
        for seq in sequences {
            let seq_oid = catalog_oids::pg_class_sequence_oid(seq.oid);
            let namespace_oid = schema_oid(ctx.schema_oids, &seq.schema);
            let relowner = owner_role_oid(Some(&seq.owner), ctx.current_user);
            rows.push(Row::new(vec![
                int_val(seq_oid),
                text_val(&seq.name),
                int_val(namespace_oid),
                int_val(0), // reltype
                int_val(0), // reloftype
                text_val("S"),
                int_val(relowner),
                int_val(0),            // relam
                int_val(0),            // reltuples
                int_val(0),            // relpages
                int_val(0),            // reltoastrelid
                Value::Boolean(false), // relhasindex
                Value::Boolean(true),  // relispopulated
                text_val("p"),         // relpersistence
                int_val(0),            // relnatts
                int_val(0),            // relchecks
                Value::Boolean(false), // relhasrules
                Value::Boolean(false), // relhastriggers
                Value::Boolean(false), // relhassubclass
                Value::Boolean(false), // relrowsecurity
                Value::Boolean(false), // relforcerowsecurity
                text_val("d"),         // relreplident
                Value::Boolean(false), // relispartition
                null_val(),            // relpartbound
                int_val(0),            // reltablespace
                null_val(),            // reloptions
            ]));
        }

        let views = ctx
            .store
            .list_views(ctx.txn, ctx.db_id)
            .await
            .unwrap_or_default();
        for view_def in views {
            let namespace_oid = schema_oid(ctx.schema_oids, &view_def.schema);
            let view_oid = catalog_oids::pg_class_view_oid(view_def.oid);
            let relowner = owner_role_oid(Some(&view_def.owner), ctx.current_user);
            rows.push(Row::new(vec![
                int_val(view_oid),
                text_val(&view_def.name),
                int_val(namespace_oid),
                int_val(0), // reltype
                int_val(0), // reloftype
                text_val("v"),
                int_val(relowner),
                int_val(0),            // relam
                int_val(0),            // reltuples
                int_val(0),            // relpages
                int_val(0),            // reltoastrelid
                Value::Boolean(false), // relhasindex
                Value::Boolean(true),  // relispopulated
                text_val("p"),         // relpersistence
                int_val(0),            // relnatts
                int_val(0),            // relchecks
                Value::Boolean(false), // relhasrules
                Value::Boolean(false), // relhastriggers
                Value::Boolean(false), // relhassubclass
                Value::Boolean(false), // relrowsecurity
                Value::Boolean(false), // relforcerowsecurity
                text_val("d"),         // relreplident
                Value::Boolean(false), // relispartition
                null_val(),            // relpartbound
                int_val(0),            // reltablespace
                null_val(),            // reloptions
            ]));
        }

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::tables_with_fk_internal_triggers;
    use crate::model::{ForeignKeyAction, ForeignKeyConstraint, TableSchema};
    use std::collections::HashMap;

    fn empty_schema(name: &str) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn fk_trigger_tables_include_source_and_target_for_qualified_and_unqualified_refs() {
        let mut schemas = HashMap::new();
        schemas.insert("public.parent".to_string(), empty_schema("public.parent"));
        schemas.insert(
            "public.child".to_string(),
            TableSchema {
                name: "public.child".to_string(),
                foreign_keys: vec![ForeignKeyConstraint {
                    name: "child_parent_fkey".to_string(),
                    columns: vec!["parent_id".to_string()],
                    ref_table: "public.parent".to_string(),
                    ref_columns: vec!["id".to_string()],
                    on_delete: ForeignKeyAction::NoAction,
                    on_update: ForeignKeyAction::NoAction,
                }],
                ..Default::default()
            },
        );
        schemas.insert("tenant.p".to_string(), empty_schema("tenant.p"));
        schemas.insert(
            "tenant.c".to_string(),
            TableSchema {
                name: "tenant.c".to_string(),
                foreign_keys: vec![ForeignKeyConstraint {
                    name: "tenant_c_p_fkey".to_string(),
                    columns: vec!["p_id".to_string()],
                    ref_table: "p".to_string(),
                    ref_columns: vec!["id".to_string()],
                    on_delete: ForeignKeyAction::NoAction,
                    on_update: ForeignKeyAction::NoAction,
                }],
                ..Default::default()
            },
        );

        let tables = tables_with_fk_internal_triggers(&schemas);
        assert!(tables.contains("public.parent"));
        assert!(tables.contains("tenant.p"));
        assert!(tables.contains("public.child"));
        assert!(tables.contains("tenant.c"));
    }
}
