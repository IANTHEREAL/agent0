//! Single-source schema definitions for _DB9_SYS_* virtual tables.
//!
//! Both the executor (data materialization) and the protocol handler
//! (type inference) use these definitions, eliminating duplicate schemas.

use crate::model::{ColumnDef, DataType, TableSchema};

fn col(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable: false,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

fn col_nullable(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

/// Return the schema for a _DB9_SYS_* virtual table, if `name` matches.
///
/// `name` is case-insensitive and used as-is for the returned schema's name field.
pub fn virtual_table_schema(name: &str) -> Option<TableSchema> {
    let columns = match name.to_ascii_uppercase().as_str() {
        "_DB9_SYS_OBSERVABILITY" => vec![
            col("window_seconds", DataType::Int64),
            col("statement_count", DataType::Int64),
            col("txn_commit_count", DataType::Int64),
            col("error_count", DataType::Int64),
            col("rate_limited_count", DataType::Int64),
            col("retry_attempts", DataType::Int64),
            col("retry_budget_exhausted", DataType::Int64),
            col("retry_timeout_aborts", DataType::Int64),
            col("retry_conflict_unknown", DataType::Int64),
            col("retry_conflict_optimistic", DataType::Int64),
            col("retry_conflict_pessimistic", DataType::Int64),
            col("retry_conflict_self_rolled_back", DataType::Int64),
            col("retry_conflict_rc_check_ts", DataType::Int64),
            col("retry_conflict_lazy_uniqueness", DataType::Int64),
            col("hnsw_graph_bytes_written", DataType::Int64),
            col("hnsw_serialize_duration_us", DataType::Int64),
            col("qps", DataType::Float64),
            col("tps", DataType::Float64),
            col("latency_avg_ms", DataType::Float64),
            col("latency_p99_ms", DataType::Float64),
            col("active_connections", DataType::Int64),
        ],
        "_DB9_SYS_QUERY_SAMPLES" => vec![
            col("query", DataType::Text),
            col("sample_count", DataType::Int64),
            col("error_count", DataType::Int64),
            col("latency_avg_ms", DataType::Float64),
            col("latency_p99_ms", DataType::Float64),
            col("latency_max_ms", DataType::Float64),
            col("last_seen_ms_ago", DataType::Int64),
        ],
        "_DB9_SYS_EXPORT_DDL" => vec![
            col("ddl_order", DataType::Int64),
            col("object_type", DataType::Text),
            col("object_name", DataType::Text),
            col("ddl_sql", DataType::Text),
        ],
        "_DB9_SYS_MIGRATIONS" => vec![
            col("name", DataType::Text),
            col("applied_at", DataType::Text),
            col("checksum", DataType::Text),
            col("sql_preview", DataType::Text),
        ],
        "_DB9_SYS_RECORD_MIGRATION" => vec![
            col("name", DataType::Text),
            col("applied_at", DataType::Text),
            col("status", DataType::Text),
        ],
        "_DB9_SYS_TRIGGER_QUEUE_STATS" => vec![
            col("keyspace", DataType::Text),
            col("pending", DataType::Int64),
            col("processing", DataType::Int64),
            col("failed", DataType::Int64),
            col("dlq_count", DataType::Int64),
            col("avg_latency_ms", DataType::Float64),
            col("events_per_min", DataType::Int64),
        ],
        "_DB9_SYS_TRIGGER_DLQ" => vec![
            col("id", DataType::Int64),
            col("trigger_name", DataType::Text),
            col("table_name", DataType::Text),
            col("operation", DataType::Text),
            col_nullable("error_msg", DataType::Text),
            col("retry_count", DataType::Int64),
            col("created_at_ms", DataType::Int64),
        ],
        _ => return None,
    };

    Some(TableSchema {
        table_id: 0,
        name: name.to_string(),
        columns,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_seven_tables_resolve() {
        let names = [
            "_DB9_SYS_OBSERVABILITY",
            "_DB9_SYS_QUERY_SAMPLES",
            "_DB9_SYS_EXPORT_DDL",
            "_DB9_SYS_MIGRATIONS",
            "_DB9_SYS_RECORD_MIGRATION",
            "_DB9_SYS_TRIGGER_QUEUE_STATS",
            "_DB9_SYS_TRIGGER_DLQ",
        ];
        for name in names {
            assert!(
                virtual_table_schema(name).is_some(),
                "{} should resolve",
                name
            );
        }
    }

    #[test]
    fn case_insensitive_lookup() {
        assert!(virtual_table_schema("_db9_sys_observability").is_some());
        assert!(virtual_table_schema("_DB9_SYS_OBSERVABILITY").is_some());
    }

    #[test]
    fn unknown_returns_none() {
        assert!(virtual_table_schema("no_such_table").is_none());
    }

    #[test]
    fn trigger_dlq_error_msg_is_nullable() {
        let schema = virtual_table_schema("_DB9_SYS_TRIGGER_DLQ").unwrap();
        let error_msg = schema
            .columns
            .iter()
            .find(|c| c.name == "error_msg")
            .unwrap();
        assert!(error_msg.nullable);
    }

    #[test]
    fn schema_uses_provided_name() {
        let schema = virtual_table_schema("my_alias").is_none();
        assert!(schema);
        let schema = virtual_table_schema("_db9_sys_export_ddl").unwrap();
        assert_eq!(schema.name, "_db9_sys_export_ddl");
    }
}
