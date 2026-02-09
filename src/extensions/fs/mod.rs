use anyhow::{anyhow, Result};

use crate::extensions::context;
use crate::types::{ColumnDef, DataType, Row, TableSchema};

pub(crate) mod backend;
pub(crate) mod decoders;
pub(crate) mod glob;

pub(crate) enum Fs9Mode {
    Directory {
        path: String,
        recursive: bool,
    },
    File {
        path: String,
        format: Option<String>,
        delimiter: Option<char>,
        header: Option<bool>,
    },
    Glob {
        pattern: String,
        format: Option<String>,
        delimiter: Option<char>,
        header: Option<bool>,
    },
}

pub(crate) const MAX_ROWS_PER_QUERY: usize = 10_000;
pub(crate) const MAX_BYTES_PER_FILE: usize = 10 * 1024 * 1024;
pub(crate) const MAX_FILES_PER_GLOB: usize = 10_000;

fn fs9_file_schema(name: &str) -> TableSchema {
    TableSchema {
        table_id: 0,
        name: name.to_string(),
        columns: vec![
            ColumnDef {
                name: "_line_number".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "line".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "_path".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
    }
}

pub(crate) fn table_function_schema(func_name: &str) -> Option<TableSchema> {
    let name = func_name.trim().to_ascii_lowercase();
    match name.as_str() {
        "fs9" => Some(fs9_file_schema(&name)),
        _ => None,
    }
}

pub(crate) async fn execute_table_function(
    _tenant: &str,
    mode: Fs9Mode,
) -> Result<(TableSchema, Vec<Row>)> {
    if !context::is_superuser() {
        return Err(anyhow!("permission denied for extension \"fs9\""));
    }

    use backend::FsBackend;

    let backend = backend::local_backend();

    match mode {
        Fs9Mode::Directory { path, recursive } => {
            let _ = recursive;
            let entries = backend.readdir(&path).await?;
            let decoded = decoders::decode_directory(entries);
            Ok((decoded.schema, decoded.rows))
        }
        Fs9Mode::File {
            path,
            format,
            delimiter,
            header,
        } => {
            let info = backend.stat(&path).await?;
            if info.is_dir {
                let entries = backend.readdir(&path).await?;
                let decoded = decoders::decode_directory(entries);
                return Ok((decoded.schema, decoded.rows));
            }

            let data = backend.read_file(&path, MAX_BYTES_PER_FILE).await?;
            let fmt = decoders::detect_format(&path, format.as_deref());

            match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        delimiter
                    };
                    let decoded = decoders::decode_csv(&data, &path, delim, header, MAX_ROWS_PER_QUERY)
                        .map_err(|e| anyhow!("fs9: CSV decode error: {e}"))?;
                    Ok((decoded.schema, decoded.rows))
                }
                "jsonl" | "ndjson" => {
                    let decoded = decoders::decode_jsonl(&data, &path, MAX_ROWS_PER_QUERY);
                    Ok((decoded.schema, decoded.rows))
                }
                _ => {
                    let decoded = decoders::decode_raw_text(&data, &path, MAX_ROWS_PER_QUERY);
                    Ok((decoded.schema, decoded.rows))
                }
            }
        }
        Fs9Mode::Glob {
            pattern,
            format,
            delimiter,
            header,
        } => {
            let matching_files = glob::expand_glob(backend, &pattern, MAX_FILES_PER_GLOB).await?;

            if matching_files.is_empty() {
                let decoded = decoders::decode_raw_text(&[], &pattern, 0);
                return Ok((decoded.schema, decoded.rows));
            }

            let mut all_rows: Vec<Row> = Vec::new();
            let mut result_schema: Option<TableSchema> = None;

            for file_path in &matching_files {
                if all_rows.len() >= MAX_ROWS_PER_QUERY {
                    break;
                }

                let data = backend.read_file(file_path, MAX_BYTES_PER_FILE).await?;
                let fmt = decoders::detect_format(file_path, format.as_deref());
                let remaining = MAX_ROWS_PER_QUERY - all_rows.len();

                let decoded = match fmt {
                    "csv" | "tsv" => {
                        let delim = if fmt == "tsv" && delimiter.is_none() {
                            Some('\t')
                        } else {
                            delimiter
                        };
                        decoders::decode_csv(&data, file_path, delim, header, remaining)
                            .map_err(|e| anyhow!("fs9: CSV decode error in {}: {e}", file_path))?
                    }
                    "jsonl" | "ndjson" => decoders::decode_jsonl(&data, file_path, remaining),
                    _ => decoders::decode_raw_text(&data, file_path, remaining),
                };

                if result_schema.is_none() {
                    result_schema = Some(decoded.schema.clone());
                }

                all_rows.extend(decoded.rows);
            }

            let schema = result_schema.unwrap_or_else(|| decoders::decode_raw_text(&[], &pattern, 0).schema);

            Ok((schema, all_rows))
        }
    }
}
