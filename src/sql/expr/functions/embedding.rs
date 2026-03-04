use crate::config::{canonical_embedding_model, get_embedding_config};
use crate::extensions::context;
use crate::extensions::embedding::{
    call_embedding_api, check_embedding_installed, embedding_function_not_found,
    record_embedding_tokens,
};
use crate::model::Value;
use crate::session_context::current_database_id;
use crate::sql::error::SqlError;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("EMBEDDING", embedding_fn);
}

fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

fn setting_or(name: &str, default: &str) -> String {
    QueryContext::current_setting_snapshot(name).unwrap_or_else(|| default.to_string())
}

fn setting_u32_or(name: &str, default: u32) -> u32 {
    QueryContext::current_setting_snapshot(name)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
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

fn normalize_embedding_model(model: &str) -> Result<String> {
    canonical_embedding_model(model)
        .map(|m| m.to_string())
        .ok_or_else(|| {
            SqlError::InvalidParameterValue {
                message: format!(
                    "embedding: unsupported model \"{}\"; only text-embedding-v4 is supported",
                    model
                ),
            }
            .into()
        })
}

fn ensure_embedding_installed_gate() -> Result<()> {
    match crate::session_context::extension_txn_status("embedding") {
        Some(true) => Ok(()),
        Some(false) => Err(embedding_function_not_found("embedding(text)")),
        None => {
            let client = context::tikv_client()
                .ok_or_else(|| anyhow!("embedding: tikv client not available"))?;
            let db_id = current_database_id();
            run_async(check_embedding_installed(&client, db_id, "embedding(text)"))
        }
    }
}

fn embedding_fn(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() || args.len() > 3 {
        return Err(SqlError::InvalidParameterValue {
            message: "embedding: expected 1 to 3 arguments (text [, model, dimensions])".into(),
        }
        .into());
    }

    // Gate checks must run for all invocations, including NULL input.
    ensure_embedding_installed_gate()?;
    if !context::is_superuser() {
        return Err(SqlError::PermissionDenied {
            object_type: "function".into(),
            object_name: "embedding".into(),
        }
        .into());
    }

    let config = get_embedding_config();
    if !config.is_available() {
        return Err(SqlError::Unsupported(
            "embedding: service not configured on this server".into(),
        )
        .into());
    }

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
        Some(Value::Text(m)) => m.clone(),
        Some(Value::Null) | None => setting_or("embedding.model", &config.model),
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
    let model = normalize_embedding_model(&model)?;
    let dimensions = match args.get(2) {
        Some(Value::Null) | None => setting_u32_or("embedding.dimensions", config.dimensions),
        Some(v) => parse_dimensions_arg(v)?,
    };

    let max_calls = setting_u32_or("embedding.max_calls", 100);
    if max_calls == 100 {
        context::try_consume_embedding_call()?;
    } else {
        context::try_consume_embedding_call_with_limit(max_calls)?;
    }

    let (vector, tokens_used) = run_async(call_embedding_api(&text, &model, dimensions))?;

    if vector.len() != dimensions as usize {
        return Err(anyhow!(
            "embedding: API returned {} dimensions, expected {}",
            vector.len(),
            dimensions
        ));
    }

    if tokens_used > 0 {
        let client = context::tikv_client()
            .ok_or_else(|| anyhow!("embedding: tikv client not available"))?;
        let db_id = current_database_id();
        if let Err(e) = run_async(record_embedding_tokens(&client, db_id, tokens_used)) {
            tracing::warn!("embedding: failed to record token usage: {}", e);
        }
    }

    Ok(Value::Vector(vector))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn extension_delta(
        created: &[&str],
        dropped: &[&str],
    ) -> Arc<crate::session_context::ExtensionTxnDelta> {
        let created_set: HashSet<String> = created.iter().map(|s| s.to_ascii_lowercase()).collect();
        let dropped_set: HashSet<String> = dropped.iter().map(|s| s.to_ascii_lowercase()).collect();
        Arc::new((created_set, dropped_set))
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
    fn embedding_null_prefers_extension_not_found_over_permission_denied() {
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
        assert_eq!(sql_err.sqlstate(), "42883");
    }

    #[test]
    fn ddl_create_is_visible_in_txn_delta() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let delta = Arc::new((HashSet::from(["embedding".to_string()]), HashSet::new()));
            let status = crate::session_context::with_extension_txn_delta(delta, async {
                crate::session_context::extension_txn_status("embedding")
            })
            .await;
            assert_eq!(status, Some(true));
        });
    }

    #[test]
    fn ddl_drop_is_visible_in_txn_delta() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut dropped = HashSet::new();
            dropped.insert("embedding".to_string());
            let delta = Arc::new((HashSet::new(), dropped));
            let status = crate::session_context::with_extension_txn_delta(delta, async {
                crate::session_context::extension_txn_status("embedding")
            })
            .await;
            assert_eq!(status, Some(false));
        });
    }

    #[test]
    fn ensure_embedding_installed_gate_honors_created_delta_without_client() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let res = crate::session_context::with_extension_txn_delta(
                extension_delta(&["embedding"], &[]),
                async { ensure_embedding_installed_gate() },
            )
            .await;
            assert!(res.is_ok());
        });
    }

    #[test]
    fn ensure_embedding_installed_gate_honors_dropped_delta_without_client() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let err = rt
            .block_on(async {
                crate::session_context::with_extension_txn_delta(
                    extension_delta(&[], &["embedding"]),
                    async { ensure_embedding_installed_gate() },
                )
                .await
            })
            .unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "42883");
    }

    #[test]
    fn ensure_embedding_installed_gate_without_delta_and_without_client_errors() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let err = rt
            .block_on(async {
                crate::session_context::with_extension_txn_delta(
                    extension_delta(&[], &[]),
                    async { ensure_embedding_installed_gate() },
                )
                .await
            })
            .unwrap_err();
        assert!(err.to_string().contains("embedding: tikv client not available"));
    }

    #[test]
    fn normalize_embedding_model_rejects_non_v4() {
        let err = normalize_embedding_model("text-embedding-v3").unwrap_err();
        assert!(err
            .to_string()
            .contains("only text-embedding-v4 is supported"));
    }

    #[test]
    fn normalize_embedding_model_accepts_v4_case_insensitive() {
        let model = normalize_embedding_model("TEXT-EMBEDDING-V4").unwrap();
        assert_eq!(model, "text-embedding-v4");
    }

    #[test]
    fn normalize_embedding_model_accepts_v4_with_spaces() {
        let model = normalize_embedding_model("  text-embedding-v4  ").unwrap();
        assert_eq!(model, "text-embedding-v4");
    }

    #[test]
    fn embedding_wrong_arity_is_22023() {
        let err = embedding_fn(vec![]).unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "22023");
        assert!(sql_err.to_string().contains("expected 1 to 3 arguments"));
    }

    #[test]
    fn embedding_too_many_args_is_22023() {
        let err = embedding_fn(vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()),
            Value::Int32(1),
            Value::Int32(2),
        ])
        .unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "22023");
        assert!(sql_err.to_string().contains("expected 1 to 3 arguments"));
    }

    #[test]
    fn parse_dimensions_arg_accepts_int32_and_int64() {
        let d1 = parse_dimensions_arg(&Value::Int32(384)).expect("int32 should pass");
        let d2 = parse_dimensions_arg(&Value::Int64(1024)).expect("int64 should pass");
        assert_eq!(d1, 384);
        assert_eq!(d2, 1024);
    }

    #[test]
    fn parse_dimensions_arg_accepts_u32_max() {
        let d = parse_dimensions_arg(&Value::Int64(u32::MAX as i64)).expect("must pass");
        assert_eq!(d, u32::MAX);
    }

    #[test]
    fn parse_dimensions_arg_rejects_non_integer_as_22023() {
        let err = parse_dimensions_arg(&Value::Text("1024".to_string())).unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "22023");
        assert!(sql_err.to_string().contains("expected INTEGER dimensions"));
    }

    #[test]
    fn parse_dimensions_arg_rejects_non_positive_as_22023() {
        let err_zero = parse_dimensions_arg(&Value::Int32(0)).unwrap_err();
        let sql_zero = err_zero.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_zero.sqlstate(), "22023");
        assert!(sql_zero.to_string().contains("dimensions must be positive"));

        let err_negative = parse_dimensions_arg(&Value::Int64(-1)).unwrap_err();
        let sql_negative = err_negative
            .downcast_ref::<SqlError>()
            .expect("must be SqlError");
        assert_eq!(sql_negative.sqlstate(), "22023");
        assert!(sql_negative
            .to_string()
            .contains("dimensions must be positive"));
    }

    #[test]
    fn parse_dimensions_arg_rejects_overflow_as_22023() {
        let err = parse_dimensions_arg(&Value::Int64((u32::MAX as i64) + 1)).unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "22023");
        assert!(sql_err.to_string().contains("dimensions too large"));
    }
}
