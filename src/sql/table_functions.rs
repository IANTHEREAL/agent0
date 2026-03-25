use sqlparser::ast::{FunctionArg, FunctionArgExpr, ObjectName};

use crate::model::TableSchema;
use crate::sql::names;

#[cfg(test)]
use crate::model::Value;
#[cfg(test)]
use crate::sql::expr::bridge::eval_const_ast_expr;
#[cfg(test)]
use anyhow::{anyhow, Result as AnyResult};
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use std::time::UNIX_EPOCH;
#[cfg(test)]
use tokio::io::AsyncBufRead;

#[cfg(test)]
fn basic_fs9_schema() -> TableSchema {
    use crate::model::{ColumnDef, DataType};
    TableSchema::virtual_table(
        "fs9",
        vec![
            ColumnDef::new("_line_number", DataType::Int64, false),
            ColumnDef::new("line", DataType::Text, false),
            ColumnDef::new("_path", DataType::Text, false),
        ],
    )
}

/// Build a stable signature key for a table-valued function call in FROM.
///
/// The Analyzer is synchronous, so any dynamic table-function schemas must be
/// pre-fetched asynchronously. This key is used as the lookup handle between
/// the prefetch phase and the Analyzer.
///
/// Notes:
/// - Identifiers are normalized with `names::normalize_ident` (PostgreSQL-like
///   case-folding for unquoted idents).
/// - Expression strings are trimmed and concatenated without extra whitespace
///   to keep the key stable.
pub(crate) fn table_function_key(name: &ObjectName, args: &[FunctionArg]) -> String {
    let parts: Vec<String> = name.0.iter().map(names::normalize_ident).collect();
    let full_name = parts.join(".");

    let mut rendered_args = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                rendered_args.push(format!("{}", e).trim().to_string());
            }
            FunctionArg::Named { name, arg, .. } => {
                let param = names::normalize_ident(name);
                let expr = match arg {
                    FunctionArgExpr::Expr(e) => format!("{}", e).trim().to_string(),
                    other => format!("{}", other).trim().to_string(),
                };
                rendered_args.push(format!("{param}=>{expr}"));
            }
            other => {
                rendered_args.push(format!("{}", other).trim().to_string());
            }
        }
    }

    format!("{}({})", full_name, rendered_args.join(","))
}

fn canonical_system_virtual_table_function(func_name: &str) -> Option<&'static str> {
    match func_name.to_ascii_uppercase().as_str() {
        "_DB9_SYS_OBSERVABILITY" => Some("_DB9_SYS_OBSERVABILITY"),
        "_DB9_SYS_QUERY_SAMPLES" => Some("_DB9_SYS_QUERY_SAMPLES"),
        "_DB9_SYS_EXPORT_DDL" => Some("_DB9_SYS_EXPORT_DDL"),
        "_DB9_SYS_MIGRATIONS" => Some("_DB9_SYS_MIGRATIONS"),
        "_DB9_SYS_RECORD_MIGRATION" => Some("_DB9_SYS_RECORD_MIGRATION"),
        "_DB9_SYS_TRIGGER_QUEUE_STATS" => Some("_DB9_SYS_TRIGGER_QUEUE_STATS"),
        "_DB9_SYS_TRIGGER_DLQ" => Some("_DB9_SYS_TRIGGER_DLQ"),
        _ => None,
    }
}

pub(crate) fn is_virtual_table_backed_system_function(name: &str) -> bool {
    let base = name.rsplit('.').next().unwrap_or(name);
    canonical_system_virtual_table_function(base).is_some()
}

pub(crate) fn infer_system_virtual_table_function_schema(
    name: &ObjectName,
    args: &[FunctionArg],
) -> Option<TableSchema> {
    let base = name.0.last().map(names::normalize_ident)?;
    let canonical = canonical_system_virtual_table_function(&base)?;

    // Most virtual-table-backed system functions are zero-arg.
    // _DB9_SYS_RECORD_MIGRATION is the only supported arg-taking variant.
    if canonical == "_DB9_SYS_RECORD_MIGRATION" {
        if args.len() != 3 {
            return None;
        }
    } else if !args.is_empty() {
        return None;
    }

    crate::sql::catalog::virtual_tables::virtual_table_schema(canonical)
}

#[cfg(test)]
fn try_parse_fs9_mode_from_args(args: &[FunctionArg]) -> Option<crate::extensions::fs::Fs9Mode> {
    if args.is_empty() {
        return None;
    }

    let path_expr = match &args[0] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => e,
        _ => return None,
    };
    let path = match eval_const_ast_expr(path_expr).ok()? {
        Value::Text(s) => s,
        _ => return None,
    };

    let mut format: Option<String> = None;
    let mut delimiter: Option<char> = None;
    let mut header: Option<bool> = None;
    let mut recursive: Option<bool> = None;
    let mut exclude: Option<String> = None;

    for arg in &args[1..] {
        match arg {
            FunctionArg::Named {
                name,
                arg: FunctionArgExpr::Expr(e),
                ..
            } => {
                let param_name = name.value.to_ascii_lowercase();
                let val = eval_const_ast_expr(e).ok()?;
                match param_name.as_str() {
                    "format" => match val {
                        Value::Text(s) => format = Some(s),
                        _ => return None,
                    },
                    "delimiter" => match val {
                        Value::Text(s) => {
                            if s.chars().count() != 1 {
                                return None;
                            }
                            delimiter = s.chars().next();
                        }
                        _ => return None,
                    },
                    "header" => match val {
                        Value::Boolean(b) => header = Some(b),
                        Value::Text(s) if s.eq_ignore_ascii_case("true") => header = Some(true),
                        Value::Text(s) if s.eq_ignore_ascii_case("false") => header = Some(false),
                        _ => return None,
                    },
                    "recursive" => match val {
                        Value::Boolean(b) => recursive = Some(b),
                        Value::Text(s) if s.eq_ignore_ascii_case("true") => recursive = Some(true),
                        Value::Text(s) if s.eq_ignore_ascii_case("false") => {
                            recursive = Some(false)
                        }
                        _ => return None,
                    },
                    "exclude" => match val {
                        Value::Text(s) => exclude = Some(s),
                        _ => return None,
                    },
                    _ => return None,
                }
            }
            _ => return None,
        }
    }

    let mode = if path.ends_with('/') {
        crate::extensions::fs::Fs9Mode::Directory {
            path,
            recursive: recursive.unwrap_or(false),
            exclude,
        }
    } else if crate::extensions::fs::glob::is_glob_pattern(&path) {
        crate::extensions::fs::Fs9Mode::Glob {
            pattern: path,
            format,
            delimiter,
            header,
            exclude,
        }
    } else {
        crate::extensions::fs::Fs9Mode::File {
            path,
            format,
            delimiter,
            header,
        }
    };

    Some(mode)
}

#[cfg(test)]
pub(crate) async fn infer_fs9_table_function_schema(
    args: &[FunctionArg],
    is_superuser: bool,
) -> Option<TableSchema> {
    if !is_superuser {
        return Some(basic_fs9_schema());
    }

    let mode = match try_parse_fs9_mode_from_args(args) {
        Some(mode) => mode,
        None => return Some(basic_fs9_schema()),
    };

    struct TestLocalBackend;

    fn to_file_info(
        path: &str,
        metadata: std::fs::Metadata,
    ) -> AnyResult<crate::extensions::fs::backend::FsFileInfo> {
        let is_dir = metadata.is_dir();
        let _is_file = metadata.is_file();
        let mtime = metadata
            .modified()
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            .duration_since(UNIX_EPOCH)
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
            .as_secs();

        Ok(crate::extensions::fs::backend::FsFileInfo {
            path: path.to_string(),
            is_dir,
            // is_file field removed from FsFileInfo
            is_symlink: false,
            size: metadata.len(),
            mode: if is_dir { 0o755 } else { 0o644 },
            generation: 0,
            mtime,
            storage: None,
            sealed: None,
        })
    }

    #[async_trait]
    impl crate::extensions::fs::backend::FsBackend for TestLocalBackend {
        async fn stat(&self, path: &str) -> AnyResult<crate::extensions::fs::backend::FsFileInfo> {
            let metadata = std::fs::metadata(path)
                .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
            to_file_info(path, metadata)
        }

        async fn readdir(
            &self,
            path: &str,
        ) -> AnyResult<Vec<crate::extensions::fs::backend::FsFileInfo>> {
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
                info.is_symlink = entry.path().is_symlink();
                out.push(info);
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        }

        async fn read_file(&self, path: &str, max_bytes: usize) -> AnyResult<Vec<u8>> {
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
            _path: &str,
            _max_bytes: usize,
        ) -> AnyResult<Box<dyn AsyncBufRead + Unpin + Send>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn remove(&self, _path: &str) -> AnyResult<()> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn remove_recursive(&self, _path: &str) -> AnyResult<u64> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> AnyResult<()> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn write_file(
            &self,
            _path: &str,
            _data: &[u8],
            _mode: Option<u32>,
        ) -> AnyResult<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: crate::extensions::fs::backend::FsWriteStreamOptions,
        ) -> AnyResult<Box<dyn crate::extensions::fs::backend::FsWriteStream>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn read_file_at(
            &self,
            _path: &str,
            _offset: u64,
            _length: usize,
        ) -> AnyResult<Vec<u8>> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> AnyResult<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> AnyResult<usize> {
            anyhow::bail!("not implemented for test backend")
        }

        async fn truncate(&self, _path: &str, _size: u64) -> AnyResult<()> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
        ) -> AnyResult<crate::extensions::fs::backend::FsCreateUpload> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
        ) -> AnyResult<crate::extensions::fs::backend::FsPresignedRequest> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<crate::extensions::fs::backend::FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> AnyResult<usize> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn abort_upload(&self, _upload_token: &str) -> AnyResult<()> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn prepare_download(
            &self,
            _path: &str,
        ) -> AnyResult<crate::extensions::fs::backend::FsPreparedDownload> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn rename(&self, _old_path: &str, _new_path: &str) -> AnyResult<()> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn symlink(&self, _path: &str, _target: &str) -> AnyResult<()> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn readlink(&self, _path: &str) -> AnyResult<String> {
            anyhow::bail!("not implemented for test backend")
        }
        async fn chmod(&self, _path: &str, _mode: u32) -> AnyResult<()> {
            anyhow::bail!("not implemented for test backend")
        }
    }

    let backend: Box<dyn crate::extensions::fs::backend::FsBackend> = Box::new(TestLocalBackend);

    match mode {
        crate::extensions::fs::Fs9Mode::Directory { .. } => {
            Some(crate::extensions::fs::decoders::decode_directory(Vec::new()).schema)
        }
        crate::extensions::fs::Fs9Mode::File {
            path,
            format,
            delimiter,
            header,
        } => {
            if backend
                .stat(&path)
                .await
                .ok()
                .is_some_and(|info| info.is_dir)
            {
                return Some(crate::extensions::fs::decoders::decode_directory(Vec::new()).schema);
            }

            let fmt = crate::extensions::fs::decoders::detect_format(&path, format.as_deref());
            match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        delimiter
                    };
                    let data = match backend
                        .read_file(&path, crate::extensions::fs::MAX_BYTES_PER_FILE)
                        .await
                    {
                        Ok(data) => data,
                        Err(_) => return Some(basic_fs9_schema()),
                    };
                    let decoded =
                        crate::extensions::fs::decoders::decode_csv(&data, &path, delim, header, 0)
                            .ok()?;
                    Some(decoded.schema)
                }
                "jsonl" | "ndjson" => {
                    Some(crate::extensions::fs::decoders::decode_jsonl(&[], &path, 0).schema)
                }
                _ => Some(crate::extensions::fs::decoders::decode_raw_text(&[], &path, 0).schema),
            }
        }
        crate::extensions::fs::Fs9Mode::Glob {
            pattern,
            format,
            delimiter,
            header,
            exclude,
        } => {
            let files = match crate::extensions::fs::glob::expand_glob(
                &*backend,
                &pattern,
                crate::extensions::fs::MAX_FILES_PER_GLOB,
                exclude.as_deref(),
            )
            .await
            {
                Ok(files) => files,
                Err(_) => return Some(basic_fs9_schema()),
            };

            if files.is_empty() {
                return Some(
                    crate::extensions::fs::decoders::decode_raw_text(&[], &pattern, 0).schema,
                );
            }

            let first = files.first().cloned().unwrap_or_default();
            if first.is_empty() {
                return Some(basic_fs9_schema());
            }

            let fmt = crate::extensions::fs::decoders::detect_format(&first, format.as_deref());
            match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        delimiter
                    };
                    let data = match backend
                        .read_file(&first, crate::extensions::fs::MAX_BYTES_PER_FILE)
                        .await
                    {
                        Ok(data) => data,
                        Err(_) => return Some(basic_fs9_schema()),
                    };
                    let decoded = crate::extensions::fs::decoders::decode_csv(
                        &data, &first, delim, header, 0,
                    )
                    .ok()?;
                    Some(decoded.schema)
                }
                "jsonl" | "ndjson" => {
                    Some(crate::extensions::fs::decoders::decode_jsonl(&[], &first, 0).schema)
                }
                _ => Some(crate::extensions::fs::decoders::decode_raw_text(&[], &first, 0).schema),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::{Expr, Ident, Value as AstValue};

    fn obj(name: &str) -> ObjectName {
        ObjectName(vec![Ident::new(name)])
    }

    fn str_arg(s: &str) -> FunctionArg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
            AstValue::SingleQuotedString(s.to_string()),
        )))
    }

    #[test]
    fn record_migration_schema_is_available_with_three_args() {
        let schema = infer_system_virtual_table_function_schema(
            &obj("_db9_sys_record_migration"),
            &[str_arg("n"), str_arg("c"), str_arg("p")],
        )
        .expect("record migration schema");
        assert_eq!(schema.columns.len(), 3);
        assert_eq!(schema.columns[0].name, "name");
        assert_eq!(schema.columns[1].name, "applied_at");
        assert_eq!(schema.columns[2].name, "status");
    }

    #[test]
    fn record_migration_schema_requires_three_args() {
        assert!(infer_system_virtual_table_function_schema(
            &obj("_db9_sys_record_migration"),
            &[str_arg("n"), str_arg("c")],
        )
        .is_none());
    }
}
