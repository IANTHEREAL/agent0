//! Shared error/parse helpers for COPY protocol handling.

use crate::model::{DataType, Value};
use crate::sql::Executor;
use pgwire::error::{PgWireError, PgWireResult};

use super::super::super::errors::{sqlstate_for_executor_error, user_error};

pub(super) fn copy_display_table_name(resolved_table: &str) -> &str {
    resolved_table.rsplit('.').next().unwrap_or(resolved_table)
}

pub(super) fn copy_missing_data_error(
    resolved_table: &str,
    column_name: &str,
    line_no: usize,
    raw_line: &str,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    user_error(
        "22P04",
        format!(
            "missing data for column \"{}\"\nCONTEXT:  COPY {}, line {}: \"{}\"",
            column_name, table, line_no, raw_line
        ),
    )
}

pub(super) fn copy_extra_data_after_last_expected_column_error(
    resolved_table: &str,
    line_no: usize,
    raw_line: &str,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    user_error(
        "22P04",
        format!(
            "extra data after last expected column\nCONTEXT:  COPY {}, line {}: \"{}\"",
            table, line_no, raw_line
        ),
    )
}

pub(super) fn copy_value_parse_error(
    resolved_table: &str,
    line_no: usize,
    column_name: &str,
    raw_value: &str,
    err: &anyhow::Error,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    user_error(
        sqlstate_for_executor_error(err),
        format!(
            "{}\nCONTEXT:  COPY {}, line {}, column {}: \"{}\"",
            err, table, line_no, column_name, raw_value
        ),
    )
}

pub(super) fn parse_copy_text_line(
    executor: &Executor,
    resolved_table: &str,
    columns: &[String],
    column_types: &[Option<DataType>],
    line_no: usize,
    line_bytes: &[u8],
    copy_opts: &crate::protocol::copy_format::CopyOptions,
) -> PgWireResult<Vec<(String, Value)>> {
    let line = String::from_utf8_lossy(line_bytes);
    let fields = line
        .split(copy_opts.delimiter as char)
        .map(|val| CopyInputField {
            value: val.to_string(),
            was_quoted: false,
        })
        .collect::<Vec<_>>();
    parse_copy_values(
        executor,
        resolved_table,
        columns,
        column_types,
        line_no,
        line.as_ref(),
        &fields,
        copy_opts,
    )
}

struct CopyInputField {
    value: String,
    was_quoted: bool,
}

fn parse_copy_values(
    executor: &Executor,
    resolved_table: &str,
    columns: &[String],
    column_types: &[Option<DataType>],
    line_no: usize,
    raw_line: &str,
    fields: &[CopyInputField],
    copy_opts: &crate::protocol::copy_format::CopyOptions,
) -> PgWireResult<Vec<(String, Value)>> {
    if fields.len() > columns.len() {
        return Err(copy_extra_data_after_last_expected_column_error(
            resolved_table,
            line_no,
            raw_line,
        ));
    }

    let mut col_values: Vec<(String, Value)> = Vec::with_capacity(columns.len());
    for (idx, (col_name, col_type)) in columns.iter().zip(column_types.iter()).enumerate() {
        let Some(field) = fields.get(idx) else {
            return Err(copy_missing_data_error(
                resolved_table,
                col_name,
                line_no,
                raw_line,
            ));
        };
        let val = field.value.as_str();
        let csv_quoted_non_null = matches!(
            copy_opts.format,
            crate::protocol::copy_format::CopyFormat::Csv
        ) && field.was_quoted;

        let value = if !csv_quoted_non_null && val == copy_opts.null_string.as_str() {
            Value::Null
        } else if let Some(dt) = col_type.as_ref() {
            executor
                .parse_value_for_copy(val, dt)
                .map_err(|e| copy_value_parse_error(resolved_table, line_no, col_name, val, &e))?
        } else {
            Value::Text(val.to_string())
        };
        col_values.push((col_name.clone(), value));
    }

    Ok(col_values)
}

fn copy_csv_parse_error(
    resolved_table: &str,
    line_no: usize,
    raw_line: &str,
    err: &csv::Error,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    user_error(
        "22P04",
        format!(
            "CSV parse error at line {}: {}\nCONTEXT:  COPY {}, line {}: \"{}\"",
            line_no, err, table, line_no, raw_line
        ),
    )
}

fn parse_csv_quoted_flags(
    line_bytes: &[u8],
    copy_opts: &crate::protocol::copy_format::CopyOptions,
) -> Vec<bool> {
    if line_bytes.is_empty() {
        return Vec::new();
    }

    let delimiter = copy_opts.delimiter;
    let quote = copy_opts.quote;
    let escape = copy_opts.escape;
    let len = line_bytes.len();
    let mut flags = Vec::new();
    let mut idx = 0;

    while idx < len {
        let mut was_quoted = false;
        if line_bytes[idx] == quote {
            was_quoted = true;
            idx += 1;

            while idx < len {
                let b = line_bytes[idx];
                if escape != quote && b == escape && idx + 1 < len {
                    idx += 2;
                    continue;
                }
                if b == quote {
                    if escape == quote && idx + 1 < len && line_bytes[idx + 1] == quote {
                        idx += 2;
                        continue;
                    }
                    idx += 1;
                    break;
                }
                idx += 1;
            }

            while idx < len && line_bytes[idx] != delimiter {
                idx += 1;
            }
        } else {
            while idx < len && line_bytes[idx] != delimiter {
                idx += 1;
            }
        }

        flags.push(was_quoted);
        if idx >= len {
            break;
        }

        idx += 1;
        if idx == len {
            flags.push(false);
            break;
        }
    }

    flags
}

fn parse_copy_csv_line(
    executor: &Executor,
    resolved_table: &str,
    columns: &[String],
    column_types: &[Option<DataType>],
    line_no: usize,
    line_bytes: &[u8],
    copy_opts: &crate::protocol::copy_format::CopyOptions,
) -> PgWireResult<Vec<(String, Value)>> {
    let raw_line = String::from_utf8_lossy(line_bytes);

    let mut builder = csv::ReaderBuilder::new();
    builder
        .has_headers(false)
        .flexible(true)
        .delimiter(copy_opts.delimiter)
        .quote(copy_opts.quote);
    if copy_opts.escape == copy_opts.quote {
        builder.double_quote(true);
    } else {
        builder.double_quote(false).escape(Some(copy_opts.escape));
    }
    let mut rdr = builder.from_reader(line_bytes);

    let fields = match rdr.records().next() {
        Some(Ok(record)) => {
            let mut quoted_flags = parse_csv_quoted_flags(line_bytes, copy_opts);
            if quoted_flags.len() < record.len() {
                quoted_flags.resize(record.len(), false);
            } else if quoted_flags.len() > record.len() {
                quoted_flags.truncate(record.len());
            }

            record
                .iter()
                .zip(quoted_flags)
                .map(|(value, was_quoted)| CopyInputField {
                    value: value.to_string(),
                    was_quoted,
                })
                .collect::<Vec<_>>()
        }
        Some(Err(err)) => {
            return Err(copy_csv_parse_error(
                resolved_table,
                line_no,
                &raw_line,
                &err,
            ))
        }
        None if !columns.is_empty() => {
            vec![CopyInputField {
                value: String::new(),
                was_quoted: false,
            }]
        }
        None => Vec::new(),
    };

    parse_copy_values(
        executor,
        resolved_table,
        columns,
        column_types,
        line_no,
        raw_line.as_ref(),
        &fields,
        copy_opts,
    )
}

pub(super) fn parse_copy_input_line(
    executor: &Executor,
    resolved_table: &str,
    columns: &[String],
    column_types: &[Option<DataType>],
    line_no: usize,
    line_bytes: &[u8],
    copy_opts: &crate::protocol::copy_format::CopyOptions,
) -> PgWireResult<Vec<(String, Value)>> {
    match copy_opts.format {
        crate::protocol::copy_format::CopyFormat::Text => parse_copy_text_line(
            executor,
            resolved_table,
            columns,
            column_types,
            line_no,
            line_bytes,
            copy_opts,
        ),
        crate::protocol::copy_format::CopyFormat::Csv => parse_copy_csv_line(
            executor,
            resolved_table,
            columns,
            column_types,
            line_no,
            line_bytes,
            copy_opts,
        ),
        crate::protocol::copy_format::CopyFormat::Parquet => {
            Err(user_error("0A000", "Parquet format does not support STDIN"))
        }
    }
}

pub(super) fn should_add_copy_insert_context(err: &anyhow::Error) -> bool {
    use crate::sql::error::SqlError;
    matches!(
        err.downcast_ref::<SqlError>(),
        Some(SqlError::UniqueViolation { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::copy_format::{CopyFormat, CopyOptions};
    use pgwire::error::PgWireError;
    use std::sync::Arc;

    fn test_executor() -> Executor {
        let store = crate::storage::TikvStore::new_stub();
        let keyspace = "copy_helpers_csv_blank_row".to_string();
        let observability = crate::observability::registry().tenant(&keyspace);
        let trigger_cache = Arc::new(crate::sql::triggers::TriggerBodyCache::new());
        let rls_policy_cache = Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
        let stats_cache = Arc::new(crate::sql::stats::TableStatsCache::new());
        Executor::new(
            store,
            keyspace,
            observability,
            crate::pool::TenantMemoryAccountant::unlimited("copy_helpers".to_string()),
            trigger_cache,
            rls_policy_cache,
            stats_cache,
        )
    }

    #[test]
    fn csv_blank_row_reports_missing_second_column() {
        let executor = test_executor();
        let copy_opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: String::new(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let columns = vec!["a".to_string(), "b".to_string()];
        let column_types = vec![None, None];

        let err = parse_copy_input_line(
            &executor,
            "public.t_copy",
            &columns,
            &column_types,
            1,
            b"",
            &copy_opts,
        )
        .expect_err("blank CSV row with two columns should report missing column");

        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22P04");
                assert!(info.message.contains("missing data for column \"b\""));
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn csv_extra_column_reports_pg_extra_data_message() {
        let executor = test_executor();
        let copy_opts = CopyOptions {
            format: CopyFormat::Csv,
            delimiter: b',',
            null_string: String::new(),
            header: false,
            quote: b'"',
            escape: b'"',
        };
        let columns = vec!["a".to_string(), "b".to_string()];
        let column_types = vec![None, None];

        let err = parse_copy_input_line(
            &executor,
            "public.t_copy",
            &columns,
            &column_types,
            1,
            b"1,2,3",
            &copy_opts,
        )
        .expect_err("CSV row with too many fields should report extra data");

        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22P04");
                assert!(info
                    .message
                    .contains("extra data after last expected column"));
                assert!(info
                    .message
                    .contains("CONTEXT:  COPY t_copy, line 1: \"1,2,3\""));
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }
}
