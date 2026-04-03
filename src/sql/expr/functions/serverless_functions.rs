use crate::extensions::context;
use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::SqlFn;

/// Backend URL for the internal invoke endpoint.
fn backend_url() -> Option<&'static str> {
    static URL: OnceLock<Option<String>> = OnceLock::new();
    URL.get_or_init(|| {
        std::env::var("DB9_FUNCTIONS_BACKEND_URL")
            .ok()
            .map(|s| s.trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
    })
    .as_deref()
}

/// Shared secret for service-to-service auth with the backend.
fn internal_secret() -> Option<&'static str> {
    static SECRET: OnceLock<Option<String>> = OnceLock::new();
    SECRET
        .get_or_init(|| {
            std::env::var("INTERNAL_CONTROL_SECRET")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .as_deref()
}

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn get_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // Generous HTTP timeout — the real cap is the SQL statement_timeout.
            // Serverless functions may run up to 300s; this just prevents leaked
            // connections if the backend becomes unresponsive.
            .timeout(Duration::from_secs(310))
            .build()
            .expect("failed to build reqwest client for serverless_functions")
    })
}

/// Bridge sync SqlFn to async HTTP calls.
fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// Maximum allowed nesting level. 0 = only the initial SQL invoke is allowed;
/// a function invoked via SQL cannot call invoke() again.
const MAX_INVOKE_NESTING: u32 = 1;

/// RAII guard that decrements invoke depth on drop, ensuring correct cleanup
/// even on panic or early return.
struct InvokeDepthGuard;

impl Drop for InvokeDepthGuard {
    fn drop(&mut self) {
        context::leave_invoke();
    }
}

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("SERVERLESS_FUNCTIONS.INVOKE", invoke);
}

/// `serverless_functions.invoke(function_name TEXT, input JSONB) → JSONB`
///
/// Invokes a serverless function by name and returns its result as JSONB.
///
/// - Superuser only
/// - Rejected inside explicit transactions (functions have side effects)
/// - Recursion depth limited to 1 (no nested invoke)
/// - Shares HTTP quota (100/statement) with http_get/http_post
fn invoke(args: Vec<Value>) -> Result<Value> {
    // 1. Permission check: superuser only
    if !context::is_superuser() {
        return Err(SqlError::PermissionDenied {
            object_type: "extension".into(),
            object_name: "\"serverless_functions\"".into(),
        }
        .into());
    }

    // 2. Configuration check
    let base_url = backend_url().ok_or_else(|| {
        anyhow!("serverless_functions.invoke: DB9_FUNCTIONS_BACKEND_URL is not configured")
    })?;
    let secret = internal_secret().ok_or_else(|| {
        anyhow!("serverless_functions.invoke: INTERNAL_CONTROL_SECRET is not configured")
    })?;

    // 3. Transaction check: reject inside explicit transaction
    if context::is_in_transaction() {
        return Err(SqlError::Unsupported(
            "serverless_functions.invoke() cannot be called inside a transaction block".into(),
        )
        .into());
    }

    // 4. Recursion depth check (RAII guard ensures leave_invoke on all exit paths)
    context::try_enter_invoke(MAX_INVOKE_NESTING)?;
    let _guard = InvokeDepthGuard;
    invoke_inner(args, base_url, secret)
}

fn invoke_inner(args: Vec<Value>, base_url: &str, secret: &str) -> Result<Value> {
    // 5. HTTP quota — shared with http_get/http_post (100/statement)
    context::try_consume_http_request(100)?;

    // 6. Extract arguments
    let function_name = match args.first() {
        Some(Value::Text(s)) => s.clone(),
        Some(Value::Null) => {
            return Err(anyhow!(
                "serverless_functions.invoke: function_name must not be NULL"
            ))
        }
        Some(other) => {
            return Err(anyhow!(
                "serverless_functions.invoke: function_name must be TEXT, got {}",
                other.type_display_name()
            ))
        }
        None => {
            return Err(anyhow!(
                "serverless_functions.invoke: missing required argument function_name"
            ))
        }
    };

    let input_json = match args.get(1) {
        Some(Value::Jsonb(s)) => Some(s.clone()),
        Some(Value::Text(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => {
            return Err(anyhow!(
                "serverless_functions.invoke: input must be JSONB or TEXT, got {}",
                other.type_display_name()
            ))
        }
    };

    // 7. Gather context
    let tenant_keyspace = context::tenant_keyspace()
        .ok_or_else(|| anyhow!("serverless_functions.invoke: extension context not available"))?;
    // Backend expects tenant_id (without keyspace prefix), not the full keyspace string.
    let tenant_id = tenant_keyspace
        .strip_prefix("db9_tenant_")
        .unwrap_or(&tenant_keyspace);
    let invoke_depth = context::current_invoke_depth().saturating_sub(1); // current depth (already incremented)

    tracing::info!(
        tenant_id = %tenant_id,
        function_name = %function_name,
        invoke_depth = invoke_depth,
        has_input = input_json.is_some(),
        "sql_invoke: request"
    );

    // 8. Build request
    let caller_sub = context::caller_sub();
    // Deterministic transport-level idempotency key: protects against HTTP-level
    // retries/replays on the internal boundary. Components: tenant, function,
    // statement timestamp (stable within a statement), and the HTTP request
    // ordinal within this statement (from the shared quota counter).
    let stmt_ts = crate::sql::statement_time::statement_timestamp_millis_or_now();
    let call_ordinal = context::http_request_ordinal(); // Nth HTTP call in this statement
    let idempotency_key = format!(
        "sql:{}:{}:{}:{}",
        tenant_id, function_name, stmt_ts, call_ordinal
    );
    let url = format!("{}/internal/v1/functions/invoke", base_url);
    let body = serde_json::json!({
        "tenant_id": tenant_id,
        "function_name": function_name,
        "input_json": input_json,
        "caller_sub": caller_sub,
        "invoke_depth": invoke_depth,
        "idempotency_key": idempotency_key,
    });

    // 9. Execute HTTP request (single block_in_place for both send + body read)
    let start = Instant::now();
    let (status, response_text) = run_async(async {
        let resp = get_client()
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-Internal-Secret", secret)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("serverless_functions.invoke: request failed: {e}"))?;
        let st = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("serverless_functions.invoke: failed to read response: {e}"))?;
        Ok::<_, anyhow::Error>((st, text))
    })?;
    let elapsed_ms = start.elapsed().as_millis();

    // 10. Parse response
    if !status.is_success() {
        // Try to extract error details from JSON response
        if let Ok(err_json) = serde_json::from_str::<serde_json::Value>(&response_text) {
            let code = err_json
                .get("error_code")
                .and_then(|v| v.as_str())
                .unwrap_or("invoke_error");
            let msg = err_json
                .get("message")
                .or_else(|| err_json.get("error_message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            tracing::warn!(
                tenant_id = %tenant_id,
                function_name = %function_name,
                http_status = status.as_u16(),
                error_code = %code,
                elapsed_ms = elapsed_ms,
                "sql_invoke: backend error"
            );
            return Err(anyhow!("serverless_functions.invoke: {} ({})", msg, code));
        }
        tracing::warn!(
            tenant_id = %tenant_id,
            function_name = %function_name,
            http_status = status.as_u16(),
            elapsed_ms = elapsed_ms,
            "sql_invoke: backend HTTP error"
        );
        return Err(anyhow!(
            "serverless_functions.invoke: HTTP {} — {}",
            status,
            response_text
        ));
    }

    let resp: serde_json::Value = serde_json::from_str(&response_text)
        .map_err(|e| anyhow!("serverless_functions.invoke: invalid response JSON: {e}"))?;

    let resp_status = resp
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if resp_status != "succeeded" {
        let error_msg = resp
            .get("error_message")
            .and_then(|v| v.as_str())
            .unwrap_or("function did not complete successfully");
        let error_code = resp
            .get("error_code")
            .and_then(|v| v.as_str())
            .unwrap_or("invoke_failed");
        tracing::warn!(
            tenant_id = %tenant_id,
            function_name = %function_name,
            status = %resp_status,
            error_code = %error_code,
            elapsed_ms = elapsed_ms,
            "sql_invoke: function did not complete"
        );
        return Err(anyhow!(
            "serverless_functions.invoke: {} ({}: {})",
            error_msg,
            error_code,
            resp_status
        ));
    }

    tracing::info!(
        tenant_id = %tenant_id,
        function_name = %function_name,
        elapsed_ms = elapsed_ms,
        "sql_invoke: completed"
    );

    // 11. Return result_json as JSONB, or NULL if absent
    match resp.get("result_json") {
        Some(serde_json::Value::String(s)) => Ok(Value::Jsonb(s.clone())),
        Some(serde_json::Value::Null) | None => Ok(Value::Null),
        Some(other) => Ok(Value::Jsonb(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::context::{self, ExtensionContextOpts};

    static INIT_ENV: std::sync::Once = std::sync::Once::new();

    /// Ensure env vars are set before any test that passes the config check.
    /// OnceLock in backend_url()/internal_secret() is lazy, so this must run
    /// before the first invoke() call that reaches step 2.
    fn ensure_env() {
        INIT_ENV.call_once(|| unsafe {
            std::env::set_var("DB9_FUNCTIONS_BACKEND_URL", "http://localhost:19999");
            std::env::set_var("INTERNAL_CONTROL_SECRET", "test-secret");
        });
    }

    fn sample_args() -> Vec<Value> {
        vec![Value::Text("my_function".into()), Value::Null]
    }

    #[tokio::test]
    async fn invoke_rejects_non_superuser() {
        let err = context::with_context(false, "test_ks", async { invoke(sample_args()) })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("permission denied for extension"),
            "expected PermissionDenied, got: {err}"
        );
    }

    #[tokio::test]
    async fn invoke_rejects_inside_transaction() {
        ensure_env();
        let opts =
            ExtensionContextOpts::statement(true, false, "test_ks").with_in_transaction(true);
        let err = context::with_context_opts(opts, async { invoke(sample_args()) })
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot be called inside a transaction"),
            "expected transaction rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn invoke_rejects_nested_invocation() {
        ensure_env();
        let err = context::with_context(true, "test_ks", async {
            // Pre-set depth to 1 so invoke() sees depth >= MAX_INVOKE_NESTING (1)
            context::try_enter_invoke(100).unwrap();
            invoke(sample_args())
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("max recursion depth"),
            "expected recursion depth error, got: {err}"
        );
    }
}
