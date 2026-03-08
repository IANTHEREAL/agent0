use crate::extensions::context;
use crate::extensions::embedding::{
    call_embedding_api, check_embedding_installed, current_embedding_runtime_setting,
    embedding_function_not_found, record_embedding_tokens, resolve_embedding_config,
    ResolvedEmbeddingConfig,
};
use crate::model::Value;
use crate::session_context::current_database_id;
use crate::sql::embedding_options::parse_embed_text_json_options_dimensions;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("EMBEDDING", embedding_fn);
    map.insert("EMBED_TEXT", embed_text_fn);
}

fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

fn ensure_embedding_installed_gate(function_signature: &str) -> Result<()> {
    match crate::session_context::extension_txn_status("embedding") {
        Some(true) => Ok(()),
        Some(false) => Err(embedding_function_not_found(function_signature)),
        None => {
            let client = context::tikv_client()
                .ok_or_else(|| anyhow!("embedding: tikv client not available"))?;
            let db_id = current_database_id();
            run_async(check_embedding_installed(
                &client,
                db_id,
                function_signature,
            ))
        }
    }
}

fn embedding_permission_denied(function_name: &str) -> anyhow::Error {
    SqlError::PermissionDenied {
        object_type: "function".into(),
        object_name: function_name.to_string(),
    }
    .into()
}

pub(crate) fn require_direct_embedding_superuser(function_name: &str) -> Result<()> {
    if context::is_superuser() {
        return Ok(());
    }
    Err(embedding_permission_denied(function_name))
}

fn require_embedding_execution_privilege(function_name: &str) -> Result<()> {
    if context::is_superuser() || context::is_embedding_authorized() {
        return Ok(());
    }
    Err(embedding_permission_denied(function_name))
}

fn consume_embedding_call_budget() -> Result<()> {
    let max_calls = current_embedding_runtime_setting("embedding.max_calls")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(100);
    if max_calls == 100 {
        context::try_consume_embedding_call()
    } else {
        context::try_consume_embedding_call_with_limit(max_calls)
    }
}

fn execute_embedding_call(
    function_name: &str,
    text: &str,
    config: &ResolvedEmbeddingConfig,
) -> Result<Vec<f64>> {
    consume_embedding_call_budget()?;

    let (vector, tokens_used) = run_async(call_embedding_api(config, text))?;

    if vector.len() != config.dimensions as usize {
        return Err(anyhow!(
            "{}: API returned {} dimensions, expected {}",
            function_name,
            vector.len(),
            config.dimensions
        ));
    }

    if tokens_used > 0 {
        let client = context::tikv_client()
            .ok_or_else(|| anyhow!("{function_name}: tikv client not available"))?;
        let db_id = current_database_id();
        if let Err(e) = run_async(record_embedding_tokens(&client, db_id, tokens_used)) {
            tracing::warn!("{function_name}: failed to record token usage: {}", e);
        }
    }

    Ok(vector)
}

pub(crate) fn embedding_call_internal(
    function_name: &str,
    function_signature: &str,
    text: &str,
    config: &ResolvedEmbeddingConfig,
) -> Result<Vec<f64>> {
    ensure_embedding_installed_gate(function_signature)?;
    require_embedding_execution_privilege(function_name)?;
    execute_embedding_call(function_name, text, config)
}

fn parse_dimensions_arg(value: &Value) -> Result<u32> {
    let dims = match value {
        Value::Int32(v) => *v as i64,
        Value::Int64(v) => *v,
        other => {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embedding: expected INTEGER dimensions, got {}",
                    other.type_display_name()
                ),
            }
            .into());
        }
    };
    if dims <= 0 {
        return Err(SqlError::InvalidParameterValue {
            message: "embedding: invalid dimensions value: dimensions must be positive".into(),
        }
        .into());
    }
    if dims > u32::MAX as i64 {
        return Err(SqlError::InvalidParameterValue {
            message: format!(
                "embedding: invalid dimensions value: dimensions too large (max {})",
                u32::MAX
            ),
        }
        .into());
    }
    Ok(dims as u32)
}

pub(crate) fn embed_query_text_with_cache(
    function_name: &str,
    function_signature: &str,
    text: &str,
    dimensions: u32,
) -> Result<Vec<f64>> {
    require_direct_embedding_superuser(function_name)?;
    let config = resolve_embedding_config(None, Some(dimensions))?;
    let cache_key = config.cache_key(text);
    if let Some(cached) = context::cached_embedding(&cache_key)? {
        return Ok(cached);
    }
    let vector = embedding_call_internal(function_name, function_signature, text, &config)?;
    context::cache_embedding(cache_key, vector.clone())?;
    Ok(vector)
}

/// EMBEDDING(text [, model, dimensions]) -> VECTOR
///
/// Extension-gated scalar embedding function. NULL input still goes through the
/// extension visibility and superuser gates to preserve PG-style behavior.
fn embedding_fn(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() || args.len() > 3 {
        return Err(SqlError::InvalidParameterValue {
            message: "embedding: expected 1 to 3 arguments (text [, model, dimensions])".into(),
        }
        .into());
    }

    ensure_embedding_installed_gate("embedding(text)")?;
    require_direct_embedding_superuser("embedding")?;

    let text = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Text(s) if s.trim().is_empty() => {
            return Err(SqlError::InvalidParameterValue {
                message: "embedding: input text must not be empty".into(),
            }
            .into());
        }
        Value::Text(s) => s.clone(),
        other => {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embedding: expected TEXT, got {}",
                    other.type_display_name()
                ),
            }
            .into());
        }
    };

    let model = match args.get(1) {
        Some(Value::Null) | None => None,
        Some(Value::Text(m)) if !m.trim().is_empty() => Some(m.trim()),
        Some(Value::Text(_)) => {
            return Err(SqlError::InvalidParameterValue {
                message: "embedding: model name must not be empty".into(),
            }
            .into());
        }
        Some(other) => {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embedding: expected TEXT model, got {}",
                    other.type_display_name()
                ),
            }
            .into());
        }
    };

    let dimensions = match args.get(2) {
        Some(Value::Null) | None => None,
        Some(value) => Some(parse_dimensions_arg(value)?),
    };

    let config = resolve_embedding_config(model, dimensions)?;
    let vector = execute_embedding_call("embedding", &text, &config)?;
    Ok(Value::Vector(vector))
}

/// EMBED_TEXT(model, text [, json_options])
///
/// TiDB-compatible auto-embedding function.  The model name is the first
/// argument (e.g. `'bedrock/amazon-titan-v2'`), and the text to embed is
/// the second.  An optional third argument is a JSON string with extra
/// options such as `{"dimensions": 1024}`.
fn embed_text_fn(args: Vec<Value>) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(SqlError::InvalidParameterValue {
            message: "embed_text: expected 2 to 3 arguments (model, text [, json_options])".into(),
        }
        .into());
    }

    // arg[0]: model name (TEXT).  For Bedrock this is informational since the
    // real model is baked into the endpoint URL.
    let model = match &args[0] {
        Value::Text(m) if !m.trim().is_empty() => m.trim().to_string(),
        Value::Text(_) => {
            return Err(SqlError::InvalidParameterValue {
                message: "embed_text: model name must not be empty".into(),
            }
            .into());
        }
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embed_text: expected TEXT model, got {}",
                    other.type_display_name()
                ),
            }
            .into());
        }
    };

    // arg[1]: text to embed.
    let text = match &args[1] {
        Value::Null => return Ok(Value::Null),
        Value::Text(s) if s.trim().is_empty() => {
            return Err(SqlError::InvalidParameterValue {
                message: "embed_text: input text must not be empty".into(),
            }
            .into());
        }
        Value::Text(s) => s.clone(),
        other => {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embed_text: expected TEXT, got {}",
                    other.type_display_name()
                ),
            }
            .into());
        }
    };

    // arg[2]: optional JSON options string (e.g. {"dimensions": 1024}).
    let dimensions = match args.get(2) {
        Some(Value::Null) | None => None,
        Some(Value::Text(opts)) => parse_json_dimensions(opts)?,
        Some(other) => {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embed_text: expected TEXT json_options, got {}",
                    other.type_display_name()
                ),
            }
            .into());
        }
    };

    let config = resolve_embedding_config(Some(&model), dimensions)?;
    let vector = embedding_call_internal("embed_text", "embed_text(text, text)", &text, &config)?;
    Ok(Value::Vector(vector))
}

/// Parse dimensions from a JSON options string like `{"dimensions": 1024}`.
fn parse_json_dimensions(opts: &str) -> Result<Option<u32>> {
    parse_embed_text_json_options_dimensions(opts)
        .map_err(|message| SqlError::InvalidParameterValue { message }.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::context;
    use std::collections::{HashMap, HashSet};
    use std::future::Future;
    use std::sync::Arc;

    fn run_with_context<R>(is_superuser: bool, future: impl Future<Output = R>) -> R {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(context::with_context(is_superuser, "default", future))
    }

    fn extension_delta(
        created: &[&str],
        dropped: &[&str],
    ) -> Arc<crate::session_context::ExtensionTxnDelta> {
        let created_set: HashSet<String> = created.iter().map(|s| s.to_ascii_lowercase()).collect();
        let dropped_set: HashSet<String> = dropped.iter().map(|s| s.to_ascii_lowercase()).collect();
        Arc::new((created_set, dropped_set))
    }

    #[test]
    fn register_includes_embedding_and_embed_text() {
        let mut map = HashMap::new();
        register(&mut map);
        assert!(map.contains_key("EMBEDDING"));
        assert!(map.contains_key("EMBED_TEXT"));
    }

    #[test]
    fn direct_embedding_superuser_check_allows_superuser() {
        run_with_context(true, async {
            require_direct_embedding_superuser("embed_text").expect("superuser should be allowed");
        });
    }

    #[test]
    fn direct_embedding_superuser_check_rejects_non_superuser() {
        let err = run_with_context(false, async {
            require_direct_embedding_superuser("embed_text").unwrap_err()
        });
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "42501");
    }

    #[test]
    fn embedding_null_still_checks_extension_gate() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let err = rt
            .block_on(async {
                crate::session_context::with_extension_txn_delta(
                    extension_delta(&[], &["embedding"]),
                    async {
                        crate::extensions::context::with_context(true, "default", async {
                            embedding_fn(vec![Value::Null])
                        })
                        .await
                    },
                )
                .await
            })
            .unwrap_err();

        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "42883");
        assert!(sql_err
            .to_string()
            .contains("function embedding(text) does not exist"));
    }

    #[test]
    fn embedding_null_still_checks_superuser_gate() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let err = rt
            .block_on(async {
                crate::session_context::with_extension_txn_delta(
                    extension_delta(&["embedding"], &[]),
                    async {
                        crate::extensions::context::with_context(false, "default", async {
                            embedding_fn(vec![Value::Null])
                        })
                        .await
                    },
                )
                .await
            })
            .unwrap_err();

        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "42501");
        assert!(sql_err.to_string().contains("permission denied"));
    }

    #[test]
    fn parse_dimensions_arg_accepts_positive_integer() {
        assert_eq!(parse_dimensions_arg(&Value::Int32(1024)).unwrap(), 1024);
    }

    #[test]
    fn parse_dimensions_arg_rejects_zero() {
        let err = parse_dimensions_arg(&Value::Int64(0)).unwrap_err();
        assert!(err
            .to_string()
            .contains("dimensions value: dimensions must be positive"));
    }

    #[test]
    fn parse_json_dimensions_returns_none_when_missing() {
        assert_eq!(parse_json_dimensions("{\"foo\":1}").unwrap(), None);
    }

    #[test]
    fn parse_json_dimensions_accepts_positive_integer() {
        assert_eq!(
            parse_json_dimensions("{\"dimensions\":1024}").unwrap(),
            Some(1024)
        );
    }

    #[test]
    fn parse_json_dimensions_rejects_invalid_json() {
        let err = parse_json_dimensions("{not-json").unwrap_err();
        assert!(err.to_string().contains("invalid JSON options"));
    }

    #[test]
    fn parse_json_dimensions_rejects_non_numeric_dimensions() {
        let err = parse_json_dimensions("{\"dimensions\":\"1024\"}").unwrap_err();
        assert!(err.to_string().contains("dimensions must be a number"));
    }

    #[test]
    fn parse_json_dimensions_rejects_zero() {
        let err = parse_json_dimensions("{\"dimensions\":0}").unwrap_err();
        assert!(err.to_string().contains("dimensions out of range"));
    }

    #[test]
    fn parse_json_dimensions_rejects_negative() {
        let err = parse_json_dimensions("{\"dimensions\":-1}").unwrap_err();
        assert!(err
            .to_string()
            .contains("dimensions must be a positive integer"));
    }

    #[test]
    fn parse_json_dimensions_rejects_out_of_range() {
        let too_large = format!("{{\"dimensions\":{}}}", (u32::MAX as u64) + 1);
        let err = parse_json_dimensions(&too_large).unwrap_err();
        assert!(err.to_string().contains("dimensions out of range"));
    }
}
