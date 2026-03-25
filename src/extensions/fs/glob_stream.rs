use super::*;

pub(crate) async fn start_glob_stream(
    tenant: &str,
    pattern: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
    exclude: Option<&str>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    start_glob_stream_with_budget(
        tenant,
        pattern,
        format,
        delimiter,
        header,
        exclude,
        MAX_TOTAL_BYTES,
    )
    .await
}

async fn start_glob_stream_with_budget(
    tenant: &str,
    pattern: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
    exclude: Option<&str>,
    max_total_bytes: usize,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    let backend = backend::acquire_statement_backend(tenant).await?;
    start_glob_stream_with_budget_for_backend(
        backend,
        pattern,
        format,
        delimiter,
        header,
        exclude,
        max_total_bytes,
    )
    .await
}

async fn start_glob_stream_with_budget_for_backend(
    backend: Arc<dyn backend::FsBackend>,
    pattern: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
    exclude: Option<&str>,
    max_total_bytes: usize,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    let matching_files = glob::expand_glob(&*backend, pattern, MAX_FILES_PER_GLOB, exclude).await?;
    if matching_files.is_empty() {
        return Ok(None);
    }

    let fmt = decoders::detect_format(&matching_files[0], format);

    let (schema, probe_file) = {
        let mut found = None;
        for probe_path in &matching_files {
            let reader = match backend
                .read_file_stream(probe_path, MAX_BYTES_PER_FILE)
                .await
            {
                Ok(r) => r,
                Err(err) => {
                    warn!("fs9: skipping {} during schema probe: {}", probe_path, err);
                    continue;
                }
            };
            let result = match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        delimiter
                    };
                    let has_headers = header.unwrap_or(true);
                    streaming::StreamingCsvDecoder::new(
                        reader,
                        probe_path.clone(),
                        delim,
                        has_headers,
                    )
                    .await
                    .map(|d| d.schema().clone())
                    .map_err(|(e, _)| e)
                }
                "jsonl" | "ndjson" => {
                    let decoder = streaming::StreamingJsonlDecoder::new(reader, probe_path.clone());
                    Ok(decoder.schema().clone())
                }
                _ => {
                    let decoder = streaming::StreamingTextDecoder::new(reader, probe_path.clone());
                    Ok(decoder.schema().clone())
                }
            };
            match result {
                Ok(s) => {
                    found = Some((s, probe_path.clone()));
                    break;
                }
                Err(err) => {
                    warn!("fs9: skipping {} during schema probe: {}", probe_path, err);
                }
            }
        }
        found.ok_or_else(|| {
            anyhow::anyhow!(
                "fs9: no readable files found matching pattern '{}'",
                pattern
            )
        })?
    };

    if fmt == "csv" || fmt == "tsv" {
        let base_cols = decoders::csv_user_column_names(&schema);
        let delim = if fmt == "tsv" && delimiter.is_none() {
            Some('\t')
        } else {
            delimiter
        };

        const HEADER_PROBE_BYTES: usize = 8192;
        for file_path in &matching_files {
            if *file_path == probe_file {
                continue;
            }
            let data = match backend.read_file(file_path, HEADER_PROBE_BYTES).await {
                Ok(d) => d,
                Err(err) => {
                    warn!(
                        "fs9: skipping {} during header validation: {}",
                        file_path, err
                    );
                    continue;
                }
            };
            let file_schema =
                match decoders::decode_csv_header_only(&data, file_path, delim, header) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!(
                            "fs9: skipping {} during header validation: {}",
                            file_path, err
                        );
                        continue;
                    }
                };
            let file_cols = decoders::csv_user_column_names(&file_schema);
            if base_cols != file_cols {
                return Err(anyhow::anyhow!(
                    "fs9 glob schema mismatch: '{}' has columns {:?} but '{}' has columns {:?}",
                    file_path,
                    file_cols,
                    probe_file,
                    base_cols
                ));
            }
        }
    }

    let (tx, rx) = mpsc::channel(256);
    let fmt_owned = fmt.to_string();
    let pattern_owned = pattern.to_string();
    tokio::spawn(async move {
        let mut total_bytes: usize = 0;
        let mut files_read_count: usize = 0;

        for file_path in matching_files {
            let remaining_budget = max_total_bytes.saturating_sub(total_bytes);
            if remaining_budget == 0 {
                warn!(
                    "fs9: bytes budget exhausted ({} MB), {} files were streamed for pattern {}",
                    total_bytes / (1024 * 1024),
                    files_read_count,
                    pattern_owned
                );
                break;
            }

            let file_limit = remaining_budget.min(MAX_BYTES_PER_FILE);
            let reader = match backend.read_file_stream(&file_path, file_limit).await {
                Ok(reader) => reader,
                Err(err) => {
                    warn!("fs9: cannot stream {}: {}", file_path, err);
                    continue;
                }
            };

            match fmt_owned.as_str() {
                "csv" | "tsv" => {
                    let delim = if fmt_owned == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        delimiter
                    };
                    let has_headers = header.unwrap_or(true);
                    let mut decoder = match streaming::StreamingCsvDecoder::new(
                        reader,
                        file_path.clone(),
                        delim,
                        has_headers,
                    )
                    .await
                    {
                        Ok(decoder) => decoder,
                        Err((err, bytes_read)) => {
                            warn!("fs9: streaming decode error for {}: {}", file_path, err);
                            total_bytes = total_bytes.saturating_add(bytes_read);
                            continue;
                        }
                    };

                    loop {
                        match decoder.next_row().await {
                            Ok(Some(row)) => {
                                if tx.send(row).await.is_err() {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(err) => {
                                warn!("fs9: streaming decode error for {}: {}", file_path, err);
                                break;
                            }
                        }
                    }

                    total_bytes = total_bytes.saturating_add(decoder.bytes_read());
                    files_read_count += 1;
                }
                "jsonl" | "ndjson" => {
                    let mut decoder =
                        streaming::StreamingJsonlDecoder::new(reader, file_path.clone());

                    loop {
                        match decoder.next_row().await {
                            Ok(Some(row)) => {
                                if tx.send(row).await.is_err() {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(err) => {
                                warn!("fs9: streaming decode error for {}: {}", file_path, err);
                                break;
                            }
                        }
                    }

                    total_bytes = total_bytes.saturating_add(decoder.bytes_read());
                    files_read_count += 1;
                }
                _ => {
                    let mut decoder =
                        streaming::StreamingTextDecoder::new(reader, file_path.clone());

                    loop {
                        match decoder.next_row().await {
                            Ok(Some(row)) => {
                                if tx.send(row).await.is_err() {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(err) => {
                                warn!("fs9: streaming decode error for {}: {}", file_path, err);
                                break;
                            }
                        }
                    }

                    total_bytes = total_bytes.saturating_add(decoder.bytes_read());
                    files_read_count += 1;
                }
            }

            if total_bytes >= max_total_bytes {
                warn!(
                    "fs9: bytes budget exhausted ({} MB), {} files were streamed for pattern {}",
                    total_bytes / (1024 * 1024),
                    files_read_count,
                    pattern_owned
                );
                break;
            }
        }
    });

    Ok(Some((schema, rx)))
}

#[cfg(test)]
pub(crate) async fn start_glob_stream_with_budget_for_test_backend(
    backend: Box<dyn backend::FsBackend>,
    pattern: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
    exclude: Option<&str>,
    max_total_bytes: usize,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    start_glob_stream_with_budget_for_backend(
        Arc::from(backend),
        pattern,
        format,
        delimiter,
        header,
        exclude,
        max_total_bytes,
    )
    .await
}
