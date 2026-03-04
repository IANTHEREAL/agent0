//! DDL operation execution (CREATE TABLE AS, CREATE/DROP INDEX, ALTER TABLE)

use super::super::ddl;
use super::super::names;
use super::super::ExecuteResult;
use super::Executor;
use crate::sql::error::SqlError;
use crate::sql::sequences::SequenceSession;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterTableOperation, ColumnDef as SqlColumnDef, Expr, Ident, ObjectName, OrderByExpr, Query,
};
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
            let table_schema = table.split('.').next().unwrap_or("");
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
            let table_schema = table.split('.').next().unwrap_or("");
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

/// T10: Detect `SELECT * FROM read_parquet('url')` and return SelectStream directly,
/// bypassing full CBO materialization. Only matches the exact simple pattern.
#[cfg(feature = "parquet")]
async fn try_streaming_ctas_for_read_parquet(
    store: &std::sync::Arc<crate::storage::TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    query: &Query,
) -> Result<Option<ExecuteResult>> {
    use sqlparser::ast::{
        FunctionArg, FunctionArgExpr, SelectItem, SetExpr, TableFactor, Value as AstValue,
    };

    // Match: SELECT * FROM read_parquet('url') with no WHERE/GROUP/HAVING/ORDER/LIMIT
    let select = match query.body.as_ref() {
        SetExpr::Select(s) => s,
        _ => return Ok(None),
    };
    if !query.order_by.is_empty()
        || query.limit.is_some()
        || query.offset.is_some()
        || select.selection.is_some()
        || select.having.is_some()
        || select.distinct.is_some()
    {
        return Ok(None);
    }
    // Check group_by is empty
    match &select.group_by {
        sqlparser::ast::GroupByExpr::Expressions(exprs) => {
            if !exprs.is_empty() {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    }
    // Must be SELECT * (single wildcard projection)
    if select.projection.len() != 1 {
        return Ok(None);
    }
    if !matches!(&select.projection[0], SelectItem::Wildcard(_)) {
        return Ok(None);
    }
    // Must have exactly one FROM table, which is a table function named read_parquet
    if select.from.len() != 1 || !select.from[0].joins.is_empty() {
        return Ok(None);
    }
    let (func_name, func_args) = match &select.from[0].relation {
        TableFactor::Table {
            name,
            args: Some(tfa),
            ..
        } => (name.to_string(), tfa.as_slice()),
        _ => return Ok(None),
    };
    if !func_name.eq_ignore_ascii_case("read_parquet") {
        return Ok(None);
    }

    // Extract URL from first argument
    let url = match func_args.first() {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(sqlparser::ast::Expr::Value(
            AstValue::SingleQuotedString(s),
        )))) => s.clone(),
        _ => return Ok(None),
    };

    // Verify parquet extension is installed
    let installed = store.get_extension(txn, db_id, "parquet").await?;
    match installed {
        Some(ext) if ext.enabled => {}
        _ => {
            return Err(anyhow!(
                "extension parquet is not installed. Run: CREATE EXTENSION parquet"
            ));
        }
    }

    // Acquire per-tenant concurrency permit -- held across stream consumption
    let tenant =
        crate::extensions::context::tenant_keyspace().unwrap_or_else(|| "default".to_string());
    let _permit = crate::extensions::parquet::limits::acquire_import_permit(&tenant)?;

    tracing::info!("CTAS streaming path activated for read_parquet('{}')", url);

    // Open streaming row reader
    let (schema, row_stream) = crate::extensions::parquet::reader::open_row_stream(&url)
        .await
        .map_err(|e| anyhow!("Failed to open Parquet file for CTAS: {}", e))?;

    let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    let column_types: Vec<crate::model::DataType> =
        schema.columns.iter().map(|c| c.data_type.clone()).collect();

    use futures::StreamExt;
    let mapped_stream = row_stream.map(move |values_result| {
        let _ = &_permit; // keep permit alive across stream consumption
        values_result.map(crate::model::Row::new)
    });

    let boxed: futures::stream::BoxStream<'static, anyhow::Result<crate::model::Row>> =
        Box::pin(mapped_stream);

    Ok(Some(ExecuteResult::SelectStream {
        columns,
        column_types,
        stream: crate::sql::result::RowStream(boxed),
        timezone: std::sync::Arc::from("UTC"),
    }))
}

impl Executor {
    pub(crate) async fn execute_create_table_as(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
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

        // T10: Detect simple `SELECT * FROM read_parquet('url')` and use streaming CTAS
        #[cfg(feature = "parquet")]
        if let Some(stream_result) =
            try_streaming_ctas_for_read_parquet(&self.store(), txn, db_id, query).await?
        {
            return match stream_result {
                ExecuteResult::SelectStream {
                    columns: cols,
                    column_types,
                    stream,
                    ..
                } => {
                    ddl::create_table_from_stream(
                        &self.store(),
                        txn,
                        db_id,
                        &table_name,
                        if_not_exists,
                        cols,
                        column_types,
                        stream,
                    )
                    .await
                }
                _ => unreachable!(),
            };
        }

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

        match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => {
                ddl::create_table_from_query_result(
                    &self.store(),
                    txn,
                    db_id,
                    &table_name,
                    if_not_exists,
                    cols,
                    rows,
                    columns,
                )
                .await
            }
            ExecuteResult::SelectStream {
                columns: cols,
                column_types,
                stream,
                ..
            } => {
                ddl::create_table_from_stream(
                    &self.store(),
                    txn,
                    db_id,
                    &table_name,
                    if_not_exists,
                    cols,
                    column_types,
                    stream,
                )
                .await
            }
            _ => Err(anyhow!("CREATE TABLE AS requires a SELECT query")),
        }
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

        match result {
            ExecuteResult::Select {
                columns: cols,
                rows,
                ..
            } => {
                ddl::create_table_from_select_into(
                    &self.store(),
                    txn,
                    db_id,
                    &table_name,
                    cols,
                    rows,
                )
                .await
            }
            ExecuteResult::SelectStream {
                columns: cols,
                column_types,
                stream,
                ..
            } => {
                ddl::create_table_from_stream(
                    &self.store(),
                    txn,
                    db_id,
                    &table_name,
                    false,
                    cols,
                    column_types,
                    stream,
                )
                .await
            }
            _ => Err(anyhow!("SELECT INTO requires a SELECT query")),
        }
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
        concurrently: bool,
        predicate: Option<&Expr>,
        with_params: Option<&str>,
        username: &str,
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
        // CONCURRENTLY delegates backfill to the BgDdl worker — skip the
        // pre-scan so we reach the worker gate without unnecessary I/O.
        let needs_backfill = !concurrently
            && using
                .map(|u| {
                    u.value.eq_ignore_ascii_case("btree") || u.value.eq_ignore_ascii_case("gin")
                })
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
            concurrently,
            predicate,
            with_params,
            rows,
            self.tenant_keyspace(),
            username,
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
                let table_schema = table_name.split('.').next().unwrap_or("");
                if !schema_filter.contains(&table_schema) {
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

            // Within-schema Ambiguous case should be unreachable with
            // schema-wide relation-name reservation; keep defensive handling.
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
                return Err(anyhow!("index \"{}\" does not exist", idx_name));
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
        let (result, invalidate_table_id) =
            ddl::execute_alter_table(&self.store(), txn, db_id, search_path, name, operation)
                .await?;

        // Invalidate cached + persisted statistics when the DDL structurally
        // changed the table (column added/dropped/renamed/retyped).
        if let Some(table_id) = invalidate_table_id {
            self.stats_cache().invalidate(db_id, table_id);
            self.store().delete_statistics(txn, db_id, table_id).await?;
        }

        Ok(result)
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
