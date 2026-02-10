use anyhow::{anyhow, Result};

use crate::extensions::context;
use crate::types::{ColumnDef, DataType, Row, TableSchema};
use std::collections::HashSet;
use tracing::warn;

pub(crate) mod backend;
pub(crate) mod decoders;
pub(crate) mod glob;

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

pub(crate) async fn execute_table_function(
    _tenant: &str,
    mode: Fs9Mode,
) -> Result<(TableSchema, Vec<Row>)> {
    if !context::allow_local_fs() {
        return Err(anyhow!("permission denied for extension \"fs9\""));
    }

    use backend::FsBackend;

    let backend = backend::local_backend();

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

    use super::{backend, list_directory_entries};

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
}
