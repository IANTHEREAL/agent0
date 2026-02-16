//! DDL operation execution (CREATE TABLE AS, CREATE/DROP INDEX, ALTER TABLE)

use super::super::ddl;
use super::super::names;
use super::super::ExecuteResult;
use super::Executor;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterTableOperation, ColumnDef as SqlColumnDef, Expr, Ident, ObjectName, OrderByExpr, Query,
};
use std::collections::HashMap;
use tikv_client::Transaction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropIndexResolutionError {
    Ambiguous,
}

fn pick_drop_index_target(
    explicit_schema: Option<&str>,
    search_path: &[String],
    matching_tables: &[String],
) -> std::result::Result<Option<String>, DropIndexResolutionError> {
    if let Some(schema) = explicit_schema {
        let mut found: Option<&String> = None;
        for table in matching_tables {
            let table_schema = table.splitn(2, '.').next().unwrap_or("");
            if table_schema != schema {
                continue;
            }
            if found.is_some() {
                return Err(DropIndexResolutionError::Ambiguous);
            }
            found = Some(table);
        }
        return Ok(found.cloned());
    }

    let schemas: Vec<&str> = if search_path.is_empty() {
        vec!["public"]
    } else {
        search_path.iter().map(|s| s.as_str()).collect()
    };

    for schema in schemas {
        let mut found: Option<&String> = None;
        for table in matching_tables {
            let table_schema = table.splitn(2, '.').next().unwrap_or("");
            if table_schema != schema {
                continue;
            }
            if found.is_some() {
                return Err(DropIndexResolutionError::Ambiguous);
            }
            found = Some(table);
        }
        if let Some(table) = found {
            return Ok(Some(table.clone()));
        }
    }

    Ok(None)
}

impl Executor {
    pub(crate) async fn execute_create_table_as(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        name: &ObjectName,
        query: &Query,
        columns: &[SqlColumnDef],
        if_not_exists: bool,
        _temporary: bool,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self
            .store()
            .schema_exists(txn, db_id, &resolved.schema)
            .await?
        {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let table_name = resolved.full;

        let ctes = self
            .build_cte_context(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                current_role,
            )
            .await?;
        let result = self
            .execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                &ctes,
                current_role,
            )
            .await?;

        let (result_cols, result_rows) = match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => (cols, rows),
            _ => return Err(anyhow!("CREATE TABLE AS requires a SELECT query")),
        };

        ddl::create_table_from_query_result(
            &self.store(),
            txn,
            db_id,
            &table_name,
            if_not_exists,
            result_cols,
            result_rows,
            columns,
        )
        .await
    }

    pub(crate) async fn create_table_from_result(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        target_name: &ObjectName,
        result: ExecuteResult,
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_ddl_object_name(target_name, search_path)?;
        if !self
            .store()
            .schema_exists(txn, db_id, &resolved.schema)
            .await?
        {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let table_name = resolved.full;

        let (result_cols, result_rows) = match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => (cols, rows),
            _ => return Err(anyhow!("SELECT INTO requires a SELECT query")),
        };

        ddl::create_table_from_select_into(
            &self.store(),
            txn,
            db_id,
            &table_name,
            result_cols,
            result_rows,
        )
        .await
    }

    pub(crate) async fn execute_create_index(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        idx_name: &ObjectName,
        table_name: &ObjectName,
        using: Option<&Ident>,
        columns: &[OrderByExpr],
        unique: bool,
        if_not_exists: bool,
        predicate: Option<&Expr>,
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_existing_table_name(
            self.store().as_ref(),
            txn,
            db_id,
            table_name,
            search_path,
        )
        .await?
        .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
        let tbl_name = resolved.full;
        let (idx_schema_opt, idx_name_str) = names::split_object_name(idx_name)?;
        if let Some(idx_schema) = idx_schema_opt {
            if idx_schema != resolved.schema {
                return Err(anyhow!(
                    "index schema '{}' does not match table schema '{}'",
                    idx_schema,
                    resolved.schema
                ));
            }
        }
        let schema = self
            .store()
            .get_schema(txn, db_id, &tbl_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(tbl_name.clone()))?;
        let needs_backfill = using
            .map(|u| u.value.eq_ignore_ascii_case("btree") || u.value.eq_ignore_ascii_case("gin"))
            .unwrap_or(true);
        let rows = if needs_backfill {
            self.scan_and_fill(txn, db_id, &tbl_name, &schema).await?
        } else {
            Vec::new()
        };
        ddl::execute_create_index(
            &self.store(),
            txn,
            db_id,
            &idx_name_str,
            &tbl_name,
            using,
            columns,
            unique,
            if_not_exists,
            predicate,
            rows,
        )
        .await
    }

    pub(crate) async fn execute_drop_index(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        index_names: &[ObjectName],
        if_exists: bool,
    ) -> Result<ExecuteResult> {
        let mut last_index = String::new();
        let tables = self.store().list_tables(txn, db_id).await?;
        for name in index_names {
            let (schema_opt, idx_name) = names::split_object_name(name)?;
            let explicit_schema = schema_opt.as_deref();
            let schema_filter: Vec<&str> = match explicit_schema {
                Some(schema) => vec![schema],
                None => {
                    if search_path.is_empty() {
                        vec!["public"]
                    } else {
                        search_path.iter().map(|s| s.as_str()).collect()
                    }
                }
            };

            let mut matching_tables = Vec::new();
            for table_name in &tables {
                let table_schema = table_name.splitn(2, '.').next().unwrap_or("");
                if !schema_filter.iter().any(|s| *s == table_schema) {
                    continue;
                }
                let schema = match self.store().get_schema(txn, db_id, table_name).await? {
                    Some(s) => s,
                    None => continue,
                };
                if schema.indexes.iter().any(|i| i.name == idx_name) {
                    matching_tables.push(table_name.clone());
                }
            }

            let table_name =
                match pick_drop_index_target(explicit_schema, search_path, &matching_tables) {
                    Ok(table_name) => table_name,
                    Err(DropIndexResolutionError::Ambiguous) => {
                        return Err(anyhow!("Index '{}' is ambiguous", idx_name));
                    }
                };

            let Some(table_name) = table_name else {
                if if_exists {
                    continue;
                }
                return Err(SqlError::RelationNotFound(idx_name.to_string()).into());
            };

            let mut schema = self
                .store()
                .get_schema(txn, db_id, &table_name)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(table_name.clone()))?;
            let rows = if schema.pk_indices.is_empty() {
                Vec::new()
            } else {
                self.scan_and_fill(txn, db_id, &table_name, &schema).await?
            };

            let dropped = ddl::execute_drop_index(
                &self.store(),
                txn,
                db_id,
                &idx_name,
                &mut schema,
                &table_name,
                rows,
            )
            .await?;
            let Some(dropped) = dropped else {
                return Err(SqlError::RelationNotFound(idx_name.to_string()).into());
            };
            last_index = dropped;
        }
        Ok(ExecuteResult::DropIndex {
            index_name: last_index,
        })
    }

    pub(crate) async fn execute_alter_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        name: &ObjectName,
        operation: &AlterTableOperation,
    ) -> Result<ExecuteResult> {
        ddl::execute_alter_table(&self.store(), txn, db_id, search_path, name, operation).await
    }
}

#[cfg(test)]
mod tests {
    use super::{pick_drop_index_target, DropIndexResolutionError};

    #[test]
    fn pick_drop_index_target_prefers_explicit_schema() {
        let target = pick_drop_index_target(
            Some("public"),
            &["other".to_string(), "public".to_string()],
            &["public.t".to_string(), "other.t".to_string()],
        )
        .unwrap();
        assert_eq!(target.as_deref(), Some("public.t"));
    }

    #[test]
    fn pick_drop_index_target_explicit_schema_ambiguity_errors() {
        let err = pick_drop_index_target(
            Some("public"),
            &["public".to_string()],
            &["public.t1".to_string(), "public.t2".to_string()],
        )
        .unwrap_err();
        assert_eq!(err, DropIndexResolutionError::Ambiguous);
    }

    #[test]
    fn pick_drop_index_target_uses_search_path_order() {
        let target = pick_drop_index_target(
            None,
            &["public".to_string(), "other".to_string()],
            &["other.t".to_string()],
        )
        .unwrap();
        assert_eq!(target.as_deref(), Some("other.t"));

        let target = pick_drop_index_target(
            None,
            &["public".to_string(), "other".to_string()],
            &["other.t".to_string(), "public.t".to_string()],
        )
        .unwrap();
        assert_eq!(target.as_deref(), Some("public.t"));
    }

    #[test]
    fn pick_drop_index_target_search_path_ambiguity_errors() {
        let err = pick_drop_index_target(
            None,
            &["public".to_string(), "other".to_string()],
            &[
                "public.t1".to_string(),
                "public.t2".to_string(),
                "other.t".to_string(),
            ],
        )
        .unwrap_err();
        assert_eq!(err, DropIndexResolutionError::Ambiguous);
    }

    #[test]
    fn pick_drop_index_target_defaults_to_public() {
        let target = pick_drop_index_target(None, &[], &["other.t".to_string()]).unwrap();
        assert!(target.is_none());
    }
}
