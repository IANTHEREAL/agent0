use super::*;
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{
    AnalyzedProjection, BinaryOp, IsTestKind, TypedExpr, TypedExprKind, UnaryOp,
};
use crate::sql::error::SqlError;
use crate::sql::optimizer::physical_plan::{Db9CopOp, Db9CopScan};
use crate::sql::ScanType;
use crate::{
    pool::{try_grow_statement_memory_scope, try_shrink_statement_memory_scope},
    sql::memory::estimate_row_size,
};
use anyhow::{anyhow, Context, Result};
use prost::Message;
use std::sync::LazyLock;
use tikv_client::proto::db9_coprocessor::{
    self as wire, db9_expr, db9_value, Db9BinaryOp, Db9ScanKind, Db9TypeKind, Db9UnaryOp,
};
use tikv_client::BoundRange;

// Exact DB9 Cop wire/runtime surface sent by this server. Keep this in lockstep
// with cloud-storage-engine's `DB9_CODEC_VERSION` so mixed runtime pairs fail
// fast before expression execution.
const DB9_COP_CODEC_VERSION: u32 = 3;
// Must stay aligned with the engine-side REQ_TYPE_DB9_DAG contract.
const DB9_COP_REQUEST_TYPE_DAG: i64 = 10_001;
const DB9_JSONB_BINARY_MAGIC: &[u8] = b"\0db9jb1";
const DB9_JSONB_TEXT_MAGIC: &[u8] = b"\0db9jb2";
const DB9_COP_BUFFER_COMPONENT: &str = "storage.db9_cop.buffered_rows";
static DB9_COP_KV_ERROR_MESSAGE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"message: (?:"((?:[^"\\]|\\.)*)"|\\\"((?:[^"\\]|\\.)*)\\\")"#)
        .expect("DB9 cop KV error message regex must compile")
});
static DB9_COP_STRING_ERROR_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"StringError\((?:"((?:[^"\\]|\\.)*)"|\\\"((?:[^"\\]|\\.)*)\\\")\)"#)
        .expect("DB9 cop StringError regex must compile")
});

fn push_unique_db9_cop_message(messages: &mut Vec<String>, message: &str) {
    if !messages.iter().any(|existing| existing == message) {
        messages.push(message.to_string());
    }
}

fn unescape_db9_cop_debug_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn db9_cop_is_datetime_field_overflow_message(message: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "date field value out of range",
        "time field value out of range",
        "timestamp field value out of range",
        "timestamp out of range",
        "timestamp cannot be NaN",
        "interval out of range",
        "interval is out of range",
        "TO_TIMESTAMP epoch must be finite",
        "TO_TIMESTAMP timestamp out of range",
        "TO_TIMESTAMP timestamp cannot be NaN",
        "invalid DB9 timestamp value",
        "MAKE_DATE date field value out of range",
        "MAKE_TIME time field value out of range",
        "MAKE_TIME second field value out of range",
        "MAKE_TIMESTAMP date field value out of range",
        "MAKE_TIMESTAMP time field value out of range",
        "MAKE_TIMESTAMP timestamp field value out of range",
        "MAKE_TIMESTAMP second field value out of range",
    ];

    let detail = message
        .strip_prefix("DB9 function '")
        .and_then(|rest| rest.split_once("' ").map(|(_, detail)| detail))
        .unwrap_or(message);

    PREFIXES.iter().any(|prefix| detail.starts_with(prefix))
}

fn db9_cop_sql_error_from_message(message: &str) -> Option<SqlError> {
    if message.starts_with("unsupported DB9 codec_version ") {
        return Some(SqlError::Unsupported(format!(
            "{message}\nHINT: disable \"db9.enable_cop_pushdown\" or upgrade all cloud-storage-engine nodes to the paired DB9 Cop runtime surface before enabling pushdown."
        )));
    }

    let unsupported_global_regex_option = message.starts_with("regexp_")
        && message.ends_with("() does not support the \"global\" option");
    let unknown_digest_algorithm =
        message.starts_with("Cannot use ") && message.ends_with(": No such hash algorithm");
    let invalid_base64_message = message == "invalid base64 end sequence"
        || message == "unexpected \"=\" while decoding base64 sequence"
        || (message.starts_with("invalid symbol ")
            && message.ends_with(" found while decoding base64 sequence"));
    let invalid_encoding_message = message.starts_with("unrecognized encoding: ")
        || message.starts_with("invalid hexadecimal digit: ")
        || message.starts_with("invalid hexadecimal data: ")
        || message == "invalid escape sequence"
        || invalid_base64_message;
    let invalid_escape_string = message == "invalid escape string"
        || message == "LIKE pattern must not end with escape character";
    let row_decode_size_limit = message.starts_with("failed to decode DB9 row: ")
        && message.contains("stored row payload")
        && message.contains("exceeds decode limit");
    let response_size_limit = message.starts_with("DB9 response size ")
        && message.contains(" exceeds coprocessor max_resp_size ");
    let invalid_width_bucket_parameter = message
        .strip_prefix("DB9 function 'width_bucket' ")
        .is_some_and(|detail| {
            matches!(
                detail,
                "operand, lower bound, and upper bound cannot be NaN"
                    | "lower and upper bounds must be finite"
                    | "count must be greater than zero"
                    | "lower bound cannot equal upper bound"
            )
        });

    if message.starts_with("invalid regular expression: ") {
        return Some(SqlError::InvalidRegularExpression {
            message: message.to_string(),
        });
    }

    if message == "invalid input syntax for type bytea" {
        return Some(SqlError::InvalidInputSyntax {
            type_name: "bytea".to_string(),
            value: String::new(),
        });
    }

    if message.starts_with("invalid regular expression option: ")
        || unsupported_global_regex_option
        || unknown_digest_algorithm
        || invalid_encoding_message
        || message == "field position must not be zero"
        || message == "character number must be positive"
    {
        return Some(SqlError::InvalidParameterValue {
            message: message.to_string(),
        });
    }

    if invalid_width_bucket_parameter {
        return Some(SqlError::InvalidArgumentForWidthBucket {
            message: message.to_string(),
        });
    }

    if invalid_escape_string {
        return Some(SqlError::InvalidEscapeString {
            message: message.to_string(),
        });
    }

    if db9_cop_is_datetime_field_overflow_message(message) {
        return Some(SqlError::DatetimeFieldOverflow {
            message: message.to_string(),
        });
    }

    if let Some(value) = message
        .strip_prefix("invalid DB9 date literal '")
        .and_then(|value| value.strip_suffix('\''))
    {
        return Some(SqlError::InvalidInputSyntax {
            type_name: "date".to_string(),
            value: value.to_string(),
        });
    }

    if message == "cannot take square root of a negative number"
        || message == "a negative number raised to a non-integer power yields a complex result"
        || message == "zero raised to a negative power is undefined"
    {
        return Some(SqlError::InvalidArgumentForPowerFunction {
            message: message.to_string(),
        });
    }

    if message == "negative substring length not allowed" {
        return Some(SqlError::SubstringError {
            message: message.to_string(),
        });
    }

    if message == "searching for elements in multidimensional arrays is not supported"
        || message == "removing elements from multidimensional arrays is not supported"
    {
        return Some(SqlError::Unsupported(message.to_string()));
    }

    if message == "argument must be empty or one-dimensional array" {
        return Some(SqlError::ArrayDimensionError {
            message: message.to_string(),
        });
    }

    if message == "cannot concatenate incompatible arrays" {
        return Some(SqlError::ArraySubscriptError {
            message: message.to_string(),
        });
    }

    if message == "cannot take logarithm of zero"
        || message == "cannot take logarithm of a negative number"
    {
        return Some(SqlError::InvalidArgumentForLogarithm {
            message: message.to_string(),
        });
    }

    if message == "division by zero" {
        return Some(SqlError::DivisionByZero);
    }

    if message == "input is out of range"
        || message == "value out of range: underflow"
        || message == "value overflows numeric format"
        || message == "integer out of range"
        || message == "bigint out of range"
        || message == "DB9 numeric value is out of range"
    {
        return Some(SqlError::NumericValueOutOfRange {
            message: message.to_string(),
        });
    }

    if message == "null character not permitted"
        || message == "requested length too large"
        || message.starts_with("requested character too large for encoding: ")
        || message.starts_with("requested character not valid for encoding: ")
        || row_decode_size_limit
        || response_size_limit
    {
        return Some(SqlError::ValueTooLarge {
            message: message.to_string(),
        });
    }

    None
}

fn collect_db9_coprocessor_semantic_messages_from_text(text: &str, messages: &mut Vec<String>) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }

    if db9_cop_sql_error_from_message(trimmed).is_some() {
        push_unique_db9_cop_message(messages, trimmed);
        return;
    }

    if let Some(inner) = trimmed.strip_prefix("Kv error. ") {
        collect_db9_coprocessor_semantic_messages_from_text(inner, messages);
    }

    for capture in DB9_COP_KV_ERROR_MESSAGE_RE.captures_iter(trimmed) {
        if let Some(inner) = capture.get(1).or_else(|| capture.get(2)) {
            let inner = unescape_db9_cop_debug_string(inner.as_str());
            collect_db9_coprocessor_semantic_messages_from_text(&inner, messages);
        }
    }

    for capture in DB9_COP_STRING_ERROR_RE.captures_iter(trimmed) {
        if let Some(inner) = capture.get(1).or_else(|| capture.get(2)) {
            let inner = unescape_db9_cop_debug_string(inner.as_str());
            collect_db9_coprocessor_semantic_messages_from_text(&inner, messages);
        }
    }
}

fn extract_db9_coprocessor_semantic_errors(err: &tikv_client::Error) -> Vec<String> {
    fn collect_from_error(err: &tikv_client::Error, messages: &mut Vec<String>) {
        match err {
            tikv_client::Error::KvError { message }
            | tikv_client::Error::StringError(message)
            | tikv_client::Error::InternalError { message } => {
                collect_db9_coprocessor_semantic_messages_from_text(message, messages);
                if matches!(err, tikv_client::Error::KvError { .. }) && messages.is_empty() {
                    push_unique_db9_cop_message(messages, message);
                }
            }
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => {
                for nested in errors {
                    collect_from_error(nested, messages);
                }
            }
            _ => {}
        }
    }

    let mut messages = Vec::new();
    collect_from_error(err, &mut messages);
    messages
}

#[cfg(test)]
fn extract_db9_coprocessor_semantic_error(err: &tikv_client::Error) -> Option<String> {
    let messages = extract_db9_coprocessor_semantic_errors(err);
    if messages.is_empty() {
        None
    } else {
        Some(messages.join("; "))
    }
}

fn map_db9_coprocessor_rpc_error(err: tikv_client::Error, request_summary: &str) -> anyhow::Error {
    let semantic_messages = extract_db9_coprocessor_semantic_errors(&err);
    if let Some(sql_err) = semantic_messages
        .iter()
        .find_map(|message| db9_cop_sql_error_from_message(message))
    {
        sql_err.into()
    } else if !semantic_messages.is_empty() {
        anyhow!(semantic_messages.join("; "))
    } else {
        anyhow::Error::new(err).context(format!("DB9 coprocessor RPC failed for {request_summary}"))
    }
}

impl TikvStore {
    /// Execute a DB9 coprocessor request and return buffered rows plus the bytes
    /// charged against the current statement memory scope. The caller owns that
    /// charge and must release it when the buffer is dropped.
    pub async fn cop_select(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_schema: &TableSchema,
        scan: &Db9CopScan,
        ops: &[Db9CopOp],
        output_schema: &TableSchema,
    ) -> Result<(Vec<Row>, usize)> {
        let request = build_db9_dag_request(db_id, table_schema, scan, ops).with_context(|| {
            format!(
                "failed to build {}",
                summarize_db9_request(db_id, table_schema, scan, ops, 0)
            )
        })?;
        let ranges = build_db9_request_ranges(db_id, table_schema, scan).with_context(|| {
            format!(
                "failed to build request ranges for {}",
                summarize_db9_request(db_id, table_schema, scan, ops, 0)
            )
        })?;
        let request_summary = summarize_db9_request(db_id, table_schema, scan, ops, ranges.len());
        let mut responses = txn
            .coprocessor(
                DB9_COP_REQUEST_TYPE_DAG,
                request.encode_to_vec(),
                ranges.clone(),
            )
            .await
            .map_err(|err| map_db9_coprocessor_rpc_error(err, &request_summary))?;
        order_db9_cop_responses_for_scan(&mut responses, scan);
        decode_db9_select_chunks(responses, output_schema, &request_summary)
    }
}

fn decode_db9_select_chunks<I, R>(
    responses: I,
    output_schema: &TableSchema,
    request_summary: &str,
) -> Result<(Vec<Row>, usize)>
where
    I: IntoIterator<Item = (R, Vec<u8>)>,
{
    let mut rows = Vec::new();
    let mut charged_bytes = 0usize;

    for (chunk_index, (_meta, data)) in responses.into_iter().enumerate() {
        let raw_chunk_bytes = data.len();
        if let Err(err) =
            grow_db9_cop_buffer_charge(&mut charged_bytes, raw_chunk_bytes, request_summary)
        {
            release_db9_cop_buffer_charge(charged_bytes);
            return Err(err);
        }

        let response = match decode_db9_select_response(&data).with_context(|| {
            format!(
                "failed to decode DB9 cop response chunk {} for {}",
                chunk_index, request_summary
            )
        }) {
            Ok(response) => response,
            Err(err) => {
                release_db9_cop_buffer_charge(charged_bytes);
                return Err(err);
            }
        };

        let mut retained_chunk_bytes = 0usize;
        for (row_index, row) in response.rows.into_iter().enumerate() {
            let decoded_row = match decode_db9_row(row, output_schema).with_context(|| {
                format!(
                    "failed to decode DB9 cop row {} from chunk {} for {}",
                    row_index, chunk_index, request_summary
                )
            }) {
                Ok(decoded_row) => decoded_row,
                Err(err) => {
                    release_db9_cop_buffer_charge(charged_bytes);
                    return Err(err);
                }
            };

            let row_bytes = estimate_row_size(&decoded_row);
            let new_retained_chunk_bytes =
                retained_chunk_bytes.checked_add(row_bytes).ok_or_else(|| {
                    release_db9_cop_buffer_charge(charged_bytes);
                    anyhow!("DB9 cop buffered byte count overflowed for {request_summary}")
                })?;

            let covered_bytes = raw_chunk_bytes.max(retained_chunk_bytes);
            let required_bytes = raw_chunk_bytes.max(new_retained_chunk_bytes);
            if required_bytes > covered_bytes {
                let extra_charge = required_bytes - covered_bytes;
                if let Err(err) =
                    grow_db9_cop_buffer_charge(&mut charged_bytes, extra_charge, request_summary)
                {
                    release_db9_cop_buffer_charge(charged_bytes);
                    return Err(err);
                }
            }

            retained_chunk_bytes = new_retained_chunk_bytes;
            rows.push(decoded_row);
        }

        if raw_chunk_bytes > retained_chunk_bytes {
            shrink_db9_cop_buffer_charge(
                &mut charged_bytes,
                raw_chunk_bytes - retained_chunk_bytes,
            );
        }
    }

    Ok((rows, charged_bytes))
}

fn release_db9_cop_buffer_charge(charged_bytes: usize) {
    try_shrink_statement_memory_scope(charged_bytes);
}

fn grow_db9_cop_buffer_charge(
    charged_bytes: &mut usize,
    bytes: usize,
    request_summary: &str,
) -> Result<()> {
    if bytes == 0 {
        return Ok(());
    }

    let new_charged_bytes = charged_bytes
        .checked_add(bytes)
        .ok_or_else(|| anyhow!("DB9 cop buffered byte count overflowed for {request_summary}"))?;
    try_grow_statement_memory_scope(DB9_COP_BUFFER_COMPONENT, bytes)
        .map_err(anyhow::Error::from)?;
    *charged_bytes = new_charged_bytes;
    Ok(())
}

fn shrink_db9_cop_buffer_charge(charged_bytes: &mut usize, bytes: usize) {
    if bytes == 0 {
        return;
    }

    debug_assert!(*charged_bytes >= bytes);
    try_shrink_statement_memory_scope(bytes);
    *charged_bytes -= bytes;
}

fn summarize_db9_request(
    db_id: u64,
    table_schema: &TableSchema,
    scan: &Db9CopScan,
    ops: &[Db9CopOp],
    ranges_len: usize,
) -> String {
    format!(
        "DB9 cop request db_id={} table={}#{} scan={} ops=[{}] ranges={}",
        db_id,
        table_schema.name,
        table_schema.table_id,
        summarize_db9_scan(scan),
        summarize_db9_ops(ops),
        ranges_len,
    )
}

fn summarize_db9_scan(scan: &Db9CopScan) -> String {
    match scan {
        Db9CopScan::Seq => "seq".to_string(),
        Db9CopScan::Index {
            scan_type,
            desc,
            require_row_fetch,
        } => {
            let summary = summarize_db9_scan_type(scan_type);
            let mut parts = vec![summary];
            if *desc {
                parts.push("desc".to_string());
            }
            if !*require_row_fetch {
                parts.push("index_only".to_string());
            }
            parts.join(",")
        }
    }
}

fn db9_cop_scan_desc(scan: &Db9CopScan) -> bool {
    matches!(scan, Db9CopScan::Index { desc: true, .. })
}

fn order_db9_cop_responses_for_scan<T>(responses: &mut [T], scan: &Db9CopScan) {
    if db9_cop_scan_desc(scan) {
        // Match TiDB's KeepOrder DESC path: reverse task response order,
        // while CSE handles range order and backward scanning inside a task.
        responses.reverse();
    }
}

fn summarize_db9_scan_type(scan_type: &ScanType) -> String {
    match scan_type {
        ScanType::FullTableScan => "full_table".to_string(),
        ScanType::PrimaryKeyScan {
            index_name, values, ..
        } => {
            format!("primary_key(name={}, values={})", index_name, values.len())
        }
        ScanType::PrimaryKeyRangeScan {
            index_name,
            prefix_values,
            ..
        } => format!(
            "primary_key_prefix(name={}, prefix_values={})",
            index_name,
            prefix_values.len()
        ),
        ScanType::IndexScan {
            index_name, values, ..
        } => {
            format!("index_exact(name={}, values={})", index_name, values.len())
        }
        ScanType::IndexRangeScan {
            index_name,
            prefix_values,
            ..
        } => format!(
            "index_prefix(name={}, prefix_values={})",
            index_name,
            prefix_values.len()
        ),
        ScanType::IndexBoundedRangeScan {
            index_name,
            prefix_values,
            range_start,
            range_end,
            ..
        } => format!(
            "index_bounded(name={}, prefix_values={}, start={}, end={})",
            index_name,
            prefix_values.len(),
            range_start.is_some(),
            range_end.is_some()
        ),
        ScanType::InListScan {
            index_name,
            column_values,
            ..
        } => format!(
            "in_list(name={}, tuples={})",
            index_name,
            column_values.len()
        ),
        ScanType::GinIndexScan { index_name, .. } => format!("gin(name={})", index_name),
        ScanType::HnswIndexScan { index_name, k, .. } => {
            format!("hnsw(name={}, k={})", index_name, k)
        }
    }
}

fn summarize_db9_ops(ops: &[Db9CopOp]) -> String {
    if ops.is_empty() {
        return "none".to_string();
    }

    ops.iter()
        .map(|op| match op {
            Db9CopOp::Filter { .. } => "filter".to_string(),
            Db9CopOp::Project { projections } => format!("project({})", projections.len()),
            Db9CopOp::Limit { limit } => format!("limit({})", limit),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn build_db9_dag_request(
    db_id: u64,
    table_schema: &TableSchema,
    scan: &Db9CopScan,
    ops: &[Db9CopOp],
) -> Result<wire::Db9DagRequest> {
    let (selection, projections, limit) = encode_db9_ops(ops)?;
    Ok(wire::Db9DagRequest {
        codec_version: DB9_COP_CODEC_VERSION,
        db_id,
        table: Some(build_table_info(table_schema)?),
        scan: Some(build_scan_spec(scan)?),
        selection,
        projections,
        limit,
    })
}

fn build_table_info(table_schema: &TableSchema) -> Result<wire::Db9TableInfo> {
    Ok(wire::Db9TableInfo {
        table_name: table_schema.name.clone(),
        table_id: table_schema.table_id,
        schema_version: table_schema.version,
        columns: table_schema
            .columns
            .iter()
            .map(build_column_info)
            .collect::<Result<Vec<_>>>()?,
        pk_indices: table_schema
            .pk_indices
            .iter()
            .map(|idx| {
                u32::try_from(*idx)
                    .map_err(|_| anyhow!("PK column index {idx} exceeds DB9 wire format"))
            })
            .collect::<Result<Vec<_>>>()?,
        indexes: table_schema
            .indexes
            .iter()
            .map(|index| wire::Db9IndexInfo {
                index_id: index.id,
                index_name: index.name.clone(),
                columns: index.columns.clone(),
                unique: index.unique,
            })
            .collect(),
    })
}

fn build_column_info(column: &ColumnDef) -> Result<wire::Db9ColumnInfo> {
    Ok(wire::Db9ColumnInfo {
        name: column.name.clone(),
        data_type: Some(encode_db9_type(&column.data_type)?),
        nullable: column.nullable,
    })
}

fn build_scan_spec(scan: &Db9CopScan) -> Result<wire::Db9ScanSpec> {
    match scan {
        Db9CopScan::Seq => Ok(wire::Db9ScanSpec {
            kind: Db9ScanKind::Table as i32,
            index_id: 0,
            index_name: String::new(),
            desc: false,
            require_row_fetch: false,
        }),
        Db9CopScan::Index {
            scan_type,
            desc,
            require_row_fetch,
        } => {
            let (kind, index_id, index_name) = match scan_type {
                ScanType::PrimaryKeyScan { .. } | ScanType::PrimaryKeyRangeScan { .. } => {
                    return Err(anyhow!("DB9 cop runtime does not support primary-key scan"));
                }
                ScanType::IndexScan {
                    index_id,
                    index_name,
                    ..
                } => (Db9ScanKind::Index, *index_id, index_name.clone()),
                ScanType::IndexRangeScan {
                    index_id,
                    index_name,
                    ..
                } => (Db9ScanKind::IndexRange, *index_id, index_name.clone()),
                ScanType::IndexBoundedRangeScan {
                    index_id,
                    index_name,
                    ..
                } => (
                    Db9ScanKind::IndexBoundedRange,
                    *index_id,
                    index_name.clone(),
                ),
                ScanType::InListScan {
                    index_id,
                    index_name,
                    ..
                } => (Db9ScanKind::InList, *index_id, index_name.clone()),
                other => {
                    return Err(anyhow!(
                        "DB9 cop runtime does not support scan type {:?}",
                        other
                    ));
                }
            };

            Ok(wire::Db9ScanSpec {
                kind: kind as i32,
                index_id,
                index_name,
                desc: *desc,
                require_row_fetch: *require_row_fetch,
            })
        }
    }
}

fn build_db9_request_ranges(
    db_id: u64,
    table_schema: &TableSchema,
    scan: &Db9CopScan,
) -> Result<Vec<BoundRange>> {
    match scan {
        Db9CopScan::Index {
            scan_type: ScanType::PrimaryKeyRangeScan { .. },
            ..
        } => Err(anyhow!(
            "PrimaryKeyRangeScan is not supported by DB9 coprocessor"
        )),
        Db9CopScan::Seq => {
            let (start, end) = encode_table_data_range_v2(db_id, table_schema.table_id);
            Ok(vec![(start..end).into()])
        }
        Db9CopScan::Index { scan_type, .. } => match scan_type {
            ScanType::PrimaryKeyScan { .. } => {
                Err(anyhow!("DB9 cop runtime does not support primary-key scan"))
            }
            ScanType::IndexScan {
                index_id, values, ..
            } => Ok(vec![index_exact_range(
                db_id,
                table_schema.table_id,
                *index_id,
                values,
            )]),
            ScanType::IndexRangeScan {
                index_id,
                prefix_values,
                ..
            } => Ok(vec![index_prefix_range(
                db_id,
                table_schema.table_id,
                *index_id,
                prefix_values,
            )]),
            ScanType::IndexBoundedRangeScan {
                index_id,
                prefix_values,
                range_start,
                start_inclusive,
                range_end,
                end_inclusive,
                ..
            } => {
                let start = encode_index_range_start_v2(
                    db_id,
                    table_schema.table_id,
                    *index_id,
                    prefix_values,
                    range_start.as_ref(),
                    *start_inclusive,
                );
                let end = encode_index_range_end_v2(
                    db_id,
                    table_schema.table_id,
                    *index_id,
                    prefix_values,
                    range_end.as_ref(),
                    *end_inclusive,
                );
                Ok(vec![(start..end).into()])
            }
            ScanType::InListScan {
                index_id,
                column_values,
                ..
            } => column_values
                .iter()
                .map(|values| {
                    Ok(index_exact_range(
                        db_id,
                        table_schema.table_id,
                        *index_id,
                        values,
                    ))
                })
                .collect(),
            other => Err(anyhow!(
                "DB9 cop runtime does not support scan type {:?}",
                other
            )),
        },
    }
}

fn index_exact_range(db_id: u64, table_id: u64, index_id: u64, values: &[Value]) -> BoundRange {
    index_prefix_range(db_id, table_id, index_id, values)
}

fn index_prefix_range(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    prefix_values: &[Value],
) -> BoundRange {
    let start = encode_index_range_start_v2(db_id, table_id, index_id, prefix_values, None, true);
    let end = encode_index_range_end_v2(db_id, table_id, index_id, prefix_values, None, true);
    (start..end).into()
}

fn encode_db9_ops(
    ops: &[Db9CopOp],
) -> Result<(
    Option<wire::Db9Expr>,
    Vec<wire::Db9NamedExpr>,
    Option<wire::Db9LimitSpec>,
)> {
    let mut selection = None;
    let mut projections = Vec::new();
    let mut limit = None;

    for op in ops {
        match op {
            Db9CopOp::Filter { predicate } => {
                if selection.is_some() {
                    return Err(anyhow!("DB9 cop runtime expects at most one pushed filter"));
                }
                selection = Some(encode_db9_expr(predicate)?);
            }
            Db9CopOp::Project {
                projections: pushed_projections,
            } => {
                if !projections.is_empty() {
                    return Err(anyhow!(
                        "DB9 cop runtime expects at most one pushed projection"
                    ));
                }
                projections = pushed_projections
                    .iter()
                    .map(encode_projection)
                    .collect::<Result<Vec<_>>>()?;
            }
            Db9CopOp::Limit {
                limit: pushed_limit,
            } => {
                if limit.is_some() {
                    return Err(anyhow!("DB9 cop runtime expects at most one pushed limit"));
                }
                limit = Some(wire::Db9LimitSpec {
                    limit: u64::try_from(*pushed_limit)
                        .map_err(|_| anyhow!("DB9 cop limit {pushed_limit} exceeds u64"))?,
                });
            }
        }
    }

    Ok((selection, projections, limit))
}

fn encode_projection(projection: &AnalyzedProjection) -> Result<wire::Db9NamedExpr> {
    Ok(wire::Db9NamedExpr {
        output_name: projection.output_name.clone(),
        expr: Some(encode_db9_expr(&projection.expr)?),
    })
}

fn encode_db9_expr_type(expr: &TypedExpr) -> Result<wire::Db9Type> {
    match (&expr.kind, &expr.data_type) {
        (TypedExprKind::Constant(Value::Text(_)), DataType::Unknown) => {
            encode_db9_type(&DataType::Text)
        }
        _ => encode_db9_type(&expr.data_type),
    }
}

fn encode_db9_expr(expr: &TypedExpr) -> Result<wire::Db9Expr> {
    let return_type = encode_db9_expr_type(expr)?;
    let encoded_expr = match &expr.kind {
        TypedExprKind::Constant(value) => db9_expr::Expr::Constant(wire::Db9ConstantExpr {
            value: Some(encode_db9_value(value, &expr.data_type)?),
        }),
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            column_name,
        } => {
            if *scope_depth != 0 {
                return Err(anyhow!(
                    "DB9 cop runtime does not support correlated column references"
                ));
            }
            db9_expr::Expr::ColumnRef(wire::Db9ColumnRefExpr {
                column_index: u32::try_from(*column_index).map_err(|_| {
                    anyhow!(
                        "DB9 cop column index {} exceeds wire-format bounds",
                        column_index
                    )
                })?,
                column_name: column_name.clone(),
            })
        }
        TypedExprKind::BinaryOp { left, op, right } => match op {
            BinaryOp::RegexMatch => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_regex_match".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::RegexIMatch => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_regex_imatch".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::RegexNotMatch => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_regex_not_match".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::RegexNotIMatch => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_regex_not_imatch".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::BitwiseAnd => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_bitand".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::BitwiseOr => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_bitor".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::BitwiseXor => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_bitxor".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::ShiftLeft => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_shl".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            BinaryOp::ShiftRight => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_shr".to_owned(),
                args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
            }),
            _ => db9_expr::Expr::Binary(Box::new(wire::Db9BinaryExpr {
                left: Some(Box::new(encode_db9_expr(left)?)),
                op: encode_binary_op(op)? as i32,
                right: Some(Box::new(encode_db9_expr(right)?)),
            })),
        },
        TypedExprKind::UnaryOp { op, operand } => match op {
            UnaryOp::BitwiseNot => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: "__db9_bitnot".to_owned(),
                args: vec![encode_db9_expr(operand)?],
            }),
            _ => db9_expr::Expr::Unary(Box::new(wire::Db9UnaryExpr {
                op: encode_unary_op(op)? as i32,
                operand: Some(Box::new(encode_db9_expr(operand)?)),
            })),
        },
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            ..
        } => db9_expr::Expr::Cast(Box::new(wire::Db9CastExpr {
            expr: Some(Box::new(encode_db9_expr(inner)?)),
            target_type: Some(encode_db9_type(target_type)?),
        })),
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => match test {
            IsTestKind::Null => db9_expr::Expr::IsNull(Box::new(wire::Db9IsNullExpr {
                expr: Some(Box::new(encode_db9_expr(inner)?)),
                negated: *negated,
            })),
            _ => return encode_db9_expr(&rewrite_db9_is_test(inner, *test, *negated)),
        },
        TypedExprKind::IsDistinctFrom {
            left,
            right,
            negated,
        } => {
            return encode_db9_expr(&rewrite_db9_is_distinct_from(left, right, *negated));
        }
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => return encode_db9_expr(&rewrite_db9_between(inner, low, high, *negated)),
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => return encode_db9_expr(&rewrite_db9_in_list(inner, list, *negated)?),
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => {
            if !order_by.is_empty() || filter.is_some() {
                return Err(anyhow!(
                    "DB9 cop runtime does not support function ORDER BY/FILTER modifiers"
                ));
            }
            db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: func.name.to_ascii_lowercase(),
                args: args
                    .iter()
                    .map(encode_db9_expr)
                    .collect::<Result<Vec<_>>>()?,
            })
        }
        TypedExprKind::Coalesce(exprs) => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
            function_name: "coalesce".to_owned(),
            args: exprs
                .iter()
                .map(encode_db9_expr)
                .collect::<Result<Vec<_>>>()?,
        }),
        TypedExprKind::NullIf(left, right) => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
            function_name: "nullif".to_owned(),
            args: vec![encode_db9_expr(left)?, encode_db9_expr(right)?],
        }),
        TypedExprKind::ArrayLiteral(elems) => db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
            function_name: "__db9_make_array".to_owned(),
            args: elems
                .iter()
                .map(encode_db9_expr)
                .collect::<Result<Vec<_>>>()?,
        }),
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => {
            let mut args = vec![encode_db9_expr(inner)?, encode_db9_expr(pattern)?];
            if let Some(escape) = escape {
                args.push(encode_db9_expr(escape)?);
            }
            db9_expr::Expr::FuncCall(wire::Db9FuncCallExpr {
                function_name: encode_like_internal_function(*case_insensitive, *negated)
                    .to_owned(),
                args,
            })
        }
        TypedExprKind::Collate { expr: inner, .. } => return encode_db9_expr(inner),
        other => {
            return Err(anyhow!(
                "DB9 cop runtime does not support expression {:?}",
                other
            ));
        }
    };

    Ok(wire::Db9Expr {
        return_type: Some(return_type),
        expr: Some(encoded_expr),
    })
}

fn bool_constant(value: bool) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::Constant(Value::Boolean(value)),
        DataType::Boolean,
    )
}

fn binary_boolean_expr(left: TypedExpr, op: BinaryOp, right: TypedExpr) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        },
        DataType::Boolean,
    )
}

fn unary_boolean_expr(op: UnaryOp, operand: TypedExpr) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::UnaryOp {
            op,
            operand: Box::new(operand),
        },
        DataType::Boolean,
    )
}

fn is_null_expr(expr: TypedExpr, negated: bool) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::IsTest {
            expr: Box::new(expr),
            test: IsTestKind::Null,
            negated,
        },
        DataType::Boolean,
    )
}

fn coalesce_boolean_expr(exprs: Vec<TypedExpr>) -> TypedExpr {
    TypedExpr::new(TypedExprKind::Coalesce(exprs), DataType::Boolean)
}

fn rewrite_db9_is_test(expr: &TypedExpr, test: IsTestKind, negated: bool) -> TypedExpr {
    match test {
        IsTestKind::Null => is_null_expr(expr.clone(), negated),
        IsTestKind::True => {
            let eq_true = binary_boolean_expr(expr.clone(), BinaryOp::Eq, bool_constant(true));
            let rewritten = coalesce_boolean_expr(vec![eq_true, bool_constant(false)]);
            if negated {
                unary_boolean_expr(UnaryOp::Not, rewritten)
            } else {
                rewritten
            }
        }
        IsTestKind::False => {
            let eq_false = binary_boolean_expr(expr.clone(), BinaryOp::Eq, bool_constant(false));
            let rewritten = coalesce_boolean_expr(vec![eq_false, bool_constant(false)]);
            if negated {
                unary_boolean_expr(UnaryOp::Not, rewritten)
            } else {
                rewritten
            }
        }
        IsTestKind::Unknown => is_null_expr(expr.clone(), negated),
    }
}

fn rewrite_db9_is_distinct_from(left: &TypedExpr, right: &TypedExpr, negated: bool) -> TypedExpr {
    if left.is_null_constant() && right.is_null_constant() {
        return bool_constant(negated);
    }
    if left.is_null_constant() || right.is_null_constant() {
        let operand = if left.is_null_constant() {
            right.clone()
        } else {
            left.clone()
        };
        return is_null_expr(operand, !negated);
    }
    if matches!(left.kind, TypedExprKind::Constant(_))
        || matches!(right.kind, TypedExprKind::Constant(_))
    {
        let (expr, constant) = if matches!(left.kind, TypedExprKind::Constant(_)) {
            (right.clone(), left.clone())
        } else {
            (left.clone(), right.clone())
        };
        let comparison = binary_boolean_expr(
            expr,
            if negated {
                BinaryOp::Eq
            } else {
                BinaryOp::NotEq
            },
            constant,
        );
        return coalesce_boolean_expr(vec![comparison, bool_constant(!negated)]);
    }

    // This rewrite relies on the paired coprocessor preserving PostgreSQL
    // three-valued logic for `<>`: the value comparison must yield NULL when
    // either side is NULL so the COALESCE fallback can distinguish the
    // null-mismatch case with `(left IS NULL) <> (right IS NULL)`.
    let value_distinct = binary_boolean_expr(left.clone(), BinaryOp::NotEq, right.clone());
    let null_distinct = binary_boolean_expr(
        is_null_expr(left.clone(), false),
        BinaryOp::NotEq,
        is_null_expr(right.clone(), false),
    );
    let rewritten = coalesce_boolean_expr(vec![value_distinct, null_distinct]);
    if negated {
        unary_boolean_expr(UnaryOp::Not, rewritten)
    } else {
        rewritten
    }
}

fn rewrite_db9_between(
    expr: &TypedExpr,
    low: &TypedExpr,
    high: &TypedExpr,
    negated: bool,
) -> TypedExpr {
    let lower = binary_boolean_expr(expr.clone(), BinaryOp::GtEq, low.clone());
    let upper = binary_boolean_expr(expr.clone(), BinaryOp::LtEq, high.clone());
    let rewritten = binary_boolean_expr(lower, BinaryOp::And, upper);
    if negated {
        unary_boolean_expr(UnaryOp::Not, rewritten)
    } else {
        rewritten
    }
}

fn rewrite_db9_in_list(expr: &TypedExpr, list: &[TypedExpr], negated: bool) -> Result<TypedExpr> {
    let mut comparisons = list
        .iter()
        .map(|item| {
            if matches!(item.kind, TypedExprKind::Constant(Value::Null)) {
                Err(anyhow!(
                    "DB9 cop runtime does not support IN-list pushdown with NULL elements"
                ))
            } else {
                Ok(binary_boolean_expr(
                    expr.clone(),
                    if negated {
                        BinaryOp::NotEq
                    } else {
                        BinaryOp::Eq
                    },
                    item.clone(),
                ))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let first = comparisons
        .drain(..1)
        .next()
        .ok_or_else(|| anyhow!("DB9 cop runtime does not support empty IN lists"))?;
    Ok(comparisons.into_iter().fold(first, |acc, comparison| {
        binary_boolean_expr(
            acc,
            if negated { BinaryOp::And } else { BinaryOp::Or },
            comparison,
        )
    }))
}

fn encode_like_internal_function(case_insensitive: bool, negated: bool) -> &'static str {
    match (case_insensitive, negated) {
        (false, false) => "__db9_like",
        (true, false) => "__db9_ilike",
        (false, true) => "__db9_not_like",
        (true, true) => "__db9_not_ilike",
    }
}

fn encode_binary_op(op: &BinaryOp) -> Result<Db9BinaryOp> {
    match op {
        BinaryOp::Eq => Ok(Db9BinaryOp::Eq),
        BinaryOp::NotEq => Ok(Db9BinaryOp::Ne),
        BinaryOp::Lt => Ok(Db9BinaryOp::Lt),
        BinaryOp::LtEq => Ok(Db9BinaryOp::Le),
        BinaryOp::Gt => Ok(Db9BinaryOp::Gt),
        BinaryOp::GtEq => Ok(Db9BinaryOp::Ge),
        BinaryOp::And => Ok(Db9BinaryOp::And),
        BinaryOp::Or => Ok(Db9BinaryOp::Or),
        other => Err(anyhow!(
            "DB9 cop runtime does not support binary operator {:?}",
            other
        )),
    }
}

fn encode_unary_op(op: &UnaryOp) -> Result<Db9UnaryOp> {
    match op {
        UnaryOp::Not => Ok(Db9UnaryOp::Not),
        UnaryOp::Minus => Ok(Db9UnaryOp::Neg),
        UnaryOp::Plus => Ok(Db9UnaryOp::Pos),
        other => Err(anyhow!(
            "DB9 cop runtime does not support unary operator {:?}",
            other
        )),
    }
}

fn encode_db9_type(data_type: &DataType) -> Result<wire::Db9Type> {
    let mut encoded = wire::Db9Type::default();
    match data_type {
        DataType::Boolean => encoded.kind = Db9TypeKind::Boolean as i32,
        DataType::Int32 => encoded.kind = Db9TypeKind::Int32 as i32,
        DataType::Int64 | DataType::Oid => encoded.kind = Db9TypeKind::Int64 as i32,
        DataType::Float64 => encoded.kind = Db9TypeKind::Float64 as i32,
        DataType::Text => encoded.kind = Db9TypeKind::Text as i32,
        DataType::Bytes => encoded.kind = Db9TypeKind::Bytes as i32,
        DataType::Timestamp => encoded.kind = Db9TypeKind::Timestamp as i32,
        DataType::Interval => encoded.kind = Db9TypeKind::Interval as i32,
        DataType::Uuid => encoded.kind = Db9TypeKind::Uuid as i32,
        DataType::Array(elem_type) => {
            encoded.kind = Db9TypeKind::Array as i32;
            encoded.elem_type = Some(Box::new(encode_db9_type(elem_type)?));
        }
        DataType::Json => encoded.kind = Db9TypeKind::Json as i32,
        DataType::Jsonb => encoded.kind = Db9TypeKind::Jsonb as i32,
        DataType::Time => encoded.kind = Db9TypeKind::Time as i32,
        DataType::Date => encoded.kind = Db9TypeKind::Date as i32,
        DataType::Numeric { .. } => encoded.kind = Db9TypeKind::Numeric as i32,
        DataType::TimestampTz => encoded.kind = Db9TypeKind::Timestamptz as i32,
        DataType::Tsvector => encoded.kind = Db9TypeKind::Tsvector as i32,
        DataType::Tsquery => encoded.kind = Db9TypeKind::Tsquery as i32,
        DataType::Name => encoded.kind = Db9TypeKind::Name as i32,
        DataType::Varchar(len) => {
            encoded.kind = Db9TypeKind::Varchar as i32;
            encoded.varchar_len = *len;
        }
        DataType::UserDefined(name) => {
            encoded.kind = Db9TypeKind::UserDefined as i32;
            encoded.user_defined_name = name.clone();
        }
        DataType::Unknown => {
            return Err(anyhow!(
                "DB9 cop runtime requires resolved types; got unknown in wire contract"
            ));
        }
        DataType::Vector(_) => {
            return Err(anyhow!(
                "DB9 cop runtime does not yet encode vector columns in the wire contract"
            ));
        }
    }
    Ok(encoded)
}

fn db9_array_leaf_elem_type(elem_type: &DataType) -> &DataType {
    match elem_type {
        DataType::Array(inner) => db9_array_leaf_elem_type(inner),
        other => other,
    }
}

fn cast_decoded_db9_array_elem(value: Value, elem_type: &DataType) -> Result<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Array(inner) => Ok(Value::Array(
            inner
                .into_iter()
                .map(|value| cast_decoded_db9_array_elem(value, elem_type))
                .collect::<Result<Vec<_>>>()?,
        )),
        other => {
            use crate::sql::types::{cast::cast, CastContext};
            cast(other, elem_type, CastContext::Explicit)
        }
    }
}

fn decode_db9_array_text(value: &str, elem_type: &DataType) -> Result<Value> {
    let leaf_elem_type = db9_array_leaf_elem_type(elem_type);
    let elems = crate::sql::value_coercion::parse_pg_array(value)?
        .into_iter()
        .map(|elem| cast_decoded_db9_array_elem(elem, leaf_elem_type))
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(elems))
}

fn decode_db9_jsonb_bytes(value: &[u8]) -> Result<String> {
    if let Some(payload) = value.strip_prefix(DB9_JSONB_TEXT_MAGIC) {
        let text = String::from_utf8(payload.to_vec())
            .map_err(|err| anyhow!("invalid DB9 jsonb text payload: {err}"))?;
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|err| anyhow!("invalid DB9 jsonb payload '{text}': {err}"))?;
        return Ok(canonical_jsonb_text(&parsed));
    }

    if let Some(payload) = value.strip_prefix(DB9_JSONB_BINARY_MAGIC) {
        let parsed: serde_json::Value = rmp_serde::from_slice(payload)
            .map_err(|err| anyhow!("invalid DB9 jsonb binary payload: {err}"))?;
        return Ok(canonical_jsonb_text(&parsed));
    }

    let text = String::from_utf8(value.to_vec())
        .map_err(|err| anyhow!("invalid DB9 jsonb utf8 payload: {err}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| anyhow!("invalid DB9 jsonb payload '{text}': {err}"))?;
    Ok(canonical_jsonb_text(&parsed))
}

fn canonical_jsonb_text(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical_jsonb_text(&mut out, value);
    out
}

fn write_canonical_jsonb_text(out: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => out.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            out.push_str(
                &serde_json::to_string(value)
                    .expect("serializing a JSON string into a String should not fail"),
            );
        }
        serde_json::Value::Array(values) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_jsonb_text(out, value);
            }
            out.push(']');
        }
        serde_json::Value::Object(values) => {
            let mut items: Vec<_> = values.iter().collect();
            // PostgreSQL jsonb uses a deterministic object-key order. It is not
            // insertion order; keys are sorted by byte length first, then by raw
            // lexical byte order.
            items.sort_by(|(left_key, _), (right_key, _)| {
                left_key
                    .len()
                    .cmp(&right_key.len())
                    .then_with(|| left_key.cmp(right_key))
            });

            out.push('{');
            for (index, (key, value)) in items.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(
                    &serde_json::to_string(key)
                        .expect("serializing a JSON object key into a String should not fail"),
                );
                out.push(':');
                write_canonical_jsonb_text(out, value);
            }
            out.push('}');
        }
    }
}

fn decode_db9_text_value(value: String, expected_type: &DataType) -> Result<Value> {
    match expected_type {
        DataType::Array(elem_type) => decode_db9_array_text(&value, elem_type),
        // These logical kinds still ride on the wire's shared text carrier.
        DataType::Numeric { .. } if value.trim().eq_ignore_ascii_case("Infinity") => {
            Ok(Value::Float64(f64::INFINITY))
        }
        DataType::Numeric { .. } if value.trim().eq_ignore_ascii_case("-Infinity") => {
            Ok(Value::Float64(f64::NEG_INFINITY))
        }
        DataType::Text
        | DataType::Name
        | DataType::Varchar(_)
        | DataType::UserDefined(_)
        | DataType::Uuid
        | DataType::Json
        | DataType::Jsonb
        | DataType::Time
        | DataType::Date
        | DataType::Interval
        | DataType::Numeric { .. }
        | DataType::Tsvector
        | DataType::Tsquery => {
            crate::sql::value_coercion::parse_value_for_copy(&value, expected_type)
        }
        _ => Err(db9_cop_value_type_mismatch("text", expected_type)),
    }
}

fn decode_db9_bytes_value(value: Vec<u8>, expected_type: &DataType) -> Result<Value> {
    match expected_type {
        DataType::Bytes => Ok(Value::Bytes(value)),
        DataType::Uuid => {
            if value.len() == 16 {
                let bytes: [u8; 16] = value
                    .try_into()
                    .map_err(|_| anyhow!("invalid DB9 uuid payload length 16"))?;
                Ok(Value::Uuid(bytes))
            } else {
                let text = String::from_utf8(value)
                    .map_err(|err| anyhow!("invalid DB9 uuid utf8 payload: {err}"))?;
                crate::sql::value_coercion::parse_value_for_copy(&text, expected_type)
            }
        }
        DataType::Jsonb => Ok(Value::Jsonb(decode_db9_jsonb_bytes(&value)?)),
        _ => Err(db9_cop_value_type_mismatch("bytes", expected_type)),
    }
}

fn encode_db9_value(value: &Value, data_type: &DataType) -> Result<wire::Db9Value> {
    let kind = match value {
        Value::Null => db9_value::Kind::NullValue(wire::Db9Null {}),
        Value::Boolean(value) => match data_type {
            DataType::Boolean => db9_value::Kind::BoolValue(*value),
            other => {
                return Err(anyhow!(
                    "DB9 cop runtime cannot encode boolean literal with type {:?}",
                    other
                ));
            }
        },
        Value::Int32(value) => match data_type {
            DataType::Int32 => db9_value::Kind::Int32Value(*value),
            other => {
                return Err(anyhow!(
                    "DB9 cop runtime cannot encode int32 literal with type {:?}",
                    other
                ));
            }
        },
        Value::Int64(value) => match data_type {
            DataType::Int64 | DataType::Oid => db9_value::Kind::Int64Value(*value),
            other => {
                return Err(anyhow!(
                    "DB9 cop runtime cannot encode int64 literal with type {:?}",
                    other
                ));
            }
        },
        Value::Float64(value) => match data_type {
            DataType::Float64 => db9_value::Kind::Float64Value(*value),
            other => {
                return Err(anyhow!(
                    "DB9 cop runtime cannot encode float64 literal with type {:?}",
                    other
                ));
            }
        },
        Value::Text(value) => match data_type {
            DataType::Text | DataType::Name | DataType::Varchar(_) | DataType::Unknown => {
                db9_value::Kind::TextValue(value.clone())
            }
            other => {
                return Err(anyhow!(
                    "DB9 cop runtime cannot encode text literal with type {:?}",
                    other
                ));
            }
        },
        Value::Bytes(value) => match data_type {
            DataType::Bytes => db9_value::Kind::BytesValue(value.clone()),
            other => {
                return Err(anyhow!(
                    "DB9 cop runtime cannot encode bytes literal with type {:?}",
                    other
                ));
            }
        },
        Value::Timestamp(value) => match data_type {
            DataType::Timestamp => db9_value::Kind::TimestampValue(*value),
            DataType::TimestampTz => db9_value::Kind::TimestamptzValue(*value),
            other => {
                return Err(anyhow!(
                    "DB9 cop runtime cannot encode timestamp literal with type {:?}",
                    other
                ));
            }
        },
        other => {
            return Err(anyhow!(
                "DB9 cop runtime does not support constant value {:?}",
                other
            ));
        }
    };
    Ok(wire::Db9Value { kind: Some(kind) })
}

fn decode_db9_select_response(data: &[u8]) -> Result<wire::Db9SelectResponse> {
    wire::Db9SelectResponse::decode(data)
        .map_err(|err| anyhow!("invalid DB9 select response: {err}"))
}

fn decode_db9_row(row: wire::Db9Row, output_schema: &TableSchema) -> Result<Row> {
    let expected_cols = output_schema.columns.len();
    if row.values.len() != expected_cols {
        return Err(anyhow!(
            "DB9 cop response row has {} values, expected exactly {}",
            row.values.len(),
            expected_cols
        ));
    }

    let values = row
        .values
        .into_iter()
        .enumerate()
        .map(|(idx, value)| decode_db9_value(value, &output_schema.columns[idx].data_type))
        .collect::<Result<Vec<_>>>()?;
    Ok(Row::new(values))
}

fn decode_db9_value(value: wire::Db9Value, expected_type: &DataType) -> Result<Value> {
    match value.kind {
        Some(db9_value::Kind::NullValue(_)) => Ok(Value::Null),
        None => Err(anyhow!(
            "DB9 cop response value is missing oneof kind for expected type {:?}",
            expected_type
        )),
        Some(db9_value::Kind::BoolValue(value)) => match expected_type {
            DataType::Boolean => Ok(Value::Boolean(value)),
            _ => Err(db9_cop_value_type_mismatch("bool", expected_type)),
        },
        Some(db9_value::Kind::Int32Value(value)) => match expected_type {
            DataType::Int32 => Ok(Value::Int32(value)),
            DataType::Date => Ok(Value::Date(value)),
            _ => Err(db9_cop_value_type_mismatch("int32", expected_type)),
        },
        Some(db9_value::Kind::Int64Value(value)) => match expected_type {
            DataType::Int64 | DataType::Oid => Ok(Value::Int64(value)),
            DataType::Time => Ok(Value::Time(value)),
            _ => Err(db9_cop_value_type_mismatch("int64", expected_type)),
        },
        Some(db9_value::Kind::Float64Value(value)) => match expected_type {
            DataType::Float64 => Ok(Value::Float64(value)),
            _ => Err(db9_cop_value_type_mismatch("float64", expected_type)),
        },
        Some(db9_value::Kind::TextValue(value)) => decode_db9_text_value(value, expected_type),
        Some(db9_value::Kind::BytesValue(value)) => decode_db9_bytes_value(value, expected_type),
        Some(db9_value::Kind::TimestampValue(value)) => match expected_type {
            DataType::Timestamp => Ok(Value::Timestamp(value)),
            _ => Err(db9_cop_value_type_mismatch("timestamp", expected_type)),
        },
        Some(db9_value::Kind::TimestamptzValue(value)) => match expected_type {
            DataType::TimestampTz => Ok(Value::Timestamp(value)),
            _ => Err(db9_cop_value_type_mismatch("timestamptz", expected_type)),
        },
        Some(db9_value::Kind::IntervalValue(value)) => match expected_type {
            DataType::Interval => Ok(Value::Interval(crate::model::IntervalValue::new(
                value.months,
                value.millis,
            ))),
            _ => Err(db9_cop_value_type_mismatch("interval", expected_type)),
        },
    }
}

fn db9_cop_value_type_mismatch(actual_kind: &str, expected_type: &DataType) -> anyhow::Error {
    anyhow!(
        "DB9 cop response value kind '{}' does not match expected type {}",
        actual_kind,
        expected_type.pg_display_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnDef;
    use crate::pool::{
        run_with_statement_memory_scope, try_grow_statement_memory_scope,
        try_shrink_statement_memory_scope, TenantHandle,
    };
    use crate::sql::analyzer::types::{AnalyzedProjection, TypedExpr, TypedExprKind};
    use crate::sql::analyzer::{FunctionKind, ResolvedFunction};

    fn test_schema() -> TableSchema {
        let mut schema = TableSchema::new(
            "public.t".to_string(),
            42,
            vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            vec![0],
        );
        schema.indexes.push(crate::model::IndexDef {
            name: "t_name_idx".to_string(),
            id: 7,
            columns: vec!["name".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
            opclasses: Vec::new(),
        });
        schema
    }

    fn id_column_ref() -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: "id".to_string(),
            },
            DataType::Int32,
        )
    }

    fn operator_schema() -> TableSchema {
        TableSchema::new(
            "public.pushdown_operator_rows".to_string(),
            77,
            vec![
                ColumnDef {
                    name: "n".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "maybe_flag".to_string(),
                    data_type: DataType::Boolean,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            vec![],
        )
    }

    fn operator_n_column_ref() -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: "n".to_string(),
            },
            DataType::Int32,
        )
    }

    fn operator_maybe_flag_column_ref() -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 1,
                column_name: "maybe_flag".to_string(),
            },
            DataType::Boolean,
        )
    }

    #[test]
    fn db9_cop_codec_version_mismatch_is_reported_as_unsupported_with_hint() {
        let message = "unsupported DB9 codec_version 1, expected 3";
        let err =
            db9_cop_sql_error_from_message(message).expect("codec mismatch must map to SqlError");

        match err {
            SqlError::Unsupported(text) => {
                assert!(text.contains("unsupported DB9 codec_version"));
                assert!(text.contains("db9.enable_cop_pushdown"));
            }
            other => panic!("expected SqlError::Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn desc_index_scan_reverses_cop_response_chunks_only() {
        let asc_scan = Db9CopScan::Index {
            scan_type: ScanType::IndexRangeScan {
                index_id: 9,
                index_name: "idx".to_owned(),
                prefix_values: vec![],
            },
            desc: false,
            require_row_fetch: false,
        };
        let desc_scan = Db9CopScan::Index {
            scan_type: ScanType::IndexRangeScan {
                index_id: 9,
                index_name: "idx".to_owned(),
                prefix_values: vec![],
            },
            desc: true,
            require_row_fetch: false,
        };

        let mut asc_chunks = vec![1, 2, 3];
        order_db9_cop_responses_for_scan(&mut asc_chunks, &asc_scan);
        assert_eq!(asc_chunks, vec![1, 2, 3]);

        let mut desc_chunks = vec![1, 2, 3];
        order_db9_cop_responses_for_scan(&mut desc_chunks, &desc_scan);
        assert_eq!(desc_chunks, vec![3, 2, 1]);

        let mut seq_chunks = vec![1, 2, 3];
        order_db9_cop_responses_for_scan(&mut seq_chunks, &Db9CopScan::Seq);
        assert_eq!(seq_chunks, vec![1, 2, 3]);
    }

    #[test]
    fn encoded_db9_column_refs_match_table_schema_columns() {
        let schema = test_schema();
        let ops = vec![Db9CopOp::Project {
            projections: vec![
                AnalyzedProjection {
                    expr: id_column_ref(),
                    output_name: "id".to_owned(),
                },
                AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 1,
                            column_name: "name".to_owned(),
                        },
                        DataType::Text,
                    ),
                    output_name: "name".to_owned(),
                },
            ],
        }];

        let request =
            build_db9_dag_request(11, &schema, &Db9CopScan::Seq, &ops).expect("encode request");
        let table = request.table.as_ref().expect("table metadata");

        for named_expr in &request.projections {
            let expr = named_expr.expr.as_ref().expect("projection expr");
            let Some(db9_expr::Expr::ColumnRef(column_ref)) = expr.expr.as_ref() else {
                panic!("expected ColumnRef projection, got {:?}", expr.expr);
            };
            let index = usize::try_from(column_ref.column_index)
                .expect("column index should fit into usize");
            assert!(
                index < table.columns.len(),
                "column index {} out of bounds (len={})",
                index,
                table.columns.len()
            );
            assert_eq!(table.columns[index].name, column_ref.column_name);
        }
    }

    fn encoded_response_with_rows(row_count: usize) -> Vec<u8> {
        let response = wire::Db9SelectResponse {
            rows: (0..row_count)
                .map(|idx| wire::Db9Row {
                    values: vec![
                        wire::Db9Value {
                            kind: Some(db9_value::Kind::Int32Value(idx as i32)),
                        },
                        wire::Db9Value {
                            kind: Some(db9_value::Kind::TextValue(format!("name-{idx}"))),
                        },
                    ],
                })
                .collect(),
            stats: None,
            warnings: vec![],
        };
        response.encode_to_vec()
    }

    fn single_decoded_test_row() -> Row {
        Row::new(vec![Value::Int32(0), Value::Text("name-0".to_owned())])
    }

    fn encoded_response_with_large_warning(warning_len: usize) -> Vec<u8> {
        wire::Db9SelectResponse {
            rows: vec![wire::Db9Row {
                values: vec![
                    wire::Db9Value {
                        kind: Some(db9_value::Kind::Int32Value(0)),
                    },
                    wire::Db9Value {
                        kind: Some(db9_value::Kind::TextValue("name-0".to_owned())),
                    },
                ],
            }],
            stats: None,
            warnings: vec!["w".repeat(warning_len)],
        }
        .encode_to_vec()
    }

    #[test]
    fn decode_db9_select_response_accepts_empty_payload_as_empty_response() {
        let response = decode_db9_select_response(&[]).expect("empty proto3 payload should decode");
        assert!(response.rows.is_empty());
        assert!(response.stats.is_none());
        assert!(response.warnings.is_empty());
    }

    #[test]
    fn decode_db9_select_response_rejects_malformed_payload() {
        let err = decode_db9_select_response(&[0x0A, 0x02, 0x08])
            .expect_err("truncated protobuf payload must fail closed");
        assert!(
            err.to_string().contains("invalid DB9 select response"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decode_db9_select_chunks_accepts_mixed_empty_and_non_empty_payloads() {
        let schema = test_schema();
        let response = wire::Db9SelectResponse {
            rows: vec![wire::Db9Row {
                values: vec![
                    wire::Db9Value {
                        kind: Some(db9_value::Kind::Int32Value(7)),
                    },
                    wire::Db9Value {
                        kind: Some(db9_value::Kind::TextValue("alpha".to_owned())),
                    },
                ],
            }],
            stats: None,
            warnings: vec![],
        };

        let (rows, charged_bytes) = decode_db9_select_chunks(
            vec![((), Vec::new()), ((), response.encode_to_vec())],
            &schema,
            "db_id=11 table=public.t#42",
        )
        .expect("mixed empty/non-empty DB9 cop chunks should decode");

        assert_eq!(rows.len(), 1);
        assert!(charged_bytes > 0);
        assert_eq!(
            rows[0].values,
            vec![Value::Int32(7), Value::Text("alpha".to_owned())]
        );
    }

    #[test]
    fn decode_db9_select_chunks_allows_more_than_legacy_row_cap() {
        let schema = test_schema();
        let row_count = 50_001;
        let (rows, charged_bytes) = decode_db9_select_chunks(
            vec![((), encoded_response_with_rows(row_count))],
            &schema,
            "db_id=11 table=public.t#42",
        )
        .expect("DB9 cop buffering is byte-accounted, not capped by row count");

        assert_eq!(rows.len(), row_count);
        assert!(charged_bytes >= row_count * std::mem::size_of::<Row>());
    }

    #[tokio::test]
    async fn decode_db9_select_chunks_shares_statement_memory_quota() {
        let schema = test_schema();
        let existing_executor_bytes = 64usize;
        let row_bytes = estimate_row_size(&single_decoded_test_row());
        let handle = TenantHandle::new_with_limits(0, existing_executor_bytes + row_bytes - 1);
        let accountant = handle.memory_accountant();

        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            try_grow_statement_memory_scope("test.executor.buffer", existing_executor_bytes)
                .expect("precharge should fit below quota");
            let err = decode_db9_select_chunks(
                vec![((), encoded_response_with_rows(1))],
                &schema,
                "db_id=11 table=public.t#42",
            )
            .expect_err("cop buffer should share and exceed the statement quota");
            let sql_err = err
                .downcast_ref::<SqlError>()
                .expect("quota failures must preserve SqlError");
            assert_eq!(sql_err.sqlstate(), "53200");
            assert_eq!(
                accountant.used_bytes(),
                existing_executor_bytes,
                "failed cop buffering must not leak charged bytes"
            );
            try_shrink_statement_memory_scope(existing_executor_bytes);
        })
        .await;

        assert_eq!(accountant.used_bytes(), 0);
    }

    #[tokio::test]
    async fn decode_db9_select_chunks_allows_short_result_within_quota() {
        let schema = test_schema();
        let existing_executor_bytes = 64usize;
        let row_bytes = estimate_row_size(&single_decoded_test_row());
        let raw_chunk_bytes = encoded_response_with_rows(1).len();
        let handle = TenantHandle::new_with_limits(
            0,
            existing_executor_bytes + row_bytes.max(raw_chunk_bytes),
        );
        let accountant = handle.memory_accountant();

        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            try_grow_statement_memory_scope("test.executor.buffer", existing_executor_bytes)
                .expect("precharge should fit below quota");
            let (rows, charged_bytes) = decode_db9_select_chunks(
                vec![((), encoded_response_with_rows(1))],
                &schema,
                "db_id=11 table=public.t#42",
            )
            .expect("small OLTP-style cop result should fit the shared memory quota");
            assert_eq!(rows.len(), 1);
            assert_eq!(charged_bytes, row_bytes);
            assert_eq!(accountant.used_bytes(), existing_executor_bytes + row_bytes);

            try_shrink_statement_memory_scope(charged_bytes);
            assert_eq!(accountant.used_bytes(), existing_executor_bytes);
            try_shrink_statement_memory_scope(existing_executor_bytes);
        })
        .await;

        assert_eq!(accountant.used_bytes(), 0);
    }

    #[tokio::test]
    async fn decode_db9_select_chunks_counts_raw_chunk_bytes_against_quota() {
        let schema = test_schema();
        let existing_executor_bytes = 64usize;
        let row_bytes = estimate_row_size(&single_decoded_test_row());
        let raw_chunk_bytes = encoded_response_with_large_warning(4096).len();
        assert!(
            raw_chunk_bytes > row_bytes,
            "test requires raw chunk bytes to exceed retained row bytes"
        );
        let handle = TenantHandle::new_with_limits(0, existing_executor_bytes + row_bytes);
        let accountant = handle.memory_accountant();

        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            try_grow_statement_memory_scope("test.executor.buffer", existing_executor_bytes)
                .expect("precharge should fit below quota");
            let err = decode_db9_select_chunks(
                vec![((), encoded_response_with_large_warning(4096))],
                &schema,
                "db_id=11 table=public.t#42",
            )
            .expect_err("raw response bytes should share the statement memory quota");
            let sql_err = err
                .downcast_ref::<SqlError>()
                .expect("quota failures must preserve SqlError");
            assert_eq!(sql_err.sqlstate(), "53200");
            assert_eq!(
                accountant.used_bytes(),
                existing_executor_bytes,
                "failed raw-chunk precharge must not leak charged bytes"
            );
            try_shrink_statement_memory_scope(existing_executor_bytes);
        })
        .await;

        assert_eq!(accountant.used_bytes(), 0);
    }

    #[test]
    fn db9_cop_request_type_dag_matches_engine_contract() {
        assert_eq!(DB9_COP_REQUEST_TYPE_DAG, 10_001);
    }

    #[test]
    fn encode_db9_expr_rewrites_array_literals_to_internal_function_calls() {
        let expr = TypedExpr::new(
            TypedExprKind::ArrayLiteral(vec![
                TypedExpr::new(TypedExprKind::Constant(Value::Int32(1)), DataType::Int32),
                TypedExpr::new(TypedExprKind::Constant(Value::Int32(2)), DataType::Int32),
            ]),
            DataType::Array(Box::new(DataType::Int32)),
        );

        let encoded = encode_db9_expr(&expr).expect("array literal should encode for DB9 wire");
        match encoded.expr {
            Some(db9_expr::Expr::FuncCall(func_call)) => {
                assert_eq!(func_call.function_name, "__db9_make_array");
                assert_eq!(func_call.args.len(), 2);
            }
            other => panic!("expected __db9_make_array call, got {other:?}"),
        }
    }

    #[test]
    fn encode_db9_expr_rejects_array_literals_with_nonencodable_constant_elements() {
        let expr = TypedExpr::new(
            TypedExprKind::ArrayLiteral(vec![TypedExpr::new(
                TypedExprKind::Constant(Value::Numeric(rust_decimal::Decimal::new(55, 1))),
                DataType::Numeric {
                    precision: None,
                    scale: None,
                },
            )]),
            DataType::Array(Box::new(DataType::Numeric {
                precision: None,
                scale: None,
            })),
        );

        let err = encode_db9_expr(&expr).expect_err("non-encodable array literal should fail");
        assert!(
            err.to_string()
                .contains("DB9 cop runtime does not support constant value"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn encode_db9_expr_rejects_constant_value_type_mismatch() {
        let expr = TypedExpr::new(TypedExprKind::Constant(Value::Int64(7)), DataType::Text);

        let err = encode_db9_expr(&expr).expect_err("mismatched constant should fail to encode");
        assert!(
            err.to_string()
                .contains("cannot encode int64 literal with type Text"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn build_db9_request_ranges_seq_scan_uses_table_data_range() {
        let schema = test_schema();
        let ranges = build_db9_request_ranges(11, &schema, &Db9CopScan::Seq).unwrap();
        assert_eq!(ranges.len(), 1);

        let (start, end) = ranges[0].clone().into_keys();
        let (expected_start, expected_end) = encode_table_data_range_v2(11, schema.table_id);
        let start: Vec<u8> = start.into();
        let end: Vec<u8> = end.unwrap().into();

        assert_eq!(start, expected_start);
        assert_eq!(end, expected_end);
    }

    #[test]
    fn build_db9_dag_request_encodes_basic_filter_project_limit() {
        let schema = test_schema();
        let ops = vec![
            Db9CopOp::Filter {
                predicate: TypedExpr::new(
                    TypedExprKind::BinaryOp {
                        left: Box::new(id_column_ref()),
                        op: BinaryOp::Eq,
                        right: Box::new(TypedExpr::new(
                            TypedExprKind::Constant(Value::Int32(7)),
                            DataType::Int32,
                        )),
                    },
                    DataType::Boolean,
                ),
            },
            Db9CopOp::Project {
                projections: vec![AnalyzedProjection {
                    expr: id_column_ref(),
                    output_name: "id".to_string(),
                }],
            },
            Db9CopOp::Limit { limit: 5 },
        ];

        let request = build_db9_dag_request(11, &schema, &Db9CopScan::Seq, &ops).unwrap();
        assert_eq!(request.codec_version, DB9_COP_CODEC_VERSION);
        assert_eq!(request.db_id, 11);
        assert_eq!(
            request.scan.as_ref().map(|scan| scan.kind),
            Some(Db9ScanKind::Table as i32)
        );
        assert!(request.selection.is_some());
        assert_eq!(request.projections.len(), 1);
        assert_eq!(request.limit.as_ref().map(|limit| limit.limit), Some(5));
        assert_eq!(request.table.as_ref().map(|table| table.table_id), Some(42));
    }

    #[test]
    fn build_db9_dag_request_accepts_unknown_text_literals_in_temporal_functions() {
        let schema = TableSchema::new(
            "public.events".to_string(),
            42,
            vec![
                ColumnDef::new("id", DataType::Int32, false),
                ColumnDef::new("created_at", DataType::Timestamp, true),
            ],
            vec![0],
        );
        let ops = vec![
            Db9CopOp::Filter {
                predicate: TypedExpr::new(
                    TypedExprKind::BinaryOp {
                        left: Box::new(TypedExpr::new(
                            TypedExprKind::FunctionCall {
                                func: ResolvedFunction {
                                    name: "DATE_TRUNC".to_string(),
                                    kind: FunctionKind::Builtin,
                                    return_type: DataType::Timestamp,
                                },
                                args: vec![
                                    TypedExpr::new(
                                        TypedExprKind::Constant(Value::Text("day".to_string())),
                                        DataType::Unknown,
                                    ),
                                    TypedExpr::new(
                                        TypedExprKind::ColumnRef {
                                            scope_depth: 0,
                                            column_index: 1,
                                            column_name: "created_at".to_string(),
                                        },
                                        DataType::Timestamp,
                                    ),
                                ],
                                order_by: vec![],
                                filter: None,
                            },
                            DataType::Timestamp,
                        )),
                        op: BinaryOp::Eq,
                        right: Box::new(TypedExpr::new(
                            TypedExprKind::Constant(Value::Timestamp(1_704_153_600_000)),
                            DataType::Timestamp,
                        )),
                    },
                    DataType::Boolean,
                ),
            },
            Db9CopOp::Project {
                projections: vec![
                    AnalyzedProjection {
                        expr: TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: "id".to_string(),
                            },
                            DataType::Int32,
                        ),
                        output_name: "id".to_string(),
                    },
                    AnalyzedProjection {
                        expr: TypedExpr::new(
                            TypedExprKind::FunctionCall {
                                func: ResolvedFunction {
                                    name: "DATE_PART".to_string(),
                                    kind: FunctionKind::Builtin,
                                    return_type: DataType::Float64,
                                },
                                args: vec![
                                    TypedExpr::new(
                                        TypedExprKind::Constant(Value::Text("day".to_string())),
                                        DataType::Unknown,
                                    ),
                                    TypedExpr::new(
                                        TypedExprKind::ColumnRef {
                                            scope_depth: 0,
                                            column_index: 1,
                                            column_name: "created_at".to_string(),
                                        },
                                        DataType::Timestamp,
                                    ),
                                ],
                                order_by: vec![],
                                filter: None,
                            },
                            DataType::Float64,
                        ),
                        output_name: "created_day".to_string(),
                    },
                ],
            },
            Db9CopOp::Limit { limit: 1 },
        ];

        let request = build_db9_dag_request(11, &schema, &Db9CopScan::Seq, &ops)
            .expect("temporal function requests should encode unknown text literals");
        assert!(request.selection.is_some());
        assert_eq!(request.projections.len(), 2);
    }

    #[test]
    fn encode_db9_expr_rewrites_regex_operators_to_internal_function_calls() {
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("hello".to_string())),
                    DataType::Text,
                )),
                op: BinaryOp::RegexIMatch,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("he".to_string())),
                    DataType::Text,
                )),
            },
            DataType::Boolean,
        );

        let encoded = encode_db9_expr(&expr).unwrap();
        match encoded.expr {
            Some(db9_expr::Expr::FuncCall(func_call)) => {
                assert_eq!(func_call.function_name, "__db9_regex_imatch");
                assert_eq!(func_call.args.len(), 2);
            }
            other => panic!("expected regex internal function call, got {other:?}"),
        }
    }

    #[test]
    fn encode_db9_expr_rewrites_like_predicates_to_internal_function_calls() {
        let expr = TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("Hello".to_owned())),
                    DataType::Text,
                )),
                pattern: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("he!_%".to_owned())),
                    DataType::Text,
                )),
                escape: Some(Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("!".to_owned())),
                    DataType::Text,
                ))),
                case_insensitive: true,
                negated: false,
            },
            DataType::Boolean,
        );

        let encoded = encode_db9_expr(&expr).unwrap();
        match encoded.expr {
            Some(db9_expr::Expr::FuncCall(func_call)) => {
                assert_eq!(func_call.function_name, "__db9_ilike");
                assert_eq!(func_call.args.len(), 3);
            }
            other => panic!("expected like internal function call, got {other:?}"),
        }
    }

    #[test]
    fn encode_db9_expr_rejects_json_exists_until_paired_surface_admits_it() {
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "payload_jsonb".to_owned(),
                    },
                    DataType::Jsonb,
                )),
                op: BinaryOp::JsonExists,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("a".to_owned())),
                    DataType::Text,
                )),
            },
            DataType::Boolean,
        );

        let err = encode_db9_expr(&expr).unwrap_err();
        assert!(
            err.to_string()
                .contains("DB9 cop runtime does not support binary operator JsonExists"),
            "{err}"
        );
    }

    #[test]
    fn encode_db9_expr_lowercases_function_call_names() {
        let expr = TypedExpr::new(
            TypedExprKind::FunctionCall {
                func: crate::sql::analyzer::types::ResolvedFunction {
                    name: "UPPER".to_owned(),
                    kind: crate::sql::analyzer::types::FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("abc".to_owned())),
                    DataType::Text,
                )],
                order_by: vec![],
                filter: None,
            },
            DataType::Text,
        );

        let encoded = encode_db9_expr(&expr).unwrap();
        match encoded.expr {
            Some(db9_expr::Expr::FuncCall(func_call)) => {
                assert_eq!(func_call.function_name, "upper");
            }
            other => panic!("expected function call, got {other:?}"),
        }
    }

    #[test]
    fn encode_db9_expr_rewrites_is_distinct_from_to_supported_boolean_ops() {
        let expr = TypedExpr::new(
            TypedExprKind::IsDistinctFrom {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "maybe_flag".to_owned(),
                    },
                    DataType::Boolean,
                )),
                right: Box::new(bool_constant(true)),
                negated: false,
            },
            DataType::Boolean,
        );

        let encoded = encode_db9_expr(&expr).unwrap();
        match encoded.expr {
            Some(db9_expr::Expr::FuncCall(func_call)) => {
                assert_eq!(func_call.function_name, "coalesce");
            }
            other => panic!("expected coalesce rewrite for IS DISTINCT FROM, got {other:?}"),
        }
    }

    #[test]
    fn encode_db9_expr_rewrites_is_not_distinct_from_null_to_is_null() {
        let expr = TypedExpr::new(
            TypedExprKind::IsDistinctFrom {
                left: Box::new(operator_maybe_flag_column_ref()),
                right: Box::new(TypedExpr::null(DataType::Unknown)),
                negated: true,
            },
            DataType::Boolean,
        );

        let encoded = encode_db9_expr(&expr).unwrap();
        match encoded.expr {
            Some(db9_expr::Expr::IsNull(is_null)) => {
                assert!(!is_null.negated);
                let inner = is_null.expr.expect("missing inner expr");
                assert!(matches!(inner.expr, Some(db9_expr::Expr::ColumnRef(_))));
            }
            other => {
                panic!("expected IS NULL rewrite for IS NOT DISTINCT FROM NULL, got {other:?}")
            }
        }
    }

    #[test]
    fn encode_db9_expr_rewrites_is_not_distinct_from_non_null_constant_via_coalesce_eq() {
        let expr = TypedExpr::new(
            TypedExprKind::IsDistinctFrom {
                left: Box::new(operator_n_column_ref()),
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(20)),
                    DataType::Int32,
                )),
                negated: true,
            },
            DataType::Boolean,
        );

        let encoded = encode_db9_expr(&expr).unwrap();
        match encoded.expr {
            Some(db9_expr::Expr::FuncCall(func_call)) => {
                assert_eq!(func_call.function_name, "coalesce");
                assert_eq!(func_call.args.len(), 2);
                match func_call.args[0].expr.as_ref() {
                    Some(db9_expr::Expr::Binary(binary)) => {
                        assert_eq!(binary.op, Db9BinaryOp::Eq as i32);
                    }
                    other => panic!("expected coalesce(eq(...), false), got {other:?}"),
                }
            }
            other => panic!("expected coalesce(eq(...), false) rewrite, got {other:?}"),
        }
    }

    #[test]
    fn encode_db9_expr_rewrites_bitwise_ops_to_internal_function_calls() {
        let bitand = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(6)),
                    DataType::Int32,
                )),
                op: BinaryOp::BitwiseAnd,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(3)),
                    DataType::Int32,
                )),
            },
            DataType::Int32,
        );
        let shl = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(1)),
                    DataType::Int64,
                )),
                op: BinaryOp::ShiftLeft,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(3)),
                    DataType::Int32,
                )),
            },
            DataType::Int64,
        );
        let bitnot = TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: UnaryOp::BitwiseNot,
                operand: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(0)),
                    DataType::Int64,
                )),
            },
            DataType::Int64,
        );

        let encoded_and = encode_db9_expr(&bitand).unwrap();
        let encoded_shl = encode_db9_expr(&shl).unwrap();
        let encoded_not = encode_db9_expr(&bitnot).unwrap();

        match encoded_and.expr {
            Some(db9_expr::Expr::FuncCall(func)) => assert_eq!(func.function_name, "__db9_bitand"),
            other => panic!("expected __db9_bitand call, got {other:?}"),
        }
        match encoded_shl.expr {
            Some(db9_expr::Expr::FuncCall(func)) => assert_eq!(func.function_name, "__db9_shl"),
            other => panic!("expected __db9_shl call, got {other:?}"),
        }
        match encoded_not.expr {
            Some(db9_expr::Expr::FuncCall(func)) => assert_eq!(func.function_name, "__db9_bitnot"),
            other => panic!("expected __db9_bitnot call, got {other:?}"),
        }
    }

    #[test]
    fn encode_db9_expr_rewrites_between_and_in_list_to_boolean_combinations() {
        let between = TypedExpr::new(
            TypedExprKind::Between {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "n".to_owned(),
                    },
                    DataType::Int32,
                )),
                low: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(10)),
                    DataType::Int32,
                )),
                high: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(20)),
                    DataType::Int32,
                )),
                negated: false,
            },
            DataType::Boolean,
        );
        let in_list = TypedExpr::new(
            TypedExprKind::InList {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "n".to_owned(),
                    },
                    DataType::Int32,
                )),
                list: vec![
                    TypedExpr::new(TypedExprKind::Constant(Value::Int32(10)), DataType::Int32),
                    TypedExpr::new(TypedExprKind::Constant(Value::Int32(20)), DataType::Int32),
                ],
                negated: false,
            },
            DataType::Boolean,
        );

        let between_encoded = encode_db9_expr(&between).unwrap();
        assert!(matches!(
            between_encoded.expr,
            Some(db9_expr::Expr::Binary(_))
        ));

        let in_list_encoded = encode_db9_expr(&in_list).unwrap();
        assert!(matches!(
            in_list_encoded.expr,
            Some(db9_expr::Expr::Binary(_))
        ));
    }

    #[test]
    fn encode_db9_expr_rewrites_is_true_via_coalesce() {
        let expr = TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: 0,
                        column_name: "flag".to_owned(),
                    },
                    DataType::Boolean,
                )),
                test: IsTestKind::True,
                negated: false,
            },
            DataType::Boolean,
        );

        let encoded = encode_db9_expr(&expr).unwrap();
        match encoded.expr {
            Some(db9_expr::Expr::FuncCall(func_call)) => {
                assert_eq!(func_call.function_name, "coalesce");
            }
            other => panic!("expected coalesce rewrite for IS TRUE, got {other:?}"),
        }
    }

    #[test]
    fn build_db9_dag_request_encodes_nested_is_not_distinct_from_null_parity_shape() {
        let schema = operator_schema();
        let predicate_expr = TypedExpr::new(
            TypedExprKind::IsDistinctFrom {
                left: Box::new(operator_maybe_flag_column_ref()),
                right: Box::new(TypedExpr::null(DataType::Unknown)),
                negated: true,
            },
            DataType::Boolean,
        );
        let parity_expr = TypedExpr::new(
            TypedExprKind::IsDistinctFrom {
                left: Box::new(predicate_expr),
                right: Box::new(bool_constant(true)),
                negated: true,
            },
            DataType::Boolean,
        );
        let ops = vec![
            Db9CopOp::Filter {
                predicate: TypedExpr::new(
                    TypedExprKind::BinaryOp {
                        left: Box::new(operator_n_column_ref()),
                        op: BinaryOp::Eq,
                        right: Box::new(TypedExpr::new(
                            TypedExprKind::Constant(Value::Int32(20)),
                            DataType::Int32,
                        )),
                    },
                    DataType::Boolean,
                ),
            },
            Db9CopOp::Project {
                projections: vec![AnalyzedProjection {
                    expr: parity_expr,
                    output_name: "matches_expected".to_string(),
                }],
            },
            Db9CopOp::Limit { limit: 1 },
        ];

        let request = build_db9_dag_request(11, &schema, &Db9CopScan::Seq, &ops)
            .expect("nested IS NOT DISTINCT FROM NULL parity shape should encode");
        assert!(request.selection.is_some());
        assert_eq!(request.projections.len(), 1);
        assert_eq!(request.limit.as_ref().map(|limit| limit.limit), Some(1));
    }

    #[test]
    fn summarize_db9_request_includes_scan_ops_and_ranges() {
        let schema = test_schema();
        let summary = summarize_db9_request(
            11,
            &schema,
            &Db9CopScan::Index {
                scan_type: ScanType::InListScan {
                    index_id: 7,
                    index_name: "t_name_idx".to_string(),
                    lookup_column: Some("name".to_string()),
                    column_values: vec![vec![Value::Text("alpha".to_string())]],
                },
                desc: false,
                require_row_fetch: false,
            },
            &[
                Db9CopOp::Filter {
                    predicate: TypedExpr::new(
                        TypedExprKind::Constant(Value::Boolean(true)),
                        DataType::Boolean,
                    ),
                },
                Db9CopOp::Project {
                    projections: vec![AnalyzedProjection {
                        expr: id_column_ref(),
                        output_name: "id".to_string(),
                    }],
                },
                Db9CopOp::Limit { limit: 5 },
            ],
            2,
        );

        assert!(summary.contains("db_id=11"));
        assert!(summary.contains("table=public.t#42"));
        assert!(summary.contains("scan=in_list(name=t_name_idx, tuples=1),index_only"));
        assert!(summary.contains("ops=[filter,project(1),limit(5)]"));
        assert!(summary.contains("ranges=2"));
    }

    #[test]
    fn extract_db9_coprocessor_semantic_error_unwraps_regex_messages() {
        let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::StringError(
            "Multiple key errors: [KvError { message: \\\"regexp_split_to_array() does not support the \\\"global\\\" option\\\" }]".to_string(),
        )]);
        assert_eq!(
            extract_db9_coprocessor_semantic_error(&err).as_deref(),
            Some("regexp_split_to_array() does not support the \"global\" option")
        );
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_regex_semantic_sqlstates() {
        for (message, expected_sqlstate) in [
            ("invalid regular expression option: \"z\"", "22023"),
            (
                "regexp_split_to_array() does not support the \"global\" option",
                "22023",
            ),
            (
                "invalid regular expression: parentheses () not balanced",
                "2201B",
            ),
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("regex semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), expected_sqlstate);
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_digest_semantic_sqlstate() {
        let message = "Cannot use \" sha256 \": No such hash algorithm";
        let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
            message: message.to_string(),
        }]);
        let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
        assert_eq!(mapped.to_string(), message);
        let sql_err = mapped
            .downcast_ref::<SqlError>()
            .expect("digest semantic DB9 cop error should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "22023");
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_encoding_semantic_sqlstates() {
        for message in [
            "invalid hexadecimal digit: \"\\\\\"",
            "unrecognized encoding: \" hex \"",
            "invalid escape sequence",
            "invalid symbol \"*\" found while decoding base64 sequence",
            "invalid base64 end sequence",
            "unexpected \"=\" while decoding base64 sequence",
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("encoding semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), "22023");
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_bytea_invalid_input_sqlstate() {
        let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
            message: "invalid input syntax for type bytea".to_string(),
        }]);
        let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
        assert_eq!(
            mapped.to_string(),
            "invalid input syntax for type bytea: \"\""
        );
        let sql_err = mapped
            .downcast_ref::<SqlError>()
            .expect("bytea invalid-input DB9 cop error should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "22P02");
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_date_invalid_input_sqlstate() {
        let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
            message: "invalid DB9 date literal 'not-a-date'".to_string(),
        }]);
        let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
        assert_eq!(
            mapped.to_string(),
            "invalid input syntax for type date: \"not-a-date\""
        );
        let sql_err = mapped
            .downcast_ref::<SqlError>()
            .expect("date semantic DB9 cop error should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "22P02");
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_datetime_field_overflow_sqlstates() {
        for message in [
            "date field value out of range: 2024-02-30",
            "time field value out of range: 24:00:1e-06",
            "timestamp field value out of range",
            "MAKE_TIME second field value out of range",
            "MAKE_TIMESTAMP timestamp field value out of range",
            "timestamp cannot be NaN",
            "timestamp out of range: \"1e+20\"",
            "interval out of range",
            "TO_TIMESTAMP epoch must be finite",
            "invalid DB9 timestamp value",
            "DB9 function 'make_date' date field value out of range",
            "DB9 function 'make_time' time field value out of range",
            "DB9 function 'make_timestamp' date field value out of range",
            "DB9 function 'make_timestamp' time field value out of range",
            "DB9 function 'age' interval is out of range",
            "DB9 function 'to_timestamp' timestamp cannot be NaN",
            "DB9 function 'to_timestamp' timestamp out of range",
            "DB9 function 'to_timestamp' timestamp field value out of range",
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("datetime semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), "22008");
            assert!(matches!(sql_err, SqlError::DatetimeFieldOverflow { .. }));
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_maps_date_messages_before_joining() {
        let err = tikv_client::Error::ExtractedErrors(vec![
            tikv_client::Error::KvError {
                message: "invalid DB9 date literal 'first-bad-date'".to_string(),
            },
            tikv_client::Error::KvError {
                message: "invalid DB9 date literal 'second-bad-date'".to_string(),
            },
        ]);
        assert_eq!(
            extract_db9_coprocessor_semantic_error(&err).as_deref(),
            Some(
                "invalid DB9 date literal 'first-bad-date'; invalid DB9 date literal 'second-bad-date'"
            )
        );

        let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
        assert_eq!(
            mapped.to_string(),
            "invalid input syntax for type date: \"first-bad-date\""
        );
        let sql_err = mapped
            .downcast_ref::<SqlError>()
            .expect("date semantic DB9 cop error should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "22P02");
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_invalid_parameter_value_sqlstates() {
        for message in [
            "field position must not be zero",
            "character number must be positive",
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("invalid-parameter semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), "22023");
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_invalid_escape_sqlstates() {
        for message in [
            "invalid escape string",
            "LIKE pattern must not end with escape character",
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("escape semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), "22025");
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_value_limit_sqlstates() {
        for message in [
            "null character not permitted",
            "requested length too large",
            "requested character too large for encoding: 1114112",
            "requested character not valid for encoding: 55296",
            "failed to decode DB9 row: stored row payload size 33554433 exceeds decode limit 33554432",
            "failed to decode DB9 row: stored row payload exceeds decode limit 33554432",
            "DB9 response size 1048580 exceeds coprocessor max_resp_size 1048576",
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("value-limit semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), "54000");
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_math_semantic_sqlstates() {
        for (message, expected_sqlstate) in [
            ("cannot take square root of a negative number", "2201F"),
            (
                "a negative number raised to a non-integer power yields a complex result",
                "2201F",
            ),
            ("zero raised to a negative power is undefined", "2201F"),
            ("cannot take logarithm of zero", "2201E"),
            ("cannot take logarithm of a negative number", "2201E"),
            (
                "DB9 function 'width_bucket' operand, lower bound, and upper bound cannot be NaN",
                "2201G",
            ),
            (
                "DB9 function 'width_bucket' lower and upper bounds must be finite",
                "2201G",
            ),
            (
                "DB9 function 'width_bucket' count must be greater than zero",
                "2201G",
            ),
            (
                "DB9 function 'width_bucket' lower bound cannot equal upper bound",
                "2201G",
            ),
            ("input is out of range", "22003"),
            ("value out of range: underflow", "22003"),
            ("value overflows numeric format", "22003"),
            ("integer out of range", "22003"),
            ("bigint out of range", "22003"),
            ("DB9 numeric value is out of range", "22003"),
            ("division by zero", "22012"),
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("math semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), expected_sqlstate);
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_substring_sqlstate() {
        let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
            message: "negative substring length not allowed".to_string(),
        }]);
        let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
        assert_eq!(mapped.to_string(), "negative substring length not allowed");
        let sql_err = mapped
            .downcast_ref::<SqlError>()
            .expect("substring semantic DB9 cop error should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "22011");
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_preserves_array_semantic_sqlstates() {
        for (message, expected_sqlstate) in [
            (
                "searching for elements in multidimensional arrays is not supported",
                "0A000",
            ),
            (
                "removing elements from multidimensional arrays is not supported",
                "0A000",
            ),
            ("argument must be empty or one-dimensional array", "22000"),
            ("cannot concatenate incompatible arrays", "2202E"),
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("array semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), expected_sqlstate);
        }
    }

    #[test]
    fn map_db9_coprocessor_rpc_error_keeps_request_summary_for_non_semantic_failures() {
        let mapped = map_db9_coprocessor_rpc_error(
            tikv_client::Error::StringError("boom".to_string()),
            "db_id=11 table=public.t#42",
        );
        assert_eq!(
            mapped.to_string(),
            "DB9 coprocessor RPC failed for db_id=11 table=public.t#42"
        );
    }

    #[test]
    fn encode_db9_value_uses_timestamp_variant_for_timestamp_type() {
        let encoded =
            encode_db9_value(&Value::Timestamp(1_700_000_000_000), &DataType::Timestamp).unwrap();
        assert!(matches!(
            encoded.kind,
            Some(db9_value::Kind::TimestampValue(1_700_000_000_000))
        ));
    }

    #[test]
    fn encode_db9_value_uses_timestamptz_variant_for_timestamptz_type() {
        let encoded =
            encode_db9_value(&Value::Timestamp(1_700_000_000_000), &DataType::TimestampTz).unwrap();
        assert!(matches!(
            encoded.kind,
            Some(db9_value::Kind::TimestamptzValue(1_700_000_000_000))
        ));
    }

    #[test]
    fn decode_db9_row_restores_timestamp_values_using_output_schema() {
        let row = wire::Db9Row {
            values: vec![wire::Db9Value {
                kind: Some(db9_value::Kind::TimestampValue(1_700_000_000_000)),
            }],
        };
        let schema = TableSchema::new(
            "public.events".to_string(),
            1,
            vec![ColumnDef {
                name: "created_at".to_string(),
                data_type: DataType::Timestamp,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            }],
            vec![],
        );

        let decoded = decode_db9_row(row, &schema).unwrap();
        assert_eq!(decoded.values, vec![Value::Timestamp(1_700_000_000_000)]);
    }

    #[test]
    fn decode_db9_row_restores_interval_values_using_output_schema() {
        let row = wire::Db9Row {
            values: vec![wire::Db9Value {
                kind: Some(db9_value::Kind::IntervalValue(wire::Db9IntervalValue {
                    months: 1,
                    millis: 16 * 24 * 60 * 60 * 1000 + 19 * 60 * 60 * 1000 + 25 * 60 * 1000 + 3_211,
                })),
            }],
        };
        let schema = TableSchema::new(
            "public.events".to_string(),
            1,
            vec![ColumnDef {
                name: "elapsed".to_string(),
                data_type: DataType::Interval,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            }],
            vec![],
        );

        let decoded = decode_db9_row(row, &schema).unwrap();
        assert_eq!(
            decoded.values,
            vec![Value::Interval(crate::model::IntervalValue::new(
                1,
                16 * 24 * 60 * 60 * 1000 + 19 * 60 * 60 * 1000 + 25 * 60 * 1000 + 3_211,
            ))]
        );
    }

    #[test]
    fn decode_db9_row_rejects_short_rows_even_with_defaultable_output_schema() {
        let row = wire::Db9Row {
            values: vec![wire::Db9Value {
                kind: Some(db9_value::Kind::Int32Value(7)),
            }],
        };
        let schema = TableSchema::new(
            "public.users".to_string(),
            1,
            vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "dept_id".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: Some("1".to_string()),
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            vec![0],
        );

        let err = decode_db9_row(row, &schema).expect_err("short DB9 cop row must fail closed");
        assert!(
            err.to_string().contains("expected exactly 2"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decode_db9_value_rejects_missing_kind() {
        let err = decode_db9_value(wire::Db9Value { kind: None }, &DataType::Int32)
            .expect_err("missing oneof kind must fail closed");
        assert!(
            err.to_string().contains("missing oneof kind"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decode_db9_value_rejects_mismatched_scalar_kind() {
        let err = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::Int64Value(7)),
            },
            &DataType::Int32,
        )
        .expect_err("mismatched scalar kind must fail closed");
        assert!(
            err.to_string()
                .contains("does not match expected type integer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decode_db9_value_accepts_text_for_name_and_varchar() {
        let name = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("postgres".to_owned())),
            },
            &DataType::Name,
        )
        .unwrap();
        let varchar = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("tenant".to_owned())),
            },
            &DataType::Varchar(64),
        )
        .unwrap();

        assert_eq!(name, Value::Text("postgres".to_owned()));
        assert_eq!(varchar, Value::Text("tenant".to_owned()));
    }

    #[test]
    fn decode_db9_value_accepts_text_for_user_defined_types() {
        let decoded = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("udt-payload".to_owned())),
            },
            &DataType::UserDefined("my_udt".to_owned()),
        )
        .unwrap();
        assert_eq!(decoded, Value::Text("udt-payload".to_owned()));
    }

    #[test]
    fn decode_db9_value_accepts_int64_for_oid() {
        let decoded = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::Int64Value(26)),
            },
            &DataType::Oid,
        )
        .unwrap();
        assert_eq!(decoded, Value::Int64(26));
    }

    #[test]
    fn decode_db9_value_accepts_text_carriers_for_richer_logical_types() {
        let numeric = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("5.5".to_owned())),
            },
            &DataType::Numeric {
                precision: None,
                scale: None,
            },
        )
        .unwrap();
        assert_eq!(numeric, Value::Numeric(rust_decimal::Decimal::new(55, 1)));

        let numeric_infinity = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("Infinity".to_owned())),
            },
            &DataType::Numeric {
                precision: None,
                scale: None,
            },
        )
        .unwrap();
        assert_eq!(numeric_infinity, Value::Float64(f64::INFINITY));

        let date = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("2024-01-02".to_owned())),
            },
            &DataType::Date,
        )
        .unwrap();
        assert_eq!(date, Value::Date(19_724));

        let time = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("01:02:03.004005".to_owned())),
            },
            &DataType::Time,
        )
        .unwrap();
        assert_eq!(time, Value::Time(3_723_004_005));

        let interval = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("1 day".to_owned())),
            },
            &DataType::Interval,
        )
        .unwrap();
        assert_eq!(
            interval,
            Value::Interval(crate::model::IntervalValue::from_millis(
                24 * 60 * 60 * 1000
            ))
        );

        let json = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("{\"b\":2,\"a\":1}".to_owned())),
            },
            &DataType::Json,
        )
        .unwrap();
        assert_eq!(json, Value::Json("{\"b\":2,\"a\":1}".to_owned()));

        let jsonb = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("{\"b\":2,\"a\":1}".to_owned())),
            },
            &DataType::Jsonb,
        )
        .unwrap();
        assert_eq!(jsonb, Value::Jsonb("{\"a\":1,\"b\":2}".to_owned()));

        let tsvector = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("'alpha' 'beta'".to_owned())),
            },
            &DataType::Tsvector,
        )
        .unwrap();
        assert_eq!(tsvector, Value::Tsvector("'alpha' 'beta'".to_owned()));

        let tsquery = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("alpha & beta".to_owned())),
            },
            &DataType::Tsquery,
        )
        .unwrap();
        assert_eq!(tsquery, Value::Tsquery("alpha & beta".to_owned()));
    }

    #[test]
    fn decode_db9_value_accepts_scalar_carriers_for_date_and_time() {
        let date = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::Int32Value(19_724)),
            },
            &DataType::Date,
        )
        .unwrap();
        assert_eq!(date, Value::Date(19_724));

        let time = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::Int64Value(3_723_004_005)),
            },
            &DataType::Time,
        )
        .unwrap();
        assert_eq!(time, Value::Time(3_723_004_005));
    }

    #[test]
    fn decode_db9_value_accepts_uuid_and_jsonb_bytes_carriers() {
        let uuid = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::BytesValue([0x44; 16].to_vec())),
            },
            &DataType::Uuid,
        )
        .unwrap();
        assert_eq!(uuid, Value::Uuid([0x44; 16]));

        let jsonb_binary = {
            let payload = rmp_serde::to_vec(&std::collections::BTreeMap::from([
                ("b", 2_i64),
                ("a", 1_i64),
            ]))
            .unwrap();
            let mut encoded = Vec::with_capacity(DB9_JSONB_BINARY_MAGIC.len() + payload.len());
            encoded.extend_from_slice(DB9_JSONB_BINARY_MAGIC);
            encoded.extend_from_slice(&payload);
            encoded
        };
        let jsonb = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::BytesValue(jsonb_binary)),
            },
            &DataType::Jsonb,
        )
        .unwrap();
        assert_eq!(jsonb, Value::Jsonb("{\"a\":1,\"b\":2}".to_owned()));

        let jsonb_text = {
            let text = r#"{"n":9007199254740993.123456789}"#;
            let mut encoded = Vec::with_capacity(DB9_JSONB_TEXT_MAGIC.len() + text.len());
            encoded.extend_from_slice(DB9_JSONB_TEXT_MAGIC);
            encoded.extend_from_slice(text.as_bytes());
            encoded
        };
        let jsonb = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::BytesValue(jsonb_text)),
            },
            &DataType::Jsonb,
        )
        .unwrap();
        assert_eq!(
            jsonb,
            Value::Jsonb(r#"{"n":9007199254740993.123456789}"#.to_owned())
        );

        let legacy_utf8_jsonb = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::BytesValue(b"{\"b\":2,\"a\":1}".to_vec())),
            },
            &DataType::Jsonb,
        )
        .unwrap();
        assert_eq!(
            legacy_utf8_jsonb,
            Value::Jsonb("{\"a\":1,\"b\":2}".to_owned())
        );

        let jsonb_text = {
            let text = r#"{"aa":{"bb":2,"a":1},"b":3,"a":4}"#;
            let mut encoded = Vec::with_capacity(DB9_JSONB_TEXT_MAGIC.len() + text.len());
            encoded.extend_from_slice(DB9_JSONB_TEXT_MAGIC);
            encoded.extend_from_slice(text.as_bytes());
            encoded
        };
        let jsonb = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::BytesValue(jsonb_text)),
            },
            &DataType::Jsonb,
        )
        .unwrap();
        assert_eq!(
            jsonb,
            Value::Jsonb(r#"{"a":4,"b":3,"aa":{"a":1,"bb":2}}"#.to_owned())
        );
    }

    #[test]
    fn decode_db9_value_preserves_quoted_empty_array_text_elements() {
        let decoded = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("{\"\"}".to_owned())),
            },
            &DataType::Array(Box::new(DataType::Text)),
        )
        .unwrap();
        assert_eq!(decoded, Value::Array(vec![Value::Text(String::new())]));
    }

    #[test]
    fn decode_db9_value_preserves_quoted_text_array_lexemes() {
        let decoded = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue(
                    "{\" 01 \",\"true\",\"NULL\",\"a,b\"}".to_owned(),
                )),
            },
            &DataType::Array(Box::new(DataType::Text)),
        )
        .unwrap();
        assert_eq!(
            decoded,
            Value::Array(vec![
                Value::Text(" 01 ".to_string()),
                Value::Text("true".to_string()),
                Value::Text("NULL".to_string()),
                Value::Text("a,b".to_string()),
            ])
        );
    }

    #[test]
    fn decode_db9_value_preserves_multidimensional_array_shape_for_flat_declared_type() {
        let decoded = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("{{1,2},{3,NULL}}".to_owned())),
            },
            &DataType::Array(Box::new(DataType::Int32)),
        )
        .unwrap();
        assert_eq!(
            decoded,
            Value::Array(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
                Value::Array(vec![Value::Int32(3), Value::Null]),
            ])
        );
    }

    #[test]
    fn decode_db9_value_preserves_multidimensional_array_shape_for_nested_declared_type() {
        let decoded = decode_db9_value(
            wire::Db9Value {
                kind: Some(db9_value::Kind::TextValue("{{1,2},{3,4}}".to_owned())),
            },
            &DataType::Array(Box::new(DataType::Array(Box::new(DataType::Int32)))),
        )
        .unwrap();
        assert_eq!(
            decoded,
            Value::Array(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
                Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
            ])
        );
    }
}
