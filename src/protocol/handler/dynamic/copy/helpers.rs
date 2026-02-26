//! Shared error/parse helpers for COPY protocol handling.

use crate::model::{DataType, Value};
use crate::sql::Executor;
use pgwire::error::{PgWireError, PgWireResult};

use super::super::super::copy::copy_row_column_mismatch_error;
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
) -> PgWireResult<Vec<(String, Value)>> {
    let line = String::from_utf8_lossy(line_bytes);
    let values: Vec<&str> = line.split('\t').collect();

    if values.len() > columns.len() {
        return Err(copy_row_column_mismatch_error(values.len(), columns.len()));
    }

    let mut col_values: Vec<(String, Value)> = Vec::with_capacity(columns.len());
    for (idx, (col_name, col_type)) in columns.iter().zip(column_types.iter()).enumerate() {
        let Some(val) = values.get(idx).copied() else {
            return Err(copy_missing_data_error(
                resolved_table,
                col_name,
                line_no,
                line.as_ref(),
            ));
        };

        let value = if val == "\\N" {
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

pub(super) fn should_add_copy_insert_context(err: &anyhow::Error) -> bool {
    use crate::sql::error::SqlError;
    matches!(
        err.downcast_ref::<SqlError>(),
        Some(SqlError::UniqueViolation { .. })
    )
}
