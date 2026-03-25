use crate::config::{
    canonical_embedding_model, get_embedding_config, normalize_embedding_endpoint, EmbeddingConfig,
    EmbeddingProvider,
};
use crate::extensions::context;
use crate::extensions::InstalledExtension;
use crate::model::{ColumnDef, DataType, TableSchema};
use crate::session_context;
use crate::sql::error::SqlError;
use crate::sql::query_context::QueryContext;
use crate::storage::{encode_embedding_usage_key_v2, encode_extension_key_v2};
use crate::txn::txn_put;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use tikv_client::{TimestampExt, TransactionClient};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const EMBEDDING_CONNECT_TIMEOUT: Duration = Duration::from_millis(5000);
const EMBEDDING_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const EMBEDDING_CONCURRENCY_PER_TENANT: usize = 5;

static EMBEDDING_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Clone)]
struct TenantSemaphore {
    limit: usize,
    sem: Arc<Semaphore>,
}

static TENANT_SEMAPHORES: LazyLock<DashMap<String, TenantSemaphore>> = LazyLock::new(DashMap::new);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedEmbeddingConfig {
    pub(crate) provider: EmbeddingProvider,
    pub(crate) endpoint: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) dimensions: u32,
}

impl ResolvedEmbeddingConfig {
    pub(crate) fn cache_key(&self, text: &str) -> context::EmbeddingCacheKey {
        let mut hasher = Sha256::new();
        hasher.update(self.api_key.as_bytes());
        context::EmbeddingCacheKey {
            provider: self.provider,
            endpoint: self.endpoint.clone(),
            api_key_fingerprint: hex::encode(hasher.finalize()),
            model: self.model.clone(),
            dimensions: self.dimensions,
            text: text.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingSettingsSource {
    Session,
    Server,
}

fn embedding_http_client() -> &'static reqwest::Client {
    EMBEDDING_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(EMBEDDING_CONNECT_TIMEOUT)
            .timeout(EMBEDDING_REQUEST_TIMEOUT)
            .build()
            .expect("embedding http client init")
    })
}

fn current_embedding_settings_source() -> EmbeddingSettingsSource {
    match context::embedding_execution_mode() {
        context::EmbeddingExecutionMode::Direct => EmbeddingSettingsSource::Session,
        context::EmbeddingExecutionMode::AuthorizedGenerated => EmbeddingSettingsSource::Server,
    }
}

pub(crate) fn current_embedding_runtime_setting(name: &str) -> Option<String> {
    match current_embedding_settings_source() {
        EmbeddingSettingsSource::Session => QueryContext::current_execution_setting_snapshot(name),
        EmbeddingSettingsSource::Server => None,
    }
}

async fn acquire_embedding_permit(tenant: &str) -> Result<OwnedSemaphorePermit> {
    let limit = current_embedding_runtime_setting("embedding.concurrency")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(EMBEDDING_CONCURRENCY_PER_TENANT);
    let sem = get_or_update_tenant_semaphore(tenant, limit);
    sem.acquire_owned().await.map_err(|_| -> anyhow::Error {
        SqlError::InvalidParameterValue {
            message: "embedding: concurrent request limit exceeded".into(),
        }
        .into()
    })
}

fn get_or_update_tenant_semaphore(tenant: &str, limit: usize) -> Arc<Semaphore> {
    match TENANT_SEMAPHORES.entry(tenant.to_string()) {
        Entry::Occupied(mut occ) => {
            if occ.get().limit != limit {
                occ.insert(TenantSemaphore {
                    limit,
                    sem: Arc::new(Semaphore::new(limit)),
                });
            }
            occ.get().sem.clone()
        }
        Entry::Vacant(vac) => {
            let sem = Arc::new(Semaphore::new(limit));
            vac.insert(TenantSemaphore {
                limit,
                sem: sem.clone(),
            });
            sem
        }
    }
}

pub(crate) fn embedding_usage_table_schema() -> TableSchema {
    TableSchema::virtual_table(
        "embedding_usage",
        vec![
            ColumnDef::new("tokens_used", DataType::Int64, false),
            ColumnDef::new("resets_at", DataType::TimestampTz, false),
        ],
    )
}

fn resolve_embedding_provider(value: &str) -> Result<EmbeddingProvider> {
    EmbeddingProvider::parse(value).ok_or_else(|| {
        SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"embedding.provider\": \"{}\"; expected 'openai' or 'bedrock'",
                value
            ),
        }
        .into()
    })
}

pub(crate) fn resolve_embedding_config(
    model_override: Option<&str>,
    dimensions_override: Option<u32>,
) -> Result<ResolvedEmbeddingConfig> {
    resolve_embedding_config_with_lookup(
        get_embedding_config(),
        current_embedding_settings_source(),
        QueryContext::current_execution_setting_snapshot,
        model_override,
        dimensions_override,
    )
}

fn resolve_embedding_config_with_lookup<F>(
    static_config: &EmbeddingConfig,
    settings_source: EmbeddingSettingsSource,
    lookup: F,
    model_override: Option<&str>,
    dimensions_override: Option<u32>,
) -> Result<ResolvedEmbeddingConfig>
where
    F: Fn(&str) -> Option<String>,
{
    let setting = |name: &str| match settings_source {
        EmbeddingSettingsSource::Session => lookup(name),
        EmbeddingSettingsSource::Server => None,
    };

    let provider_raw =
        setting("embedding.provider").unwrap_or_else(|| static_config.provider_name.clone());
    let provider = resolve_embedding_provider(&provider_raw)?;
    let endpoint_raw =
        setting("embedding.endpoint").unwrap_or_else(|| static_config.endpoint.clone());
    let api_key = setting("embedding.api_key")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| static_config.api_key.clone())
        .ok_or_else(|| -> anyhow::Error {
            SqlError::Unsupported("embedding: service not configured on this server".into()).into()
        })?;
    let model = model_override
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| setting("embedding.model"))
        .unwrap_or_else(|| static_config.model.clone());
    let dimensions = dimensions_override.unwrap_or_else(|| {
        setting("embedding.dimensions")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(static_config.dimensions)
    });

    if provider == EmbeddingProvider::OpenAICompatible
        && canonical_embedding_model(&model).is_none()
    {
        return Err(SqlError::InvalidParameterValue {
            message: format!(
                "embedding model \"{}\" is not supported for openai provider; only text-embedding-v4 is supported",
                model
            ),
        }
        .into());
    }

    Ok(ResolvedEmbeddingConfig {
        provider,
        endpoint: normalize_embedding_endpoint(&endpoint_raw, &provider),
        api_key,
        model,
        dimensions,
    })
}

pub(crate) async fn call_embedding_api(
    config: &ResolvedEmbeddingConfig,
    text: &str,
) -> Result<(Vec<f64>, u64)> {
    let tenant = context::tenant_keyspace()
        .ok_or_else(|| anyhow!("embedding: tenant keyspace not available"))?;
    let _permit = acquire_embedding_permit(&tenant).await?;

    match config.provider {
        EmbeddingProvider::OpenAICompatible => {
            call_openai_compatible(
                &config.endpoint,
                &config.api_key,
                text,
                &config.model,
                config.dimensions,
            )
            .await
        }
        EmbeddingProvider::Bedrock => {
            call_bedrock(&config.endpoint, &config.api_key, text, config.dimensions).await
        }
    }
}

async fn call_openai_compatible(
    endpoint: &str,
    api_key: &str,
    text: &str,
    model: &str,
    dimensions: u32,
) -> Result<(Vec<f64>, u64)> {
    let body = serde_json::json!({
        "model": model,
        "input": text,
        "dimensions": dimensions,
        "encoding_format": "float"
    });

    let resp = embedding_http_client()
        .post(endpoint)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "embedding: API returned {} \u{2014} {}",
            status,
            err_body
        ));
    }

    let json: serde_json::Value = resp.json().await?;

    let data_arr = json["data"]
        .as_array()
        .ok_or_else(|| anyhow!("embedding: response missing 'data' array"))?;
    let first = data_arr
        .first()
        .ok_or_else(|| anyhow!("embedding: response 'data' array is empty"))?;
    let embedding_arr = first["embedding"]
        .as_array()
        .ok_or_else(|| anyhow!("embedding: response missing 'embedding' array"))?;

    let embedding = parse_embedding_array(embedding_arr)?;
    let tokens_used = json["usage"]["total_tokens"].as_u64().unwrap_or(0);
    Ok((embedding, tokens_used))
}

async fn call_bedrock(
    endpoint: &str,
    api_key: &str,
    text: &str,
    dimensions: u32,
) -> Result<(Vec<f64>, u64)> {
    let body = serde_json::json!({
        "inputText": text,
        "dimensions": dimensions,
        "normalize": true
    });

    let resp = embedding_http_client()
        .post(endpoint)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "embedding: API returned {} \u{2014} {}",
            status,
            err_body
        ));
    }

    let json: serde_json::Value = resp.json().await?;

    let embedding_arr = json["embedding"]
        .as_array()
        .ok_or_else(|| anyhow!("embedding: response missing 'embedding' array"))?;

    let embedding = parse_embedding_array(embedding_arr)?;
    let tokens_used = json["inputTextTokenCount"].as_u64().unwrap_or(0);
    Ok((embedding, tokens_used))
}

fn parse_embedding_array(arr: &[serde_json::Value]) -> Result<Vec<f64>> {
    arr.iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_f64()
                .ok_or_else(|| anyhow!("embedding: non-float at index {}: {}", i, v))
        })
        .collect::<Result<Vec<f64>>>()
}

pub(crate) fn embedding_function_not_found(function_signature: &str) -> anyhow::Error {
    SqlError::FunctionNotFound(function_signature.to_string()).into()
}

pub(crate) async fn check_embedding_installed(
    client: &TransactionClient,
    db_id: u64,
    function_signature: &str,
) -> Result<()> {
    match session_context::extension_txn_status("embedding") {
        Some(true) => return Ok(()),
        Some(false) => {
            return Err(embedding_function_not_found(function_signature));
        }
        None => {}
    }
    // Deterministic visibility model:
    // 1) in-txn DDL delta override (handled above),
    // 2) explicit transaction path: use transaction snapshot timestamp,
    // 3) autocommit path: use latest committed timestamp per statement.
    let snapshot_ts = session_context::current_txn_snapshot_ts_version()
        .map(tikv_client::Timestamp::from_version)
        .unwrap_or(client.current_timestamp().await?);
    let key = encode_extension_key_v2(db_id, "embedding");
    let mut snap = client.snapshot(
        snapshot_ts,
        tikv_client::TransactionOptions::new_optimistic(),
    );
    let bytes = snap
        .get(key)
        .await?
        .ok_or_else(|| embedding_function_not_found(function_signature))?;
    let ext: InstalledExtension = bincode::deserialize(&bytes)
        .map_err(|_| anyhow!("embedding: corrupt extension metadata"))?;
    if !ext.enabled {
        return Err(embedding_function_not_found(function_signature));
    }
    Ok(())
}

fn parse_u64_counter(bytes: Vec<u8>) -> Result<u64> {
    let arr: [u8; 8] = bytes.try_into().map_err(|v: Vec<u8>| {
        anyhow!(
            "embedding: corrupt usage counter ({} bytes, expected 8)",
            v.len()
        )
    })?;
    Ok(u64::from_be_bytes(arr))
}

fn today_yyyymmdd() -> String {
    Utc::now().format("%Y%m%d").to_string()
}

fn tomorrow_midnight_utc_iso8601() -> String {
    let today = Utc::now().date_naive();
    let tomorrow = today + ChronoDuration::days(1);
    let tomorrow_midnight = tomorrow
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight timestamp");
    let dt_utc = DateTime::<Utc>::from_naive_utc_and_offset(tomorrow_midnight, Utc);
    dt_utc.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) async fn record_embedding_tokens(
    client: &TransactionClient,
    db_id: u64,
    tokens: u64,
) -> Result<u64> {
    let key = encode_embedding_usage_key_v2(db_id, &today_yyyymmdd());
    for _attempt in 0..10 {
        let mut txn = client
            .begin_with_options(tikv_client::TransactionOptions::new_optimistic())
            .await?;
        let current = match txn.get(key.clone()).await? {
            Some(bytes) => parse_u64_counter(bytes)?,
            None => 0,
        };
        let new_total = current
            .checked_add(tokens)
            .ok_or_else(|| anyhow!("embedding: usage counter overflow"))?;
        txn_put(&mut txn, key.clone(), new_total.to_be_bytes().to_vec()).await?;
        match txn.commit().await {
            Ok(_) => return Ok(new_total),
            Err(_) => {
                let _ = txn.rollback().await;
            }
        }
    }
    Err(anyhow!("embedding: usage update failed after 10 retries"))
}

pub(crate) async fn read_embedding_usage(
    client: &TransactionClient,
    db_id: u64,
) -> Result<(i64, String)> {
    let key = encode_embedding_usage_key_v2(db_id, &today_yyyymmdd());
    // Keep extension usage reads aligned with extension visibility contract:
    // - explicit transactions read from transaction snapshot,
    // - autocommit reads latest committed value at statement boundary.
    let snapshot_ts = session_context::current_txn_snapshot_ts_version()
        .map(tikv_client::Timestamp::from_version)
        .unwrap_or(client.current_timestamp().await?);
    let mut snap = client.snapshot(
        snapshot_ts,
        tikv_client::TransactionOptions::new_optimistic(),
    );
    let used_u64 = match snap.get(key).await? {
        Some(bytes) => parse_u64_counter(bytes)?,
        None => 0,
    };

    let used_i64 = i64::try_from(used_u64)
        .map_err(|_| anyhow!("embedding: usage counter overflow for BIGINT"))?;
    Ok((used_i64, tomorrow_midnight_utc_iso8601()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::context;
    use crate::sql::error::SqlError;
    use crate::sql::query_context::{with_scoped_query_context, QueryContext};
    use chrono::{DateTime, Timelike};
    use std::collections::HashMap;
    use std::env;
    use std::sync::Arc;

    #[test]
    fn tenant_semaphore_key_does_not_expand_with_limit_changes() {
        TENANT_SEMAPHORES.clear();

        let sem_5 = get_or_update_tenant_semaphore("tenant_a", 5);
        assert_eq!(TENANT_SEMAPHORES.len(), 1);

        let sem_8 = get_or_update_tenant_semaphore("tenant_a", 8);
        assert_eq!(TENANT_SEMAPHORES.len(), 1);
        assert!(!Arc::ptr_eq(&sem_5, &sem_8));

        let sem_8_again = get_or_update_tenant_semaphore("tenant_a", 8);
        assert_eq!(TENANT_SEMAPHORES.len(), 1);
        assert!(Arc::ptr_eq(&sem_8, &sem_8_again));

        TENANT_SEMAPHORES.clear();
    }

    #[test]
    fn embedding_function_not_found_maps_to_42883() {
        let err = embedding_function_not_found("embedding(text)");
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "42883");
        assert_eq!(
            sql_err.to_string(),
            "function embedding(text) does not exist"
        );
    }

    #[test]
    fn parse_u64_counter_roundtrip_big_endian() {
        let n = 123_456_789_u64;
        let bytes = n.to_be_bytes().to_vec();
        let parsed = parse_u64_counter(bytes).expect("must parse");
        assert_eq!(parsed, n);
    }

    #[test]
    fn parse_u64_counter_accepts_zero() {
        let parsed = parse_u64_counter(0_u64.to_be_bytes().to_vec()).expect("must parse");
        assert_eq!(parsed, 0);
    }

    #[test]
    fn parse_u64_counter_rejects_wrong_length() {
        let err = parse_u64_counter(vec![1, 2, 3]).unwrap_err();
        assert!(err
            .to_string()
            .contains("corrupt usage counter (3 bytes, expected 8)"));
    }

    #[test]
    fn today_key_and_reset_timestamp_formats_are_stable() {
        let day = today_yyyymmdd();
        assert_eq!(day.len(), 8);
        assert!(day.chars().all(|c| c.is_ascii_digit()));

        let resets_at = tomorrow_midnight_utc_iso8601();
        assert!(resets_at.ends_with('Z'));
        let parsed = DateTime::parse_from_rfc3339(&resets_at).expect("must be RFC3339");
        assert_eq!(parsed.offset().local_minus_utc(), 0);
        assert_eq!(parsed.hour(), 0);
        assert_eq!(parsed.minute(), 0);
        assert_eq!(parsed.second(), 0);
    }

    #[test]
    fn embedding_usage_table_schema_contract() {
        let schema = embedding_usage_table_schema();
        assert_eq!(schema.name, "embedding_usage");
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "tokens_used");
        assert!(matches!(schema.columns[0].data_type, DataType::Int64));
        assert!(!schema.columns[0].nullable);
        assert_eq!(schema.columns[1].name, "resets_at");
        assert!(matches!(schema.columns[1].data_type, DataType::TimestampTz));
        assert!(!schema.columns[1].nullable);
    }

    #[tokio::test]
    async fn resolve_embedding_config_accepts_bedrock_titan_model() {
        let mut qctx = QueryContext::for_tests();
        let mut snapshot = HashMap::new();
        snapshot.insert("embedding.provider".to_string(), "bedrock".to_string());
        snapshot.insert(
            "embedding.model".to_string(),
            "tidbcloud_free/amazon/titan-embed-text-v2".to_string(),
        );
        snapshot.insert("embedding.api_key".to_string(), "test-key".to_string());
        snapshot.insert(
            "embedding.endpoint".to_string(),
            "https://bedrock.example.com/invoke/".to_string(),
        );
        qctx.settings_snapshot = Some(Arc::new(snapshot.clone()));
        qctx.execution_settings_snapshot = Some(Arc::new(snapshot));

        let config =
            with_scoped_query_context(&qctx, async { resolve_embedding_config(None, None) })
                .await
                .expect("bedrock titan model should resolve");

        assert_eq!(config.provider, EmbeddingProvider::Bedrock);
        assert_eq!(config.model, "tidbcloud_free/amazon/titan-embed-text-v2");
        assert_eq!(config.endpoint, "https://bedrock.example.com/invoke");
    }

    #[tokio::test]
    async fn resolve_embedding_config_rejects_bedrock_titan_model_on_openai() {
        let mut qctx = QueryContext::for_tests();
        let mut snapshot = HashMap::new();
        snapshot.insert("embedding.provider".to_string(), "openai".to_string());
        snapshot.insert(
            "embedding.model".to_string(),
            "tidbcloud_free/amazon/titan-embed-text-v2".to_string(),
        );
        snapshot.insert("embedding.api_key".to_string(), "test-key".to_string());
        snapshot.insert(
            "embedding.endpoint".to_string(),
            "https://openai.example.com/v1".to_string(),
        );
        qctx.settings_snapshot = Some(Arc::new(snapshot.clone()));
        qctx.execution_settings_snapshot = Some(Arc::new(snapshot));

        let err = with_scoped_query_context(&qctx, async {
            resolve_embedding_config(None, None).unwrap_err()
        })
        .await;
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "22023");
        assert!(sql_err
            .to_string()
            .contains("only text-embedding-v4 is supported"));
    }

    #[tokio::test]
    async fn resolve_embedding_config_rejects_empty_api_key() {
        let mut qctx = QueryContext::for_tests();
        let mut snapshot = HashMap::new();
        snapshot.insert("embedding.provider".to_string(), "bedrock".to_string());
        snapshot.insert(
            "embedding.model".to_string(),
            "tidbcloud_free/amazon/titan-embed-text-v2".to_string(),
        );
        snapshot.insert("embedding.api_key".to_string(), "   ".to_string());
        snapshot.insert(
            "embedding.endpoint".to_string(),
            "https://bedrock.example.com/invoke".to_string(),
        );
        qctx.settings_snapshot = Some(Arc::new(snapshot.clone()));
        qctx.execution_settings_snapshot = Some(Arc::new(snapshot));

        let err = with_scoped_query_context(&qctx, async {
            resolve_embedding_config(None, None).unwrap_err()
        })
        .await;
        assert!(err
            .to_string()
            .contains("service not configured on this server"));
    }

    #[tokio::test]
    async fn current_embedding_runtime_setting_is_hidden_in_authorized_generated_mode() {
        let mut qctx = QueryContext::for_tests();
        let mut snapshot = HashMap::new();
        snapshot.insert("embedding.provider".to_string(), "bedrock".to_string());
        qctx.settings_snapshot = Some(Arc::new(snapshot.clone()));
        qctx.execution_settings_snapshot = Some(Arc::new(snapshot));

        let direct = context::with_context(false, "default", async {
            with_scoped_query_context(&qctx, async {
                current_embedding_runtime_setting("embedding.provider")
            })
            .await
        })
        .await;
        assert_eq!(direct.as_deref(), Some("bedrock"));

        let sealed = context::with_context(false, "default", async {
            with_scoped_query_context(&qctx, async {
                context::with_embedding_authorized(async {
                    current_embedding_runtime_setting("embedding.provider")
                })
                .await
            })
            .await
            .expect("authorized mode should be available")
        })
        .await;
        assert_eq!(sealed, None);
    }

    #[test]
    fn resolve_embedding_config_server_source_ignores_session_overrides() {
        let static_config = EmbeddingConfig {
            provider_name: "openai".to_string(),
            api_key: Some("server-key".to_string()),
            endpoint: "https://server.example/v1".to_string(),
            model: "text-embedding-v4".to_string(),
            dimensions: 1024,
        };
        let mut dynamic = HashMap::new();
        dynamic.insert("embedding.provider".to_string(), "bedrock".to_string());
        dynamic.insert(
            "embedding.endpoint".to_string(),
            "https://attacker.example/invoke".to_string(),
        );
        dynamic.insert("embedding.api_key".to_string(), "attacker-key".to_string());
        dynamic.insert(
            "embedding.model".to_string(),
            "tidbcloud_free/amazon/titan-embed-text-v2".to_string(),
        );
        dynamic.insert("embedding.dimensions".to_string(), "4096".to_string());

        let config = resolve_embedding_config_with_lookup(
            &static_config,
            EmbeddingSettingsSource::Server,
            |name| dynamic.get(name).cloned(),
            None,
            None,
        )
        .expect("server source should ignore session overrides");

        assert_eq!(config.provider, EmbeddingProvider::OpenAICompatible);
        assert_eq!(config.endpoint, "https://server.example/v1/embeddings");
        assert_eq!(config.api_key, "server-key");
        assert_eq!(config.model, "text-embedding-v4");
        assert_eq!(config.dimensions, 1024);
    }

    #[test]
    fn cache_key_includes_api_key_identity() {
        let config_a = ResolvedEmbeddingConfig {
            provider: EmbeddingProvider::Bedrock,
            endpoint: "https://bedrock.example.com/invoke".to_string(),
            api_key: "key-a".to_string(),
            model: "tidbcloud_free/amazon/titan-embed-text-v2".to_string(),
            dimensions: 1024,
        };
        let config_b = ResolvedEmbeddingConfig {
            api_key: "key-b".to_string(),
            ..config_a.clone()
        };

        let key_a = config_a.cache_key("hello");
        let key_b = config_b.cache_key("hello");

        assert_ne!(key_a, key_b);
        assert_ne!(key_a.api_key_fingerprint, key_b.api_key_fingerprint);
    }

    #[tokio::test]
    #[ignore = "live embedding provider test; requires EMBEDDING_API_KEY and EMBEDDING_BASE_URL/EMBEDDING_ENDPOINT"]
    async fn call_embedding_api_live_returns_expected_dimensions() {
        let has_key = env::var("EMBEDDING_API_KEY")
            .ok()
            .is_some_and(|v| !v.trim().is_empty());
        assert!(
            has_key,
            "EMBEDDING_API_KEY is required for live embedding test"
        );

        let has_endpoint = env::var("EMBEDDING_ENDPOINT")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
            || env::var("EMBEDDING_BASE_URL")
                .ok()
                .is_some_and(|v| !v.trim().is_empty());
        assert!(
            has_endpoint,
            "EMBEDDING_ENDPOINT or EMBEDDING_BASE_URL is required for live embedding test"
        );

        let dims = env::var("EMBEDDING_TEST_DIMENSIONS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .or_else(|| {
                env::var("EMBEDDING_DIMENSIONS")
                    .ok()
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .filter(|v| *v > 0)
            })
            .unwrap_or(1024);
        let model = env::var("EMBEDDING_TEST_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                env::var("EMBEDDING_MODEL")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| "text-embedding-v4".to_string());

        let (vec, tokens) = context::with_context(true, "default", async move {
            let config = resolve_embedding_config(Some(&model), Some(dims))
                .expect("embedding config should resolve");
            call_embedding_api(&config, "db9 live embedding test").await
        })
        .await
        .expect("live embedding call should succeed");

        assert_eq!(vec.len(), dims as usize, "vector dimension must match");
        assert!(
            tokens > 0,
            "provider should report positive token usage for non-empty input"
        );
    }

    #[test]
    fn parse_embedding_array_valid() {
        let arr: Vec<serde_json::Value> = vec![
            serde_json::json!(0.1),
            serde_json::json!(0.2),
            serde_json::json!(-0.5),
        ];
        let result = parse_embedding_array(&arr).unwrap();
        assert_eq!(result.len(), 3);
        assert!((result[0] - 0.1).abs() < f64::EPSILON);
        assert!((result[1] - 0.2).abs() < f64::EPSILON);
        assert!((result[2] - (-0.5)).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_embedding_array_rejects_non_float() {
        let arr: Vec<serde_json::Value> =
            vec![serde_json::json!(0.1), serde_json::json!("not_a_float")];
        let err = parse_embedding_array(&arr).unwrap_err();
        assert!(err.to_string().contains("non-float at index 1"));
    }

    #[test]
    fn parse_embedding_array_empty() {
        let arr: Vec<serde_json::Value> = vec![];
        let result = parse_embedding_array(&arr).unwrap();
        assert!(result.is_empty());
    }
}
