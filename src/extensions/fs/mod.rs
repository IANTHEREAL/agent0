use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::model::{Row, TableSchema};
use std::collections::HashSet;
use tracing::warn;

pub(crate) mod backend;
pub(crate) mod channel_reader;
pub(crate) mod config;
pub(crate) mod decoders;
pub(crate) mod embedded;
pub(crate) mod glob;
pub(crate) mod s3;
pub(crate) mod streaming;
pub(crate) mod upload_token;
pub(crate) mod ws;

pub(crate) enum Fs9Mode {
    Directory {
        path: String,
        recursive: bool,
        exclude: Option<String>,
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
        exclude: Option<String>,
    },
}

pub(crate) const MAX_BYTES_PER_FILE: usize = 100 * 1024 * 1024;
pub(crate) const MAX_FILES_PER_GLOB: usize = 10_000;
pub(crate) const MAX_TOTAL_BYTES: usize = 100 * 1024 * 1024;

/// Infer the output schema for an `fs9(...)` table function call without decoding all rows.
///
/// This is used by the Analyzer prefetch phase so it can resolve column names/types
/// for dynamic schemas (directory listing vs file vs csv headers) while keeping
/// analysis itself synchronous.
pub(crate) async fn infer_table_function_schema(
    tenant: &str,
    mode: &Fs9Mode,
) -> Result<TableSchema> {
    let backend = backend::get_backend(tenant).await?;
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
                        .map_err(|e| anyhow!("fs9: CSV decode error: {e}"))?;
                    Ok(decoded.schema)
                }
                "jsonl" | "ndjson" => Ok(decoders::decode_jsonl(&data, path, 0).schema),
                #[cfg(feature = "parquet")]
                "parquet" => {
                    let data = backend.read_file(path, crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES).await?;
                    let reader = crate::extensions::parquet::fs9_reader::Fs9ParquetReader::new(bytes::Bytes::from(data));
                    let schema = crate::extensions::parquet::reader::infer_schema_from_reader(reader).await
                        .map_err(|e| anyhow!("fs9: parquet schema inference error: {e}"))?;
                    Ok(schema)
                }
                #[cfg(not(feature = "parquet"))]
                "parquet" => {
                    Err(anyhow!("fs9: parquet format requires the parquet extension (compile with --features parquet)"))
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
                        .map_err(|e| anyhow!("fs9: CSV decode error in {}: {e}", first))?;
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
    let backend = backend::get_backend(tenant).await?;
    execute_table_function_with_budget_for_backend(backend.as_ref(), mode, MAX_TOTAL_BYTES).await
}

async fn execute_table_function_with_budget_for_backend(
    backend: &dyn backend::FsBackend,
    mode: Fs9Mode,
    max_total_bytes: usize,
) -> Result<(TableSchema, Vec<Row>)> {
    let log_budget_exhausted =
        |total_bytes_read: usize, files_read_count: usize, total_files: usize| {
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
            let entries =
                list_directory_entries(backend, &path, recursive, exclude_set.as_ref()).await?;
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
                        .map_err(|e| anyhow!("fs9: CSV decode error: {e}"))?;
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
                        .map_err(|e| anyhow!("fs9: parquet decode error: {e}"))?;
                    let mut rows = Vec::new();
                    futures::pin_mut!(row_stream);
                    while let Some(values) = futures::StreamExt::next(&mut row_stream).await {
                        let values = values.map_err(|e| anyhow!("fs9: parquet row error: {e}"))?;
                        rows.push(crate::model::Row::new(values));
                    }
                    Ok((parquet_schema, rows))
                }
                #[cfg(not(feature = "parquet"))]
                "parquet" => {
                    Err(anyhow!("fs9: parquet format requires the parquet extension (compile with --features parquet)"))
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
                    return Err(anyhow!(
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
                        decoders::decode_csv(&data, file_path, delim, header, usize::MAX)
                            .map_err(|e| anyhow!("fs9: CSV decode error in {}: {e}", file_path))?
                    }
                    "jsonl" | "ndjson" => decoders::decode_jsonl(&data, file_path, usize::MAX),
                    _ => decoders::decode_raw_text(&data, file_path, usize::MAX),
                };

                if result_schema.is_none() {
                    result_schema = Some(decoded.schema.clone());
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
async fn execute_table_function_with_budget_for_test_backend(
    backend: Box<dyn backend::FsBackend>,
    mode: Fs9Mode,
    max_total_bytes: usize,
) -> Result<(TableSchema, Vec<Row>)> {
    execute_table_function_with_budget_for_backend(backend.as_ref(), mode, max_total_bytes).await
}

pub(crate) async fn start_file_stream(
    tenant: &str,
    path: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    let backend = backend::get_backend(tenant).await?;
    start_file_stream_for_backend(backend, path, format, delimiter, header).await
}

async fn start_file_stream_for_backend(
    backend: Box<dyn backend::FsBackend>,
    path: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    let info = backend.stat(path).await?;
    if info.is_dir {
        return Ok(None);
    }

    let fmt = decoders::detect_format(path, format);

    // Parquet uses a full read_file (parquet needs random access), so skip
    // spawning a streaming reader that would race on the inode atime key.
    #[cfg(feature = "parquet")]
    if fmt == "parquet" {
        let data = backend
            .read_file(
                path,
                crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES,
            )
            .await?;
        let reader =
            crate::extensions::parquet::fs9_reader::Fs9ParquetReader::new(bytes::Bytes::from(data));
        let (parquet_schema, row_stream) =
            crate::extensions::parquet::reader::open_row_stream_from_reader(reader, 8192)
                .await
                .map_err(|e| anyhow!("fs9: parquet stream error: {e}"))?;
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            futures::pin_mut!(row_stream);
            while let Some(values) = futures::StreamExt::next(&mut row_stream).await {
                match values {
                    Ok(vals) => {
                        if tx.send(crate::model::Row::new(vals)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::warn!("fs9: parquet streaming error: {}", err);
                        break;
                    }
                }
            }
        });
        return Ok(Some((parquet_schema, rx)));
    }

    #[cfg(not(feature = "parquet"))]
    if fmt == "parquet" {
        return Err(anyhow!(
            "fs9: parquet format requires the parquet extension (compile with --features parquet)"
        ));
    }

    let reader = backend.read_file_stream(path, MAX_BYTES_PER_FILE).await?;

    let (schema, rx) = match fmt {
        "csv" | "tsv" => {
            let delim = if fmt == "tsv" && delimiter.is_none() {
                Some('\t')
            } else {
                delimiter
            };
            let has_headers = header.unwrap_or(true);
            let mut decoder =
                streaming::StreamingCsvDecoder::new(reader, path.to_string(), delim, has_headers)
                    .await
                    .map_err(|(e, _)| e)?;
            let schema = decoder.schema().clone();
            let (tx, rx) = mpsc::channel(256);
            let stream_path = path.to_string();
            tokio::spawn(async move {
                loop {
                    match decoder.next_row().await {
                        Ok(Some(row)) => {
                            if tx.send(row).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!("fs9: streaming decode error for {}: {}", stream_path, err);
                            break;
                        }
                    }
                }
            });
            (schema, rx)
        }
        "jsonl" | "ndjson" => {
            let mut decoder = streaming::StreamingJsonlDecoder::new(reader, path.to_string());
            let schema = decoder.schema().clone();
            let (tx, rx) = mpsc::channel(256);
            let stream_path = path.to_string();
            tokio::spawn(async move {
                loop {
                    match decoder.next_row().await {
                        Ok(Some(row)) => {
                            if tx.send(row).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!("fs9: streaming decode error for {}: {}", stream_path, err);
                            break;
                        }
                    }
                }
            });
            (schema, rx)
        }
        _ => {
            let mut decoder = streaming::StreamingTextDecoder::new(reader, path.to_string());
            let schema = decoder.schema().clone();
            let (tx, rx) = mpsc::channel(256);
            let stream_path = path.to_string();
            tokio::spawn(async move {
                loop {
                    match decoder.next_row().await {
                        Ok(Some(row)) => {
                            if tx.send(row).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!("fs9: streaming decode error for {}: {}", stream_path, err);
                            break;
                        }
                    }
                }
            });
            (schema, rx)
        }
    };

    Ok(Some((schema, rx)))
}

#[cfg(test)]
async fn start_file_stream_for_test_backend(
    backend: Box<dyn backend::FsBackend>,
    path: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    start_file_stream_for_backend(backend, path, format, delimiter, header).await
}

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
    let backend = backend::get_backend(tenant).await?;
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
    backend: Box<dyn backend::FsBackend>,
    pattern: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
    exclude: Option<&str>,
    max_total_bytes: usize,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    // Expand glob once — reused for both schema probe and streaming.
    let matching_files = glob::expand_glob(&*backend, pattern, MAX_FILES_PER_GLOB, exclude).await?;
    if matching_files.is_empty() {
        return Ok(None);
    }

    let fmt = decoders::detect_format(&matching_files[0], format);

    // Probe schema from first readable file; skip oversized/unreadable
    // files with a warning instead of failing the entire query.
    let schema = {
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
                    found = Some(s);
                    break;
                }
                Err(err) => {
                    warn!("fs9: skipping {} during schema probe: {}", probe_path, err);
                }
            }
        }
        found.ok_or_else(|| {
            anyhow!(
                "fs9: no readable files found matching pattern '{}'",
                pattern
            )
        })?
    };

    let (tx, rx) = mpsc::channel(256);
    let fmt_owned = fmt.to_string();
    let pattern_owned = pattern.to_string();
    // Move the boxed backend into the spawned task so it can make further requests.
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
async fn start_glob_stream_with_budget_for_test_backend(
    backend: Box<dyn backend::FsBackend>,
    pattern: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
    exclude: Option<&str>,
    max_total_bytes: usize,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
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

async fn list_directory_entries(
    backend: &dyn backend::FsBackend,
    path: &str,
    recursive: bool,
    exclude_set: Option<&globset::GlobSet>,
) -> Result<Vec<backend::FsFileInfo>> {
    if !recursive {
        let mut entries = backend.readdir(path).await?;
        if let Some(exclude_set) = exclude_set {
            entries.retain(|entry| !glob::path_matches_exclude(&entry.path, exclude_set));
        }
        return Ok(entries);
    }

    const MAX_RECURSIVE_DEPTH: usize = 10;
    const MAX_DIR_ENTRIES: usize = 100_000;
    let max_entries = MAX_DIR_ENTRIES;

    let mut entries = Vec::new();
    let mut stack = vec![(path.to_string(), 0usize)];
    let mut visited: HashSet<String> = HashSet::new();

    while let Some((current_dir, depth)) = stack.pop() {
        if entries.len() >= max_entries {
            warn!("fs9: directory listing capped at {} entries", max_entries);
            break;
        }
        if depth > MAX_RECURSIVE_DEPTH {
            continue;
        }
        if !visited.insert(current_dir.clone()) {
            continue;
        }

        let dir_entries = backend.readdir(&current_dir).await?;
        for entry in dir_entries {
            if exclude_set.is_some_and(|set| glob::path_matches_exclude(&entry.path, set)) {
                continue;
            }

            if entries.len() >= max_entries {
                break;
            }

            if entry.is_dir && depth < MAX_RECURSIVE_DEPTH {
                // Avoid following directory symlinks in recursive mode to prevent loops.
                if !entry.is_symlink {
                    stack.push((entry.path.clone(), depth + 1));
                }
            }

            entries.push(entry);
            if entries.len() >= max_entries {
                break;
            }
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::AsyncBufRead;

    use tokio::time::{timeout, Duration};

    use super::{
        execute_table_function, execute_table_function_with_budget_for_test_backend,
        infer_table_function_schema, list_directory_entries, start_file_stream,
        start_file_stream_for_test_backend, start_glob_stream,
        start_glob_stream_with_budget_for_test_backend, Fs9Mode,
    };
    use crate::extensions::fs::backend::{FsBackend, FsFileInfo, FsWriteStream};
    use crate::model::Value;

    struct TestLocalBackend;

    fn to_file_info(path: &str, metadata: std::fs::Metadata) -> Result<FsFileInfo> {
        let is_dir = metadata.is_dir();
        let _is_file = metadata.is_file();
        let mtime = metadata
            .modified()
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            .duration_since(UNIX_EPOCH)
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            .as_secs();

        Ok(FsFileInfo {
            path: path.to_string(),
            is_dir,
            // is_file field removed from FsFileInfo
            is_symlink: false,
            size: metadata.len(),
            mode: if is_dir { 0o755 } else { 0o644 },
            mtime,
            storage: None,
            sealed: None,
        })
    }

    #[async_trait]
    impl FsBackend for TestLocalBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            let metadata = std::fs::metadata(path)
                .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
            to_file_info(path, metadata)
        }

        async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
            let metadata = std::fs::metadata(path)
                .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
            if !metadata.is_dir() {
                return Err(anyhow!("fs9: not a directory: {path}"));
            }

            let mut out = Vec::new();
            for entry in std::fs::read_dir(path)
                .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            {
                let entry = entry.map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
                let entry_path = entry.path().to_string_lossy().to_string();
                let entry_meta = std::fs::metadata(entry.path())
                    .map_err(|err| anyhow!("fs9: cannot stat '{entry_path}': {err}"))?;
                let mut info = to_file_info(&entry_path, entry_meta)?;
                info.is_symlink = Path::new(&entry_path).is_symlink();
                out.push(info);
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        }

        async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
            let data = std::fs::read(path)
                .map_err(|err| anyhow!("fs9: cannot read file '{path}': {err}"))?;
            if data.len() > max_bytes {
                return Err(anyhow!(
                    "fs9: file too large: {} bytes exceeds limit {}",
                    data.len(),
                    max_bytes
                ));
            }
            Ok(data)
        }

        async fn read_file_stream(
            &self,
            path: &str,
            max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            let data = std::fs::read(path)
                .map_err(|err| anyhow!("fs9: cannot read file '{path}': {err}"))?;
            if data.len() > max_bytes {
                return Err(anyhow!(
                    "fs9: file too large: {} bytes exceeds limit {}",
                    data.len(),
                    max_bytes
                ));
            }
            Ok(Box::new(tokio::io::BufReader::new(std::io::Cursor::new(
                data,
            ))))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn mkdir(&self, _path: &str, _recursive: bool) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn write_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: crate::extensions::fs::backend::FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
        ) -> Result<crate::extensions::fs::backend::FsCreateUpload> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
        ) -> Result<crate::extensions::fs::backend::FsPresignedRequest> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<crate::extensions::fs::backend::FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn prepare_download(
            &self,
            _path: &str,
        ) -> Result<crate::extensions::fs::backend::FsPreparedDownload> {
            anyhow::bail!("not implemented for test backend")
        }
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_base(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/db9-fs9-listdir-test-{name}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn infer_table_function_schema_without_context_returns_error() {
        let mode = Fs9Mode::File {
            path: "/tmp/unused.csv".to_string(),
            format: None,
            delimiter: None,
            header: None,
        };
        let err = match infer_table_function_schema("tenant", &mode).await {
            Ok(_) => panic!("expected missing context error"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("fs9: TiKV client not available in extension context"));
    }

    #[tokio::test]
    async fn execute_table_function_without_context_returns_error() {
        let mode = Fs9Mode::File {
            path: "/tmp/unused.csv".to_string(),
            format: None,
            delimiter: None,
            header: None,
        };
        let err = match execute_table_function("tenant", mode).await {
            Ok(_) => panic!("expected missing context error"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("fs9: TiKV client not available in extension context"));
    }

    #[tokio::test]
    async fn start_file_stream_without_context_returns_error() {
        let err = match start_file_stream("tenant", "/tmp/unused.csv", None, None, None).await {
            Ok(_) => panic!("expected missing context error"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("fs9: TiKV client not available in extension context"));
    }

    #[tokio::test]
    async fn start_glob_stream_without_context_returns_error() {
        let err = match start_glob_stream("tenant", "/tmp/*.csv", None, None, None, None).await {
            Ok(_) => panic!("expected missing context error"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("fs9: TiKV client not available in extension context"));
    }

    #[tokio::test]
    async fn recursive_directory_listing_skips_symlink_dirs() {
        #[cfg(not(unix))]
        {
            return;
        }
        #[cfg(unix)]
        use std::os::unix::fs::symlink;

        let dir = unique_base("symlink-loop");
        fs::create_dir_all(dir.join("subdir")).expect("create subdir");
        symlink(&dir, dir.join("loop")).expect("create symlink loop");

        let backend = TestLocalBackend;
        let dir_str = dir.to_string_lossy().to_string();
        let fut = list_directory_entries(&backend, &dir_str, true, None);
        let entries = timeout(Duration::from_secs(1), fut)
            .await
            .expect("list_directory_entries should not hang")
            .expect("list directory entries");

        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        assert!(paths.iter().any(|p| p.ends_with("/loop")));
        assert!(paths.iter().any(|p| p.ends_with("/subdir")));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_glob_stream_multiple_text_files() {
        let dir = unique_base("glob-stream-multi");
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").expect("write a.txt");
        fs::write(dir.join("b.txt"), "gamma\n").expect("write b.txt");
        fs::write(dir.join("c.txt"), "delta\nepsilon\n").expect("write c.txt");

        let pattern = format!("{}/*.txt", dir.display());
        let (schema, mut rx) = start_glob_stream_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            &pattern,
            None,
            None,
            None,
            None,
            super::MAX_TOTAL_BYTES,
        )
        .await
        .expect("start glob stream")
        .expect("expected streaming result");

        assert_eq!(schema.columns[1].name, "line");

        let mut lines = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        assert_eq!(lines, vec!["alpha", "beta", "gamma", "delta", "epsilon"]);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_glob_stream_bytes_budget_stops_following_files() {
        let dir = unique_base("glob-stream-budget");
        fs::write(dir.join("a.txt"), "line1\nline2\nline3\n").expect("write a.txt");
        fs::write(dir.join("b.txt"), "line4\nline5\n").expect("write b.txt");

        let pattern = format!("{}/*.txt", dir.display());
        let budget = "line1\nline2\nline3\n".len();

        let (_schema, mut rx) = start_glob_stream_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            &pattern,
            None,
            None,
            None,
            None,
            budget,
        )
        .await
        .expect("start glob stream")
        .expect("expected streaming result");

        let mut lines = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        assert_eq!(lines, vec!["line1", "line2", "line3"]);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_execute_glob_bytes_budget_does_not_overshoot_last_file() {
        let dir = unique_base("glob-exec-budget");
        fs::write(dir.join("a.txt"), "line1\n").expect("write a.txt");
        fs::write(dir.join("b.txt"), "line2\nline3\n").expect("write b.txt");

        let pattern = format!("{}/*.txt", dir.display());
        let budget = "line1\n".len() + 2;
        let mode = super::Fs9Mode::Glob {
            pattern,
            format: None,
            delimiter: None,
            header: None,
            exclude: None,
        };

        let (_schema, rows) = execute_table_function_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            mode,
            budget,
        )
        .await
        .expect("execute table function");

        let mut lines = Vec::new();
        for row in rows {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        assert_eq!(lines, vec!["line1"]);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_execute_glob_oversized_file_returns_error() {
        let dir = unique_base("glob-exec-oversize");
        fs::write(dir.join("a.txt"), "line1\n").expect("write a.txt");

        let oversized_path = dir.join("b_big.txt");
        let oversized = std::fs::File::create(&oversized_path).expect("create b_big.txt");
        oversized
            .set_len((super::MAX_BYTES_PER_FILE + 1) as u64)
            .expect("set b_big.txt size");

        let pattern = format!("{}/*.txt", dir.display());
        let mode = super::Fs9Mode::Glob {
            pattern,
            format: None,
            delimiter: None,
            header: None,
            exclude: None,
        };

        let err = execute_table_function_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            mode,
            super::MAX_TOTAL_BYTES,
        )
        .await
        .expect_err("expected oversized file to return an error");

        let msg = err.to_string();
        assert!(
            msg.contains("file too large"),
            "unexpected error message: {msg}"
        );

        cleanup(&dir);
    }

    /// P1-1 regression: streaming glob loop must cap each file open to the
    /// remaining budget, not the fixed MAX_BYTES_PER_FILE.
    #[tokio::test]
    async fn test_glob_stream_remaining_budget_caps_per_file() {
        let dir = unique_base("glob-stream-budget-cap");
        // a.txt = 6 bytes, b.txt = 6 bytes
        fs::write(dir.join("a.txt"), "alpha\n").expect("write a.txt");
        fs::write(dir.join("b.txt"), "bravo\n").expect("write b.txt");

        let pattern = format!("{}/*.txt", dir.display());
        // Budget of 8 bytes: enough for a.txt (6), but remaining 2 < b.txt (6).
        let budget = 8;

        let (_schema, mut rx) = start_glob_stream_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            &pattern,
            None,
            None,
            None,
            None,
            budget,
        )
        .await
        .expect("start glob stream")
        .expect("expected streaming result");

        let mut lines = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        // b.txt exceeds remaining budget (2 bytes) and must be skipped.
        assert_eq!(lines, vec!["alpha"]);

        cleanup(&dir);
    }

    /// P1-2 regression: oversized first file in schema probe must be skipped
    /// (not hard-error), and the query should fall through to the next file.
    #[tokio::test]
    async fn test_glob_stream_oversized_first_file_skips_to_next() {
        let dir = unique_base("glob-stream-oversize-probe");

        // Oversized file sorts first alphabetically.
        let big_path = dir.join("a_big.txt");
        let big_file = std::fs::File::create(&big_path).expect("create a_big.txt");
        big_file
            .set_len((super::MAX_BYTES_PER_FILE + 1) as u64)
            .expect("set a_big.txt size");

        fs::write(dir.join("b.txt"), "bravo\n").expect("write b.txt");

        let pattern = format!("{}/*.txt", dir.display());
        let (_schema, mut rx) = start_glob_stream_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            &pattern,
            None,
            None,
            None,
            None,
            super::MAX_TOTAL_BYTES,
        )
        .await
        .expect("schema probe should skip oversized file")
        .expect("expected streaming result");

        let mut lines = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        // a_big.txt skipped; only b.txt content present.
        assert_eq!(lines, vec!["bravo"]);

        cleanup(&dir);
    }

    /// P1-2 regression: oversized file in a later position must be skipped
    /// with a warning (consistent with schema-probe behavior).
    #[tokio::test]
    async fn test_glob_stream_oversized_later_file_skipped() {
        let dir = unique_base("glob-stream-oversize-later");

        fs::write(dir.join("a.txt"), "alpha\n").expect("write a.txt");

        let big_path = dir.join("b_big.txt");
        let big_file = std::fs::File::create(&big_path).expect("create b_big.txt");
        big_file
            .set_len((super::MAX_BYTES_PER_FILE + 1) as u64)
            .expect("set b_big.txt size");

        fs::write(dir.join("c.txt"), "charlie\n").expect("write c.txt");

        let pattern = format!("{}/*.txt", dir.display());
        let (_schema, mut rx) = start_glob_stream_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            &pattern,
            None,
            None,
            None,
            None,
            super::MAX_TOTAL_BYTES,
        )
        .await
        .expect("start glob stream")
        .expect("expected streaming result");

        let mut lines = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        // b_big.txt skipped; a.txt and c.txt present.
        assert_eq!(lines, vec!["alpha", "charlie"]);

        cleanup(&dir);
    }

    /// Regression: malformed CSV files that fail during StreamingCsvDecoder::new
    /// must still charge their bytes against the total budget so that repeated
    /// malformed files cannot bypass the budget limit.
    #[tokio::test]
    async fn test_malformed_csv_charges_bytes_against_budget() {
        let dir = unique_base("malformed-csv-budget");

        // a.csv: valid CSV (schema probe succeeds on this file). 12 bytes.
        let valid = b"col1\nvalue1\n";
        assert_eq!(valid.len(), 12);
        fs::write(dir.join("a.csv"), valid).expect("write a.csv");

        // b.csv, c.csv: malformed CSV (invalid UTF-8 → csv headers() error).
        // Each file is 10 bytes.
        let bad: &[u8] = b"\xff\xfe\xff\xfe\xff\xfe\xff\xfe\xff\xfe";
        assert_eq!(bad.len(), 10);
        fs::write(dir.join("b.csv"), bad).expect("write b.csv");
        fs::write(dir.join("c.csv"), bad).expect("write c.csv");

        // d.csv: valid CSV that should NOT be reached if budget is enforced.
        fs::write(dir.join("d.csv"), b"col1\nextra\n").expect("write d.csv");

        let pattern = format!("{}/*.csv", dir.display());
        // Budget 30: a.csv charges 12 (success), b.csv fails but charges
        // file_limit (remaining 18), total reaches 30 = budget. Loop breaks
        // before c.csv/d.csv.
        let budget: usize = 30;

        let (_schema, mut rx) = start_glob_stream_with_budget_for_test_backend(
            Box::new(TestLocalBackend),
            &pattern,
            Some("csv"),
            None,
            None,
            None,
            budget,
        )
        .await
        .expect("start glob stream")
        .expect("expected streaming result");

        let mut rows = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(v) = &row.values[1] {
                rows.push(v.clone());
            }
        }

        // Only a.csv's data row should appear; b.csv and c.csv are malformed
        // (no rows) but charge bytes, exhausting the budget before d.csv.
        assert_eq!(
            rows,
            vec!["value1"],
            "malformed CSV must charge bytes against budget; d.csv should be skipped"
        );

        cleanup(&dir);
    }

    /// #1418 regression: start_file_stream must reject files exceeding
    /// MAX_BYTES_PER_FILE (the cap enforced by PR #1412 in the non-glob path).
    #[tokio::test]
    async fn test_file_stream_rejects_oversized_file() {
        let dir = unique_base("file-stream-oversize");
        let big_path = dir.join("big.txt");
        let big_file = std::fs::File::create(&big_path).expect("create big.txt");
        big_file
            .set_len((super::MAX_BYTES_PER_FILE + 1) as u64)
            .expect("set big.txt size");

        let path_str = big_path.to_string_lossy().to_string();
        let result = start_file_stream_for_test_backend(
            Box::new(TestLocalBackend),
            &path_str,
            None,
            None,
            None,
        )
        .await;

        assert!(result.is_err(), "oversized file must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("file too large"),
            "expected 'file too large' error, got: {err_msg}"
        );

        cleanup(&dir);
    }

    /// #1418 regression: start_file_stream must succeed for files within
    /// MAX_BYTES_PER_FILE and stream their content correctly.
    #[tokio::test]
    async fn test_file_stream_accepts_normal_file() {
        let dir = unique_base("file-stream-normal");
        let file_path = dir.join("hello.txt");
        fs::write(&file_path, "hello\nworld\n").expect("write hello.txt");

        let path_str = file_path.to_string_lossy().to_string();
        let (_schema, mut rx) = start_file_stream_for_test_backend(
            Box::new(TestLocalBackend),
            &path_str,
            None,
            None,
            None,
        )
        .await
        .expect("start file stream")
        .expect("expected streaming result");

        let mut lines = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        assert_eq!(lines, vec!["hello", "world"]);

        cleanup(&dir);
    }
}
