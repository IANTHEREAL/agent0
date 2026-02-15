use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::extensions::context;
use crate::types::{ColumnDef, DataType, Row, TableSchema};
use std::collections::HashSet;
use tracing::warn;

pub(crate) mod backend;
pub(crate) mod decoders;
pub(crate) mod glob;
pub(crate) mod streaming;

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

pub(crate) const MAX_BYTES_PER_FILE: usize = 10 * 1024 * 1024;
pub(crate) const MAX_FILES_PER_GLOB: usize = 10_000;
pub(crate) const MAX_TOTAL_BYTES: usize = 100 * 1024 * 1024;

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
        from_alias: None,
    }
}

pub(crate) fn table_function_schema(func_name: &str) -> Option<TableSchema> {
    let name = func_name.trim().to_ascii_lowercase();
    match name.as_str() {
        "fs9" => Some(fs9_file_schema(&name)),
        _ => None,
    }
}

/// Infer the output schema for an `fs9(...)` table function call without decoding all rows.
///
/// This is used by the Analyzer prefetch phase so it can resolve column names/types
/// for dynamic schemas (directory listing vs file vs csv headers) while keeping
/// analysis itself synchronous.
pub(crate) async fn infer_table_function_schema(
    tenant: &str,
    mode: &Fs9Mode,
) -> Result<TableSchema> {
    let use_remote = backend::is_remote_configured();
    if !use_remote && !context::allow_local_fs() {
        return Err(anyhow!("permission denied for extension \"fs9\""));
    }

    let backend = backend::get_backend(tenant);
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
                glob::expand_glob(&*backend, pattern, MAX_FILES_PER_GLOB, exclude.as_deref())
                    .await?;

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
    let use_remote = backend::is_remote_configured();
    if !use_remote && !context::allow_local_fs() {
        return Err(anyhow!("permission denied for extension \"fs9\""));
    }

    let backend = backend::get_backend(tenant);
    let backend = backend.as_ref();

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
            let mut files_read_count: usize = 0;

            for file_path in &matching_files {
                if total_bytes_read >= MAX_TOTAL_BYTES {
                    warn!(
                        "fs9: bytes budget exhausted ({} MB), {} of {} matched files were scanned",
                        total_bytes_read / (1024 * 1024),
                        files_read_count,
                        matching_files.len()
                    );
                    break;
                }

                let data = backend.read_file(file_path, MAX_BYTES_PER_FILE).await?;
                total_bytes_read = total_bytes_read.saturating_add(data.len());
                files_read_count += 1;

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

pub(crate) async fn start_file_stream(
    tenant: &str,
    path: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    let use_remote = backend::is_remote_configured();
    if !use_remote && !context::allow_local_fs() {
        return Err(anyhow!("permission denied for extension \"fs9\""));
    }

    let backend = backend::get_backend(tenant);
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
    let use_remote = backend::is_remote_configured();
    if !use_remote && !context::allow_local_fs() {
        return Err(anyhow!("permission denied for extension \"fs9\""));
    }

    let backend = backend::get_backend(tenant);

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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::time::{timeout, Duration};

    use super::{
        backend, list_directory_entries, start_glob_stream, start_glob_stream_with_budget,
    };
    use crate::extensions::context;
    use crate::types::Value;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_base(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/pgtikv-fs9-listdir-test-{name}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_directory_listing_skips_symlink_dirs() {
        use std::os::unix::fs::symlink;

        let dir = unique_base("symlink-loop");
        fs::create_dir_all(dir.join("subdir")).expect("create subdir");
        symlink(&dir, dir.join("loop")).expect("create symlink loop");

        let backend = backend::LocalFsBackend::new();
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
        let (schema, mut rx) = context::with_context(true, async {
            start_glob_stream("", &pattern, None, None, None, None)
                .await
                .expect("start glob stream")
                .expect("expected streaming result")
        })
        .await;

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

        let (_schema, mut rx) = context::with_context(true, async {
            start_glob_stream_with_budget("", &pattern, None, None, None, None, budget)
                .await
                .expect("start glob stream")
                .expect("expected streaming result")
        })
        .await;

        let mut lines = Vec::new();
        while let Some(row) = rx.recv().await {
            if let Value::Text(line) = &row.values[1] {
                lines.push(line.clone());
            }
        }

        assert_eq!(lines, vec!["line1", "line2", "line3"]);

        cleanup(&dir);
    }
}
