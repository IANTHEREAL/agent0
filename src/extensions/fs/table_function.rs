use super::*;

/// Infer the output schema for an `fs9(...)` table function call without decoding all rows.
///
/// This is used by the Analyzer prefetch phase so it can resolve column names/types
/// for dynamic schemas (directory listing vs file vs csv headers) while keeping
/// analysis itself synchronous.
///
/// Schema inference is an analysis-time fs9 entry point: it opens the backend,
/// can lazily materialize the JuiceFS volume, and reads file contents (CSV
/// headers, parquet footers) to derive a schema. It MUST therefore enforce the
/// same superuser authorization as every other fs9 SQL surface — otherwise
/// catalog prefetch (and `EXPLAIN`, which analyzes but never executes) would
/// open the backend and read fs9 IO for a non-superuser before the
/// execution-time gate is ever reached.
///
/// Authorization is checked here (mirroring the parquet analysis path
/// `parquet::reader::create_fs9_reader`); teardown/lifecycle fail-closed is
/// enforced where the volume is actually opened, in
/// `acquire_statement_backend` -> `init_backend`.
pub(crate) async fn infer_table_function_schema(
    tenant: &str,
    mode: &Fs9Mode,
) -> Result<TableSchema> {
    if !backend::is_backend_available() {
        anyhow::bail!("fs9: TiKV storage backend not available");
    }
    if !crate::extensions::context::is_superuser() {
        anyhow::bail!("fs9: permission denied (superuser required)");
    }
    let backend = backend::acquire_statement_backend(tenant).await?;
    let backend = backend.as_ref();

    match mode {
        Fs9Mode::Directory { .. } => Ok(decoders::decode_directory(Vec::new()).schema),
        Fs9Mode::File {
            path,
            format,
            delimiter,
            header,
        } => {
            let info = backend.stat(path).await?;
            if info.is_dir {
                return Ok(decoders::decode_directory(Vec::new()).schema);
            }

            let data = backend.read_file(path, MAX_BYTES_PER_FILE).await?;
            let fmt = decoders::detect_format(path, format.as_deref());
            match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        *delimiter
                    };
                    let decoded = decoders::decode_csv(&data, path, delim, *header, 0)
                        .map_err(|e| anyhow::anyhow!("fs9: CSV decode error: {e}"))?;
                    Ok(decoded.schema)
                }
                "jsonl" | "ndjson" => Ok(decoders::decode_jsonl(&data, path, 0).schema),
                #[cfg(feature = "parquet")]
                "parquet" => {
                    let data = backend.read_file(path, crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES).await?;
                    let reader = crate::extensions::parquet::fs9_reader::Fs9ParquetReader::new(bytes::Bytes::from(data));
                    let schema = crate::extensions::parquet::reader::infer_schema_from_reader(reader).await
                        .map_err(|e| anyhow::anyhow!("fs9: parquet schema inference error: {e}"))?;
                    Ok(schema)
                }
                #[cfg(not(feature = "parquet"))]
                "parquet" => {
                    Err(anyhow::anyhow!("fs9: parquet format requires the parquet extension (compile with --features parquet)"))
                }
                _ => Ok(decoders::decode_raw_text(&data, path, 0).schema),
            }
        }
        Fs9Mode::Glob {
            pattern,
            format,
            delimiter,
            header,
            exclude,
        } => {
            let matching_files =
                glob::expand_glob(backend, pattern, MAX_FILES_PER_GLOB, exclude.as_deref()).await?;

            if matching_files.is_empty() {
                return Ok(decoders::decode_raw_text(&[], pattern, 0).schema);
            }

            let first = &matching_files[0];
            let data = backend.read_file(first, MAX_BYTES_PER_FILE).await?;
            let fmt = decoders::detect_format(first, format.as_deref());

            match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        *delimiter
                    };
                    let decoded = decoders::decode_csv(&data, first, delim, *header, 0)
                        .map_err(|e| anyhow::anyhow!("fs9: CSV decode error in {}: {e}", first))?;
                    Ok(decoded.schema)
                }
                "jsonl" | "ndjson" => Ok(decoders::decode_jsonl(&data, first, 0).schema),
                _ => Ok(decoders::decode_raw_text(&data, first, 0).schema),
            }
        }
    }
}

pub(crate) async fn execute_table_function(
    tenant: &str,
    mode: Fs9Mode,
) -> Result<(TableSchema, Vec<Row>)> {
    let backend = backend::acquire_statement_backend(tenant).await?;
    execute_table_function_with_budget_for_backend(tenant, backend.as_ref(), mode, MAX_TOTAL_BYTES)
        .await
}

async fn execute_table_function_with_budget_for_backend(
    tenant: &str,
    backend: &dyn backend::FsBackend,
    mode: Fs9Mode,
    max_total_bytes: usize,
) -> Result<(TableSchema, Vec<Row>)> {
    let log_budget_exhausted =
        |total_bytes_read: usize, files_read_count: usize, total_files: usize| {
            crate::metrics::record_fs9_glob_truncated(tenant, "table_function");
            warn!(
                "fs9: bytes budget exhausted ({} MB), {} of {} matched files were scanned",
                total_bytes_read / (1024 * 1024),
                files_read_count,
                total_files
            );
        };

    match mode {
        Fs9Mode::Directory {
            path,
            recursive,
            exclude,
        } => {
            let exclude_set = glob::build_exclude_globset(exclude.as_deref())?;
            let entries = list_directory_entries(backend, &path, recursive, exclude_set).await?;
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
                    let decoded = decoders::decode_csv(&data, &path, delim, header, usize::MAX)
                        .map_err(|e| anyhow::anyhow!("fs9: CSV decode error: {e}"))?;
                    Ok((decoded.schema, decoded.rows))
                }
                "jsonl" | "ndjson" => {
                    let decoded = decoders::decode_jsonl(&data, &path, usize::MAX);
                    Ok((decoded.schema, decoded.rows))
                }
                #[cfg(feature = "parquet")]
                "parquet" => {
                    let data = backend.read_file(&path, crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES).await?;
                    let reader = crate::extensions::parquet::fs9_reader::Fs9ParquetReader::new(bytes::Bytes::from(data));
                    let (parquet_schema, row_stream) = crate::extensions::parquet::reader::open_row_stream_from_reader(reader, 8192).await
                        .map_err(|e| anyhow::anyhow!("fs9: parquet decode error: {e}"))?;
                    let mut rows = Vec::new();
                    futures::pin_mut!(row_stream);
                    while let Some(values) = futures::StreamExt::next(&mut row_stream).await {
                        let values = values.map_err(|e| anyhow::anyhow!("fs9: parquet row error: {e}"))?;
                        rows.push(crate::model::Row::new(values));
                    }
                    Ok((parquet_schema, rows))
                }
                #[cfg(not(feature = "parquet"))]
                "parquet" => {
                    Err(anyhow::anyhow!("fs9: parquet format requires the parquet extension (compile with --features parquet)"))
                }
                _ => {
                    let decoded = decoders::decode_raw_text(&data, &path, usize::MAX);
                    Ok((decoded.schema, decoded.rows))
                }
            }
        }
        Fs9Mode::Glob {
            pattern,
            format,
            delimiter,
            header,
            exclude,
        } => {
            let matching_files =
                glob::expand_glob(backend, &pattern, MAX_FILES_PER_GLOB, exclude.as_deref())
                    .await?;

            if matching_files.is_empty() {
                let decoded = decoders::decode_raw_text(&[], &pattern, 0);
                return Ok((decoded.schema, decoded.rows));
            }

            let mut all_rows: Vec<Row> = Vec::new();
            let mut result_schema: Option<TableSchema> = None;
            let mut base_file_path: Option<String> = None;
            let mut total_bytes_read: usize = 0;

            for (files_read_count, file_path) in matching_files.iter().enumerate() {
                let remaining_budget = max_total_bytes.saturating_sub(total_bytes_read);
                if remaining_budget == 0 {
                    log_budget_exhausted(total_bytes_read, files_read_count, matching_files.len());
                    break;
                }

                let file_info = backend.stat(file_path).await?;
                let file_size = usize::try_from(file_info.size).unwrap_or(usize::MAX);
                if file_size > MAX_BYTES_PER_FILE {
                    return Err(anyhow::anyhow!(
                        "fs9: file too large: {} bytes exceeds limit {}",
                        file_size,
                        MAX_BYTES_PER_FILE
                    ));
                }
                if file_size > remaining_budget {
                    log_budget_exhausted(total_bytes_read, files_read_count, matching_files.len());
                    break;
                }

                let max_bytes_for_file = remaining_budget.min(MAX_BYTES_PER_FILE);
                let data = backend.read_file(file_path, max_bytes_for_file).await?;
                total_bytes_read = total_bytes_read.saturating_add(data.len());

                let fmt = decoders::detect_format(file_path, format.as_deref());

                let decoded = match fmt {
                    "csv" | "tsv" => {
                        let delim = if fmt == "tsv" && delimiter.is_none() {
                            Some('\t')
                        } else {
                            delimiter
                        };
                        decoders::decode_csv(&data, file_path, delim, header, usize::MAX).map_err(
                            |e| anyhow::anyhow!("fs9: CSV decode error in {}: {e}", file_path),
                        )?
                    }
                    "jsonl" | "ndjson" => decoders::decode_jsonl(&data, file_path, usize::MAX),
                    _ => decoders::decode_raw_text(&data, file_path, usize::MAX),
                };

                match &result_schema {
                    None => {
                        base_file_path = Some(file_path.clone());
                        result_schema = Some(decoded.schema.clone());
                    }
                    Some(base_schema) if fmt == "csv" || fmt == "tsv" => {
                        let base_cols = decoders::csv_user_column_names(base_schema);
                        let cur_cols = decoders::csv_user_column_names(&decoded.schema);
                        if base_cols != cur_cols {
                            return Err(anyhow::anyhow!(
                                "fs9 glob schema mismatch: '{}' has columns {:?} but '{}' has columns {:?}",
                                file_path,
                                cur_cols,
                                base_file_path.as_deref().unwrap_or("(first file)"),
                                base_cols
                            ));
                        }
                    }
                    _ => {}
                }

                all_rows.extend(decoded.rows);
            }

            let schema =
                result_schema.unwrap_or_else(|| decoders::decode_raw_text(&[], &pattern, 0).schema);

            Ok((schema, all_rows))
        }
    }
}

#[cfg(test)]
pub(crate) async fn execute_table_function_with_budget_for_test_backend(
    backend: Box<dyn backend::FsBackend>,
    mode: Fs9Mode,
    max_total_bytes: usize,
) -> Result<(TableSchema, Vec<Row>)> {
    execute_table_function_with_budget_for_backend("test", backend.as_ref(), mode, max_total_bytes)
        .await
}
