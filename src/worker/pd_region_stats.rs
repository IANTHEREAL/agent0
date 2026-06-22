use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tikv_client::request::{EncodeKeyspace, KeyMode, Keyspace};
use tokio::sync::Mutex;

use crate::storage::encode_database_data_range;

pub(crate) const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PdRegionStats {
    pub count: u64,
    pub empty_count: u64,
    pub storage_size_mib: u64,
    pub storage_keys: u64,
}

impl PdRegionStats {
    pub(crate) fn total_bytes_estimate(&self) -> u64 {
        self.storage_size_mib.saturating_mul(MIB)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabasePdRegionStats {
    pub keyspace_id: u32,
    pub logical_start: Vec<u8>,
    pub logical_end: Vec<u8>,
    pub encoded_start: Vec<u8>,
    pub encoded_end: Vec<u8>,
    pub stats: PdRegionStats,
}

#[derive(Debug, Deserialize)]
struct RegionStatsResponse {
    #[serde(default)]
    count: u64,
    #[serde(default)]
    empty_count: u64,
    #[serde(default)]
    storage_size: u64,
    #[serde(default)]
    storage_keys: u64,
}

type KeyspaceIdCache = RwLock<HashMap<(String, String), u32>>;

fn keyspace_id_cache() -> &'static KeyspaceIdCache {
    static CACHE: OnceLock<KeyspaceIdCache> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn pd_stats_rate_limiter() -> &'static Mutex<Option<Instant>> {
    static LAST_CALL: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST_CALL.get_or_init(|| Mutex::new(None))
}

pub(crate) async fn enforce_pd_stats_rate_limit(min_interval_ms: u64) {
    if min_interval_ms == 0 {
        return;
    }
    let min_interval = Duration::from_millis(min_interval_ms);
    let mut last_call = pd_stats_rate_limiter().lock().await;
    if let Some(last) = *last_call {
        let elapsed = last.elapsed();
        if elapsed < min_interval {
            tokio::time::sleep(min_interval - elapsed).await;
        }
    }
    *last_call = Some(Instant::now());
}

pub(crate) fn pd_keyspace_name(keyspace: &str) -> &str {
    if keyspace == "default" {
        "DEFAULT"
    } else {
        keyspace
    }
}

fn cache_cluster_key(pd_endpoints: &[String]) -> String {
    pd_endpoints.join(",")
}

pub(crate) fn encode_region_key_for_txn(keyspace_id: u32, logical_key: Vec<u8>) -> Vec<u8> {
    tikv_client::Key::from(logical_key)
        .encode_keyspace(Keyspace::Enable { keyspace_id }, KeyMode::Txn)
        .to_encoded()
        .into()
}

pub(crate) fn encode_database_region_range(
    keyspace_id: u32,
    db_id: u64,
) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let (logical_start, logical_end) = encode_database_data_range(db_id);
    let encoded_start = encode_region_key_for_txn(keyspace_id, logical_start.clone());
    let encoded_end = encode_region_key_for_txn(keyspace_id, logical_end.clone());
    (logical_start, logical_end, encoded_start, encoded_end)
}

pub(crate) fn escape_pd_query_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn pd_region_stats_path(encoded_start: &[u8], encoded_end: &[u8]) -> String {
    format!(
        "/pd/api/v1/stats/region?start_key={}&end_key={}",
        escape_pd_query_bytes(encoded_start),
        escape_pd_query_bytes(encoded_end)
    )
}

fn parse_keyspace_id(body: serde_json::Value, keyspace: &str) -> Result<u32> {
    let id = body
        .get("id")
        .ok_or_else(|| anyhow!("PD keyspace '{}' response missing id", keyspace))?;
    let id = match id {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| anyhow!("PD keyspace '{}' id is not an unsigned integer", keyspace))?,
        serde_json::Value::String(s) => s
            .parse::<u64>()
            .with_context(|| format!("PD keyspace '{}' id is not a valid integer", keyspace))?,
        other => {
            return Err(anyhow!(
                "PD keyspace '{}' id has unsupported JSON type: {}",
                keyspace,
                other
            ));
        }
    };
    u32::try_from(id).with_context(|| format!("PD keyspace '{}' id does not fit u32", keyspace))
}

pub(crate) async fn fetch_keyspace_id(pd_endpoints: &[String], keyspace: &str) -> Result<u32> {
    let keyspace = pd_keyspace_name(keyspace);
    let cluster_key = cache_cluster_key(pd_endpoints);
    let cache_key = (cluster_key.clone(), keyspace.to_string());
    if let Some(id) = keyspace_id_cache().read().get(&cache_key).copied() {
        return Ok(id);
    }

    let client = crate::worker::build_pd_client()?;
    let mut last_err = None;
    for endpoint in pd_endpoints {
        let url = format!(
            "{}/pd/api/v2/keyspaces/{}",
            crate::worker::pd_base_url(endpoint),
            keyspace
        );
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.json::<serde_json::Value>().await.with_context(|| {
                    format!("failed to parse PD keyspace '{}' response", keyspace)
                })?;
                let id = parse_keyspace_id(body, keyspace)?;
                keyspace_id_cache().write().insert(cache_key, id);
                return Ok(id);
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = Some(anyhow!(
                    "PD keyspace '{}' query returned status={} body={}",
                    keyspace,
                    status,
                    body
                ));
            }
            Err(e) => {
                last_err = Some(anyhow!("PD keyspace '{}' query failed: {}", keyspace, e));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no PD endpoint configured for keyspace id lookup")))
}

pub(crate) async fn fetch_region_stats(
    pd_endpoints: &[String],
    encoded_start: &[u8],
    encoded_end: &[u8],
) -> Result<PdRegionStats> {
    let client = crate::worker::build_pd_client()?;
    let path = pd_region_stats_path(encoded_start, encoded_end);
    let mut last_err = None;
    for endpoint in pd_endpoints {
        let url = format!("{}{}", crate::worker::pd_base_url(endpoint), path);
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp
                    .json::<RegionStatsResponse>()
                    .await
                    .context("failed to parse PD Region stats response")?;
                return Ok(PdRegionStats {
                    count: body.count,
                    empty_count: body.empty_count,
                    storage_size_mib: body.storage_size,
                    storage_keys: body.storage_keys,
                });
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = Some(anyhow!(
                    "PD Region stats query returned status={} body={}",
                    status,
                    body
                ));
            }
            Err(e) => {
                last_err = Some(anyhow!("PD Region stats query failed: {}", e));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no PD endpoint configured for Region stats lookup")))
}

pub(crate) async fn fetch_database_region_stats(
    pd_endpoints: &[String],
    keyspace: &str,
    db_id: u64,
) -> Result<DatabasePdRegionStats> {
    let keyspace_id = fetch_keyspace_id(pd_endpoints, keyspace).await?;
    let (logical_start, logical_end, encoded_start, encoded_end) =
        encode_database_region_range(keyspace_id, db_id);
    let stats = fetch_region_stats(pd_endpoints, &encoded_start, &encoded_end).await?;
    Ok(DatabasePdRegionStats {
        keyspace_id,
        logical_start,
        logical_end,
        encoded_start,
        encoded_end,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn region_key_encoding_uses_txn_keyspace_prefix_before_memcomparable() {
        let encoded = encode_region_key_for_txn(0x00A1B2, b"d_test".to_vec());

        // memcomparable encoding stores one group of 8 bytes plus marker 0xff
        // for this short input. The raw input before memcomparable is:
        //   [b'x', 0x00, 0xA1, 0xB2, b'd', b'_', b't', b'e', b's', b't']
        assert_eq!(
            encoded,
            vec![
                b'x', 0x00, 0xA1, 0xB2, b'd', b'_', b't', b'e', 0xff, b's', b't', 0, 0, 0, 0, 0, 0,
                0xf9,
            ]
        );
    }

    #[test]
    fn database_region_range_encodes_logical_db_bounds() {
        let db_id = 42u64;
        let (logical_start, logical_end, encoded_start, encoded_end) =
            encode_database_region_range(7, db_id);
        let (expected_start, expected_end) = encode_database_data_range(db_id);

        assert_eq!(logical_start, expected_start);
        assert_eq!(logical_end, expected_end);
        assert_eq!(encoded_start, encode_region_key_for_txn(7, logical_start));
        assert_eq!(encoded_end, encode_region_key_for_txn(7, logical_end));
    }

    #[test]
    fn pd_query_escape_matches_raw_byte_query_escape_shape() {
        assert_eq!(
            escape_pd_query_bytes(&[b'x', 0, b' ', b'%', 0xff, b'_', b'~']),
            "x%00+%25%FF_~"
        );
    }

    #[test]
    fn region_stats_path_uses_stats_region_without_count() {
        let path = pd_region_stats_path(&[b'x', 0], &[b'y', 0xff]);
        assert_eq!(path, "/pd/api/v1/stats/region?start_key=x%00&end_key=y%FF");
        assert!(!path.contains("count"));
    }

    fn spawn_one_request_server(body: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read request");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            tx.send(request).expect("send captured request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        (addr.to_string(), rx)
    }

    #[tokio::test]
    async fn fetch_region_stats_hits_pd_stats_region_contract() {
        let (endpoint, rx) = spawn_one_request_server(
            r#"{"count":2,"empty_count":1,"storage_size":5,"storage_keys":9}"#,
        );

        let stats = fetch_region_stats(&[endpoint], &[b'x', 0], &[b'y', 0xff])
            .await
            .expect("stats response");

        assert_eq!(stats.count, 2);
        assert_eq!(stats.empty_count, 1);
        assert_eq!(stats.storage_size_mib, 5);
        assert_eq!(stats.total_bytes_estimate(), 5 * MIB);
        assert_eq!(stats.storage_keys, 9);

        let request = rx.recv().expect("captured request");
        let first_line = request.lines().next().unwrap_or_default();
        assert_eq!(
            first_line,
            "GET /pd/api/v1/stats/region?start_key=x%00&end_key=y%FF HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn fetch_region_stats_accepts_successful_zero_region_response() {
        let (endpoint, _rx) = spawn_one_request_server(
            r#"{"count":0,"empty_count":0,"storage_size":0,"storage_keys":0}"#,
        );

        let stats = fetch_region_stats(&[endpoint], b"x", b"y")
            .await
            .expect("zero stats response is valid");

        assert_eq!(stats.count, 0);
        assert_eq!(stats.empty_count, 0);
        assert_eq!(stats.storage_size_mib, 0);
        assert_eq!(stats.total_bytes_estimate(), 0);
        assert_eq!(stats.storage_keys, 0);
    }

    #[tokio::test]
    async fn fetch_keyspace_id_queries_pd_keyspace_metadata() {
        let (endpoint, rx) =
            spawn_one_request_server(r#"{"id":123,"name":"DEFAULT","state":"ENABLED"}"#);

        let id = fetch_keyspace_id(&[endpoint], "default")
            .await
            .expect("keyspace id response");

        assert_eq!(id, 123);
        let request = rx.recv().expect("captured request");
        let first_line = request.lines().next().unwrap_or_default();
        assert_eq!(first_line, "GET /pd/api/v2/keyspaces/DEFAULT HTTP/1.1");
    }
}
