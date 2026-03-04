use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::model::{Row, TableSchema};
use std::collections::HashSet;
use tracing::warn;

pub(crate) mod backend;
pub(crate) mod decoders;
pub(crate) mod embedded;
pub(crate) mod glob;
pub(crate) mod streaming;
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
    let backend = backend::get_backend(tenant).await;
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
    let backend = backend::get_backend(tenant).await;
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
    let backend = backend::get_backend(tenant).await;
    let info = backend.stat(path).await?;
    if info.is_dir {
        return Ok(None);
    }

    let fmt = decoders::detect_format(path, format);
    let reader = backend.read_file_stream(path, usize::MAX).await?;

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
                    .await?;
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
        #[cfg(feature = "parquet")]
        "parquet" => {
            let data = backend
                .read_file(
                    path,
                    crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES,
                )
                .await?;
            let reader = crate::extensions::parquet::fs9_reader::Fs9ParquetReader::new(
                bytes::Bytes::from(data),
            );
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
            (parquet_schema, rx)
        }
        #[cfg(not(feature = "parquet"))]
        "parquet" => {
            return Err(anyhow!("fs9: parquet format requires the parquet extension (compile with --features parquet)"));
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
    let backend = backend::get_backend(tenant).await;
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
    let first_path = match glob::find_first_match(&*backend, pattern, exclude).await? {
        Some(p) => p,
        None => return Ok(None),
    };

    let fmt = decoders::detect_format(&first_path, format);

    let schema = match fmt {
        "csv" | "tsv" => {
            let delim = if fmt == "tsv" && delimiter.is_none() {
                Some('\t')
            } else {
                delimiter
            };
            let has_headers = header.unwrap_or(true);
            let reader = backend.read_file_stream(&first_path, usize::MAX).await?;
            let decoder =
                streaming::StreamingCsvDecoder::new(reader, first_path.clone(), delim, has_headers)
                    .await?;
            decoder.schema().clone()
        }
        "jsonl" | "ndjson" => {
            let reader = backend.read_file_stream(&first_path, usize::MAX).await?;
            let decoder = streaming::StreamingJsonlDecoder::new(reader, first_path.clone());
            decoder.schema().clone()
        }
        _ => {
            let reader = backend.read_file_stream(&first_path, usize::MAX).await?;
            let decoder = streaming::StreamingTextDecoder::new(reader, first_path.clone());
            decoder.schema().clone()
        }
    };

    let (tx, rx) = mpsc::channel(256);
    let fmt_owned = fmt.to_string();
    let pattern_owned = pattern.to_string();
    let exclude_owned = exclude.map(|s| s.to_string());
    // Move the boxed backend into the spawned task so it can make further requests.
    tokio::spawn(async move {
        let matching_files = match glob::expand_glob(
            &*backend,
            &pattern_owned,
            MAX_FILES_PER_GLOB,
            exclude_owned.as_deref(),
        )
        .await
        {
            Ok(files) => files,
            Err(err) => {
                warn!("fs9: glob expansion error for {}: {}", pattern_owned, err);
                return;
            }
        };

        let mut total_bytes: usize = 0;
        let mut files_read_count: usize = 0;

        for file_path in matching_files {
            if total_bytes >= max_total_bytes {
                warn!(
                    "fs9: bytes budget exhausted ({} MB), {} files were streamed for pattern {}",
                    total_bytes / (1024 * 1024),
                    files_read_count,
                    pattern_owned
                );
                break;
            }

            let reader = match backend.read_file_stream(&file_path, usize::MAX).await {
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
                        Err(err) => {
                            warn!("fs9: streaming decode error for {}: {}", file_path, err);
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
        execute_table_function_with_budget_for_test_backend, list_directory_entries,
        start_glob_stream_with_budget_for_test_backend,
    };
    use crate::extensions::fs::backend::{FsBackend, FsFileInfo};
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
}
