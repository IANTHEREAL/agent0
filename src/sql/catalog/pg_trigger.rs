use super::helpers::{bool_col, int_col, int_val, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

pub struct PgTrigger;

fn normalize_relation_name(schema: &str, relation: &str) -> String {
    if crate::sql::names::parse_full_name(relation).is_ok() {
        relation.to_string()
    } else {
        format!("{}.{}", schema, relation)
    }
}

fn resolve_fk_target_table(
    source_schema: &str,
    ref_table: &str,
    table_schemas: &HashMap<String, TableSchema>,
) -> Option<String> {
    let qualified = normalize_relation_name(source_schema, ref_table);
    if table_schemas.contains_key(&qualified) {
        return Some(qualified);
    }
    if table_schemas.contains_key(ref_table) {
        return Some(ref_table.to_string());
    }
    None
}

fn desired_internal_trigger_oid(table_id: u64, fk_idx: usize, slot: u8) -> u32 {
    const BASE: u64 = 3_000_000_000;
    let table_bucket = table_id % 1_000_000;
    let fk_bucket = (fk_idx as u64) % 256;
    (BASE + table_bucket * 1024 + fk_bucket * 4 + u64::from(slot)) as u32
}

fn reserve_trigger_oid(used_oids: &mut HashSet<i64>, desired_oid: u32) -> i64 {
    let mut candidate = desired_oid;
    loop {
        let oid = catalog_oids::pg_trigger_oid(candidate);
        if used_oids.insert(oid) {
            return oid;
        }
        candidate = candidate.wrapping_add(1);
    }
}

#[async_trait]
impl VirtualTable for PgTrigger {
    fn name(&self) -> &str {
        "pg_trigger"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_trigger".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("tgname"),
                int_col("tgrelid"),
                int_col("tgfoid"),
                text_col("tgenabled"),
                bool_col("tgisinternal"),
                int_col("tgparentid"),
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
        let mut table_schemas: HashMap<String, TableSchema> = HashMap::new();
        let mut table_meta: HashMap<String, (i64, u64)> = HashMap::new();
        for table_name in ctx.user_tables {
            if let Some(schema) = ctx.store.get_schema(ctx.txn, ctx.db_id, table_name).await? {
                let full_name = table_name.to_string();
                let table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
                table_meta.insert(full_name.clone(), (table_oid, schema.table_id));
                table_schemas.insert(full_name, schema);
            }
        }

        let mut func_oids: HashMap<String, i64> = HashMap::new();
        for f in ctx.store.list_functions(ctx.txn, ctx.db_id).await? {
            func_oids.insert(
                format!("{}.{}", f.schema, f.name),
                catalog_oids::pg_proc_function_oid(f.oid),
            );
        }

        let mut rows: Vec<(i64, Row)> = Vec::new();
        let mut used_oids = HashSet::new();

        let mut triggers = ctx.store.list_triggers(ctx.txn, ctx.db_id).await?;
        triggers.sort_by_key(|t| t.oid);
        for t in triggers {
            let table_name = normalize_relation_name(&t.schema, &t.table);
            let tgrelid = table_meta
                .get(&table_name)
                .map(|(oid, _)| *oid)
                .unwrap_or(0);
            let tgfoid = func_oids.get(&t.function).copied().unwrap_or(0);
            let trigger_oid = catalog_oids::pg_trigger_oid(t.oid);
            used_oids.insert(trigger_oid);

            rows.push((
                trigger_oid,
                Row::new(vec![
                    int_val(trigger_oid),
                    text_val(&t.name),
                    int_val(tgrelid),
                    int_val(tgfoid),
                    text_val("O"),
                    Value::Boolean(false),
                    int_val(0),
                ]),
            ));
        }

        for source_table in ctx.user_tables {
            let Some(source_schema) = table_schemas.get(source_table.as_str()) else {
                continue;
            };
            let Some(&(source_relid, source_table_id)) = table_meta.get(source_table.as_str())
            else {
                continue;
            };
            let (source_schema_name, _) = split_schema_and_name(source_table);

            for (fk_idx, fk) in source_schema.foreign_keys.iter().enumerate() {
                for (slot, suffix) in [(0u8, "ins"), (1u8, "upd")] {
                    let trigger_oid = reserve_trigger_oid(
                        &mut used_oids,
                        desired_internal_trigger_oid(source_table_id, fk_idx, slot),
                    );
                    let tgname = format!("RI_ConstraintTrigger_c_{}_{}", fk.name, suffix);
                    rows.push((
                        trigger_oid,
                        Row::new(vec![
                            int_val(trigger_oid),
                            text_val(&tgname),
                            int_val(source_relid),
                            int_val(0),
                            text_val("O"),
                            Value::Boolean(true),
                            int_val(0),
                        ]),
                    ));
                }

                let Some(target_table) =
                    resolve_fk_target_table(&source_schema_name, &fk.ref_table, &table_schemas)
                else {
                    continue;
                };
                let Some(&(target_relid, target_table_id)) = table_meta.get(target_table.as_str())
                else {
                    continue;
                };

                for (slot, suffix) in [(2u8, "del"), (3u8, "upd")] {
                    let trigger_oid = reserve_trigger_oid(
                        &mut used_oids,
                        desired_internal_trigger_oid(target_table_id, fk_idx, slot),
                    );
                    let tgname = format!("RI_ConstraintTrigger_a_{}_{}", fk.name, suffix);
                    rows.push((
                        trigger_oid,
                        Row::new(vec![
                            int_val(trigger_oid),
                            text_val(&tgname),
                            int_val(target_relid),
                            int_val(0),
                            text_val("O"),
                            Value::Boolean(true),
                            int_val(0),
                        ]),
                    ));
                }
            }
        }

        rows.sort_by_key(|(oid, _)| *oid);
        Ok(rows.into_iter().map(|(_, row)| row).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_includes_psql_describe_columns() {
        let schema = PgTrigger.schema();
        let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"tgisinternal"));
        assert!(names.contains(&"tgparentid"));
    }

    #[test]
    fn internal_trigger_oid_is_stable() {
        let oid1 = desired_internal_trigger_oid(42, 1, 3);
        let oid2 = desired_internal_trigger_oid(42, 1, 3);
        assert_eq!(oid1, oid2);
    }
}
