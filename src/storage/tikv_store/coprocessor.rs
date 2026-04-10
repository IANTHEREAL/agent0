use super::*;
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{
    AnalyzedProjection, BinaryOp, IsTestKind, TypedExpr, TypedExprKind, UnaryOp,
};
use crate::sql::error::SqlError;
use crate::sql::optimizer::physical_plan::{Db9CopOp, Db9CopScan};
use crate::sql::ScanType;
use anyhow::{anyhow, Context, Result};
use prost::Message;
use std::sync::LazyLock;
use tikv_client::proto::db9_coprocessor::{
    self as wire, db9_expr, db9_value, Db9BinaryOp, Db9ScanKind, Db9TypeKind, Db9UnaryOp,
};
use tikv_client::BoundRange;

const DB9_COP_CODEC_VERSION: u32 = 1;
// Must stay aligned with the engine-side REQ_TYPE_DB9_DAG contract.
const DB9_COP_REQUEST_TYPE_DAG: i64 = 10_001;
const DEFAULT_DB9_COP_MAX_BUFFERED_ROWS: usize = 50_000;
const DB9_JSONB_BINARY_MAGIC: &[u8] = b"\0db9jb1";
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

fn db9_cop_sql_error_from_message(message: &str) -> Option<SqlError> {
    let unsupported_global_regex_option = message.starts_with("regexp_")
        && message.ends_with("() does not support the \"global\" option");

    if message.starts_with("invalid regular expression option: ") || unsupported_global_regex_option
    {
        return Some(SqlError::InvalidParameterValue {
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

fn extract_db9_coprocessor_semantic_error(err: &tikv_client::Error) -> Option<String> {
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
    if messages.is_empty() {
        None
    } else {
        Some(messages.join("; "))
    }
}

fn map_db9_coprocessor_rpc_error(err: tikv_client::Error, request_summary: &str) -> anyhow::Error {
    if let Some(message) = extract_db9_coprocessor_semantic_error(&err) {
        if let Some(sql_err) = db9_cop_sql_error_from_message(&message) {
            sql_err.into()
        } else {
            anyhow!(message)
        }
    } else {
        anyhow::Error::new(err).context(format!("DB9 coprocessor RPC failed for {request_summary}"))
    }
}

fn db9_cop_max_buffered_rows() -> usize {
    std::env::var("DB9_COP_MAX_BUFFERED_ROWS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_DB9_COP_MAX_BUFFERED_ROWS)
}

fn ensure_db9_cop_buffer_limit(
    buffered_rows: usize,
    next_chunk_rows: usize,
    max_rows: usize,
    request_summary: &str,
) -> Result<()> {
    let total_rows = buffered_rows
        .checked_add(next_chunk_rows)
        .ok_or_else(|| anyhow!("DB9 cop buffered row count overflowed for {request_summary}"))?;
    if total_rows > max_rows {
        return Err(anyhow!(
            "DB9 cop buffered row limit exceeded for {request_summary}: {total_rows} rows exceeds limit {max_rows}; reduce result size or set DB9_COP_MAX_BUFFERED_ROWS to raise the cap"
        ));
    }
    Ok(())
}

impl TikvStore {
    pub async fn cop_select(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_schema: &TableSchema,
        scan: &Db9CopScan,
        ops: &[Db9CopOp],
        output_schema: &TableSchema,
    ) -> Result<Vec<Row>> {
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
        let max_buffered_rows = db9_cop_max_buffered_rows();
        let responses = txn
            .coprocessor(
                DB9_COP_REQUEST_TYPE_DAG,
                request.encode_to_vec(),
                ranges.clone(),
            )
            .await
            .map_err(|err| map_db9_coprocessor_rpc_error(err, &request_summary))?;
        decode_db9_select_chunks(
            responses,
            output_schema,
            max_buffered_rows,
            &request_summary,
        )
    }
}

fn decode_db9_select_chunks<I, R>(
    responses: I,
    output_schema: &TableSchema,
    max_buffered_rows: usize,
    request_summary: &str,
) -> Result<Vec<Row>>
where
    I: IntoIterator<Item = (R, Vec<u8>)>,
{
    let mut rows = Vec::new();

    for (chunk_index, (_meta, data)) in responses.into_iter().enumerate() {
        let response = decode_db9_select_response(&data).with_context(|| {
            format!(
                "failed to decode DB9 cop response chunk {} for {}",
                chunk_index, request_summary
            )
        })?;
        let decoded_rows = decode_db9_rows(response, output_schema).with_context(|| {
            format!(
                "failed to decode DB9 cop rows from chunk {} for {}",
                chunk_index, request_summary
            )
        })?;
        ensure_db9_cop_buffer_limit(
            rows.len(),
            decoded_rows.len(),
            max_buffered_rows,
            request_summary,
        )?;
        rows.extend(decoded_rows);
    }

    Ok(rows)
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
        Db9CopScan::Index { scan_type } => summarize_db9_scan_type(scan_type),
    }
}

fn summarize_db9_scan_type(scan_type: &ScanType) -> String {
    match scan_type {
        ScanType::FullTableScan => "full_table".to_string(),
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
        Db9CopScan::Index { scan_type } => {
            let (kind, index_id, index_name) = match scan_type {
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
                desc: false,
                require_row_fetch: true,
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
        Db9CopScan::Seq => {
            let (start, end) = encode_table_data_range_v2(db_id, table_schema.table_id);
            Ok(vec![(start..end).into()])
        }
        Db9CopScan::Index { scan_type } => match scan_type {
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

fn encode_db9_expr(expr: &TypedExpr) -> Result<wire::Db9Expr> {
    let return_type = encode_db9_type(&expr.data_type)?;
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
        TypedExprKind::BinaryOp { left, op, right } => {
            db9_expr::Expr::Binary(Box::new(wire::Db9BinaryExpr {
                left: Some(Box::new(encode_db9_expr(left)?)),
                op: encode_binary_op(op)? as i32,
                right: Some(Box::new(encode_db9_expr(right)?)),
            }))
        }
        TypedExprKind::UnaryOp { op, operand } => {
            db9_expr::Expr::Unary(Box::new(wire::Db9UnaryExpr {
                op: encode_unary_op(op)? as i32,
                operand: Some(Box::new(encode_db9_expr(operand)?)),
            }))
        }
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
            test: IsTestKind::Null,
            negated,
        } => db9_expr::Expr::IsNull(Box::new(wire::Db9IsNullExpr {
            expr: Some(Box::new(encode_db9_expr(inner)?)),
            negated: *negated,
        })),
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
                function_name: func.name.clone(),
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
    if let Some(payload) = value.strip_prefix(DB9_JSONB_BINARY_MAGIC) {
        let parsed: serde_json::Value = rmp_serde::from_slice(payload)
            .map_err(|err| anyhow!("invalid DB9 jsonb binary payload: {err}"))?;
        return Ok(parsed.to_string());
    }

    let text = String::from_utf8(value.to_vec())
        .map_err(|err| anyhow!("invalid DB9 jsonb utf8 payload: {err}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| anyhow!("invalid DB9 jsonb payload '{text}': {err}"))?;
    Ok(parsed.to_string())
}

fn decode_db9_text_value(value: String, expected_type: &DataType) -> Result<Value> {
    match expected_type {
        DataType::Array(elem_type) => decode_db9_array_text(&value, elem_type),
        // These logical kinds still ride on the wire's shared text carrier.
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
        Value::Boolean(value) => db9_value::Kind::BoolValue(*value),
        Value::Int32(value) => db9_value::Kind::Int32Value(*value),
        Value::Int64(value) => db9_value::Kind::Int64Value(*value),
        Value::Float64(value) => db9_value::Kind::Float64Value(*value),
        Value::Text(value) => db9_value::Kind::TextValue(value.clone()),
        Value::Bytes(value) => db9_value::Kind::BytesValue(value.clone()),
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

fn decode_db9_rows(
    response: wire::Db9SelectResponse,
    output_schema: &TableSchema,
) -> Result<Vec<Row>> {
    response
        .rows
        .into_iter()
        .map(|row| decode_db9_row(row, output_schema))
        .collect()
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
    use crate::sql::analyzer::types::{AnalyzedProjection, TypedExpr, TypedExprKind};

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
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
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

        let rows = decode_db9_select_chunks(
            vec![((), Vec::new()), ((), response.encode_to_vec())],
            &schema,
            db9_cop_max_buffered_rows(),
            "db_id=11 table=public.t#42",
        )
        .expect("mixed empty/non-empty DB9 cop chunks should decode");

        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values,
            vec![Value::Int32(7), Value::Text("alpha".to_owned())]
        );
    }

    #[test]
    fn db9_cop_request_type_dag_matches_engine_contract() {
        assert_eq!(DB9_COP_REQUEST_TYPE_DAG, 10_001);
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
        assert!(summary.contains("scan=in_list(name=t_name_idx, tuples=1)"));
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
        for message in [
            "invalid regular expression option: \"z\"",
            "regexp_split_to_array() does not support the \"global\" option",
        ] {
            let err = tikv_client::Error::ExtractedErrors(vec![tikv_client::Error::KvError {
                message: message.to_string(),
            }]);
            let mapped = map_db9_coprocessor_rpc_error(err, "ignored request summary");
            assert_eq!(mapped.to_string(), message);
            let sql_err = mapped
                .downcast_ref::<SqlError>()
                .expect("regex semantic DB9 cop error should preserve SQLSTATE");
            assert_eq!(sql_err.sqlstate(), "22023");
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
            let payload = rmp_serde::to_vec(&serde_json::json!({"b": 2, "a": 1})).unwrap();
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
    }

    #[test]
    fn ensure_db9_cop_buffer_limit_fails_closed() {
        let err = ensure_db9_cop_buffer_limit(49_999, 2, 50_000, "db_id=11 table=public.t#42")
            .expect_err("buffer limit must fail closed");
        assert!(
            err.to_string().contains("buffered row limit exceeded"),
            "unexpected error: {err}"
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
