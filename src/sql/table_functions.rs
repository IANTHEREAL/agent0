use sqlparser::ast::{FunctionArg, FunctionArgExpr, ObjectName};

use crate::sql::expr::bridge::eval_const_ast_expr;
use crate::sql::names;
use crate::types::{TableSchema, Value};

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
        "_PGTIKV_SYS_OBSERVABILITY" => Some("_PGTIKV_SYS_OBSERVABILITY"),
        "_PGTIKV_SYS_QUERY_SAMPLES" => Some("_PGTIKV_SYS_QUERY_SAMPLES"),
        "_PGTIKV_SYS_EXPORT_DDL" => Some("_PGTIKV_SYS_EXPORT_DDL"),
        "_PGTIKV_SYS_MIGRATIONS" => Some("_PGTIKV_SYS_MIGRATIONS"),
        "_PGTIKV_SYS_TRIGGER_QUEUE_STATS" => Some("_PGTIKV_SYS_TRIGGER_QUEUE_STATS"),
        "_PGTIKV_SYS_TRIGGER_DLQ" => Some("_PGTIKV_SYS_TRIGGER_DLQ"),
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
    if !args.is_empty() {
        return None;
    }
    let base = name.0.last().map(names::normalize_ident)?;
    let canonical = canonical_system_virtual_table_function(&base)?;
    crate::sql::catalog::virtual_tables::virtual_table_schema(canonical)
}

pub(crate) async fn infer_extension_table_function_schema(
    search_path: &[String],
    schema_opt: Option<&str>,
    func_name: &str,
    args: &[FunctionArg],
    is_superuser: bool,
) -> Option<TableSchema> {
    let in_extensions_schema = match schema_opt {
        Some(schema) => schema.eq_ignore_ascii_case(crate::extensions::EXTENSIONS_SCHEMA),
        None => search_path
            .iter()
            .any(|s| s.eq_ignore_ascii_case(crate::extensions::EXTENSIONS_SCHEMA)),
    };
    if !in_extensions_schema {
        return None;
    }

    if let Some(schema) = crate::extensions::http::table_function_schema(func_name) {
        return Some(schema);
    }

    if func_name.eq_ignore_ascii_case("fs9") {
        return infer_fs9_table_function_schema(args, is_superuser).await;
    }

    if func_name.eq_ignore_ascii_case("fs9_events") {
        return crate::extensions::fs::table_function_schema("fs9_events");
    }

    crate::extensions::fs::table_function_schema(func_name)
}

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

pub(crate) async fn infer_fs9_table_function_schema(
    args: &[FunctionArg],
    is_superuser: bool,
) -> Option<TableSchema> {
    let fallback = crate::extensions::fs::table_function_schema("fs9")?;
    if !is_superuser {
        return Some(fallback);
    }

    let mode = match try_parse_fs9_mode_from_args(args) {
        Some(mode) => mode,
        None => return Some(fallback),
    };

    use crate::extensions::fs::backend::FsBackend;
    let backend = crate::extensions::fs::backend::local_backend();

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
                        Err(_) => return Some(fallback),
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
                backend,
                &pattern,
                crate::extensions::fs::MAX_FILES_PER_GLOB,
                exclude.as_deref(),
            )
            .await
            {
                Ok(files) => files,
                Err(_) => return Some(fallback),
            };

            if files.is_empty() {
                return Some(
                    crate::extensions::fs::decoders::decode_raw_text(&[], &pattern, 0).schema,
                );
            }

            let first = files.first().cloned().unwrap_or_default();
            if first.is_empty() {
                return Some(fallback);
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
                        Err(_) => return Some(fallback),
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
