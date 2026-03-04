use crate::config::get_embedding_config;
use crate::extensions::context;
use crate::extensions::InstalledExtension;
use crate::model::{ColumnDef, DataType, TableSchema};
use crate::session_context;
use crate::sql::error::SqlError;
use crate::sql::query_context::QueryContext;
use crate::storage::{encode_embedding_usage_key_v2, encode_extension_key_v2};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
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

async fn acquire_embedding_permit(tenant: &str) -> Result<OwnedSemaphorePermit> {
    let limit = QueryContext::current_setting_snapshot("embedding.concurrency")
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
    TableSchema::new(
        "embedding_usage".to_string(),
        0,
        vec![
            ColumnDef {
                name: "tokens_used".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            ColumnDef {
                name: "resets_at".to_string(),
                data_type: DataType::TimestampTz,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
        ],
        vec![],
    )
}

pub(crate) async fn call_embedding_api(
    text: &str,
    model: &str,
    dimensions: u32,
) -> Result<(Vec<f64>, u64)> {
    let config = get_embedding_config();
    let api_key = config.api_key.as_ref().ok_or_else(|| -> anyhow::Error {
        SqlError::Unsupported("embedding: EMBEDDING_API_KEY not set".into()).into()
    })?;

    let tenant = context::tenant_keyspace()
        .ok_or_else(|| anyhow!("embedding: tenant keyspace not available"))?;
    let _permit = acquire_embedding_permit(&tenant).await?;

    let body = serde_json::json!({
        "model": model,
        "input": text,
        "dimensions": dimensions,
        "encoding_format": "float"
    });

    let resp = embedding_http_client()
        .post(&config.endpoint)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("embedding: API returned {} — {}", status, err_body));
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

    let embedding: Vec<f64> = embedding_arr
        .iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_f64()
                .ok_or_else(|| anyhow!("embedding: non-float at index {}: {}", i, v))
        })
        .collect::<Result<Vec<f64>>>()?;

    let tokens_used = json["usage"]["total_tokens"].as_u64().unwrap_or(0);
    Ok((embedding, tokens_used))
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
        txn.put(key.clone(), new_total.to_be_bytes().to_vec())
            .await?;
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
    use chrono::{DateTime, Timelike};
    use std::env;

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

        let (vec, tokens) = context::with_context(true, "default", async move {
            call_embedding_api("db9 live embedding test", "text-embedding-v4", dims).await
        })
        .await
        .expect("live embedding call should succeed");

        assert_eq!(vec.len(), dims as usize, "vector dimension must match");
        assert!(
            tokens > 0,
            "provider should report positive token usage for non-empty input"
        );
    }
}
