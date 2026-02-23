//! HTTP-based [`AsyncFileReader`] for remote Parquet files.

use bytes::Bytes;
use futures::future::{try_join_all, BoxFuture};
use futures::FutureExt;
use parquet::arrow::async_reader::AsyncFileReader;
use parquet::errors::{ParquetError, Result};
use parquet::file::metadata::ParquetMetaData;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use reqwest::StatusCode;
use std::ops::Range;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 5;

static PARQUET_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub(crate) fn parquet_http_client() -> &'static reqwest::Client {
    PARQUET_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .build()
            .expect("parquet http client init")
    })
}

/// Validate that a URL uses a supported scheme for Parquet import.
pub(crate) fn validate_parquet_url(url: &str) -> anyhow::Result<()> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(anyhow::anyhow!(
            "Only http:// and https:// URLs are supported for Parquet import, got: {}",
            url
        ));
    }
    Ok(())
}

/// Validate URL against SSRF: block private IPs, localhost, link-local.
/// Mirrors the network policy in `src/extensions/http.rs`.
/// In insecure mode (PGTIKV_HTTP_ALLOW_INSECURE=true), all hosts are allowed.
pub(crate) async fn validate_parquet_url_security(url: &str) -> anyhow::Result<()> {
    use std::net::IpAddr;
    use tokio::net::lookup_host;

    fn allow_insecure() -> bool {
        static ALLOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ALLOW.get_or_init(|| {
            std::env::var("PGTIKV_HTTP_ALLOW_INSECURE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    fn is_ip_forbidden(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                    return is_ip_forbidden(IpAddr::V4(v4));
                }
                v6.is_loopback()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
                    || v6.is_unspecified()
            }
        }
    }

    if allow_insecure() {
        return Ok(());
    }

    let parsed =
        reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid Parquet URL: {}", e))?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(anyhow::anyhow!("parquet: userinfo in URL is not allowed"));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("parquet: URL host is missing"))?;
    let host_lower = host.to_ascii_lowercase();

    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
    {
        return Err(anyhow::anyhow!(
            "parquet: access to host '{}' is not allowed",
            host
        ));
    }

    let port = parsed.port_or_known_default().unwrap_or(443);

    let host_for_parse = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
        if is_ip_forbidden(ip) {
            return Err(anyhow::anyhow!(
                "parquet: access to IP address {} is not allowed",
                ip
            ));
        }
        return Ok(());
    }

    let addrs: Vec<std::net::SocketAddr> = lookup_host(format!("{}:{}", host, port))
        .await
        .map_err(|e| anyhow::anyhow!("parquet: DNS lookup failed for {}: {}", host, e))?
        .collect();
    let mut any = false;
    for addr in &addrs {
        any = true;
        if is_ip_forbidden(addr.ip()) {
            return Err(anyhow::anyhow!(
                "parquet: resolved IP {} for host '{}' is not allowed",
                addr.ip(),
                host
            ));
        }
    }
    if !any {
        return Err(anyhow::anyhow!(
            "parquet: DNS lookup for '{}' returned no addresses",
            host
        ));
    }

    Ok(())
}

pub(crate) struct HttpParquetReader {
    client: reqwest::Client,
    url: String,
    file_size: u64,
}

impl HttpParquetReader {
    pub(crate) fn new(client: reqwest::Client, url: String, file_size: u64) -> Self {
        Self {
            client,
            url,
            file_size,
        }
    }
}

impl AsyncFileReader for HttpParquetReader {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, Result<Bytes>> {
        let url = self.url.clone();
        let client = self.client.clone();
        let range_hdr = if range.end > 0 {
            format!("bytes={}-{}", range.start, range.end - 1)
        } else {
            format!("bytes={}-{}", range.start, range.start)
        };

        async move {
            let resp = client
                .get(&url)
                .header(RANGE, &range_hdr)
                .send()
                .await
                .map_err(|e| {
                    ParquetError::General(format!("HTTP range request failed for {url}: {e}"))
                })?;

            let status = resp.status();
            if status != StatusCode::PARTIAL_CONTENT && status != StatusCode::OK {
                return Err(ParquetError::General(format!(
                    "unexpected HTTP status {status} for range request to {url}"
                )));
            }

            let body = resp.bytes().await.map_err(|e| {
                ParquetError::General(format!("failed to read HTTP response body from {url}: {e}"))
            })?;

            // Some servers ignore Range and return 200 with the full file
            if status == StatusCode::OK {
                let start = range.start as usize;
                let end = range.end as usize;
                if end > body.len() {
                    return Err(ParquetError::General(format!(
                        "requested byte range {start}..{end} exceeds response size {}",
                        body.len()
                    )));
                }
                Ok(body.slice(start..end))
            } else {
                Ok(body)
            }
        }
        .boxed()
    }

    fn get_byte_ranges(&mut self, ranges: Vec<Range<u64>>) -> BoxFuture<'_, Result<Vec<Bytes>>> {
        let url = self.url.clone();
        let client = self.client.clone();

        async move {
            let futs = ranges.into_iter().map(|range| {
                let url = url.clone();
                let client = client.clone();
                let range_hdr = format!("bytes={}-{}", range.start, range.end - 1);

                async move {
                    let resp = client
                        .get(&url)
                        .header(RANGE, &range_hdr)
                        .send()
                        .await
                        .map_err(|e| {
                            ParquetError::General(format!(
                                "HTTP range request failed for {url}: {e}"
                            ))
                        })?;

                    let status = resp.status();
                    if status != StatusCode::PARTIAL_CONTENT && status != StatusCode::OK {
                        return Err(ParquetError::General(format!(
                            "unexpected HTTP status {status} for range request to {url}"
                        )));
                    }

                    let body = resp.bytes().await.map_err(|e| {
                        ParquetError::General(format!(
                            "failed to read HTTP response body from {url}: {e}"
                        ))
                    })?;

                    if status == StatusCode::OK {
                        let start = range.start as usize;
                        let end = range.end as usize;
                        if end > body.len() {
                            return Err(ParquetError::General(format!(
                                "requested byte range {start}..{end} exceeds response size {}",
                                body.len()
                            )));
                        }
                        Ok(body.slice(start..end))
                    } else {
                        Ok(body)
                    }
                }
            });

            try_join_all(futs).await
        }
        .boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        _options: Option<&'a parquet::arrow::arrow_reader::ArrowReaderOptions>,
    ) -> BoxFuture<'a, Result<Arc<ParquetMetaData>>> {
        async move {
            let file_size = self.file_size;
            let metadata = parquet::file::metadata::ParquetMetaDataReader::new()
                .load_and_finish(&mut *self, file_size)
                .await?;
            Ok(Arc::new(metadata))
        }
        .boxed()
    }
}

pub(crate) async fn fetch_parquet_file_size(client: &reqwest::Client, url: &str) -> Result<u64> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ParquetError::General(format!(
            "unsupported URL scheme: only http:// and https:// are supported, got: {url}"
        )));
    }

    let resp = client.head(url).send().await.map_err(|e| {
        if e.is_connect() || e.is_timeout() {
            ParquetError::General(format!("Cannot connect to Parquet file host: {url}: {e}"))
        } else {
            ParquetError::General(format!("Failed to fetch Parquet file {url}: {e}"))
        }
    })?;

    let status = resp.status();
    if !status.is_success() {
        let msg = match status {
            StatusCode::NOT_FOUND => format!("Parquet file not found: {url} (HTTP 404)"),
            StatusCode::FORBIDDEN => format!("Access denied for Parquet file: {url} (HTTP 403)"),
            _ => format!("Failed to fetch Parquet file {url}: HTTP {status}"),
        };
        return Err(ParquetError::General(msg));
    }

    resp.headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| {
            ParquetError::General(format!(
                "missing or invalid Content-Length header for {url}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_file_size_rejects_ftp_scheme() {
        let client = parquet_http_client().clone();
        let err = fetch_parquet_file_size(&client, "ftp://example.com/file.parquet")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported URL scheme"), "got: {msg}");
    }

    #[tokio::test]
    async fn fetch_file_size_rejects_file_scheme() {
        let client = parquet_http_client().clone();
        let err = fetch_parquet_file_size(&client, "file:///tmp/data.parquet")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported URL scheme"), "got: {msg}");
    }

    #[tokio::test]
    async fn fetch_file_size_accepts_http() {
        let client = parquet_http_client().clone();
        let err = fetch_parquet_file_size(&client, "http://nonexistent.invalid/data.parquet")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("unsupported URL scheme"), "got: {msg}");
    }

    #[tokio::test]
    async fn fetch_file_size_accepts_https() {
        let client = parquet_http_client().clone();
        let err = fetch_parquet_file_size(&client, "https://nonexistent.invalid/data.parquet")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("unsupported URL scheme"), "got: {msg}");
    }

    #[test]
    fn range_header_formatting() {
        let range: Range<u64> = 100..200;
        let hdr = format!("bytes={}-{}", range.start, range.end - 1);
        assert_eq!(hdr, "bytes=100-199");
    }

    #[test]
    fn range_header_single_byte() {
        let range: Range<u64> = 42..43;
        let hdr = format!("bytes={}-{}", range.start, range.end - 1);
        assert_eq!(hdr, "bytes=42-42");
    }

    #[test]
    fn range_header_large_offset() {
        let range: Range<u64> = 1_000_000_000..1_000_001_000;
        let hdr = format!("bytes={}-{}", range.start, range.end - 1);
        assert_eq!(hdr, "bytes=1000000000-1000000999");
    }

    #[test]
    fn client_singleton_returns_same_instance() {
        let c1 = parquet_http_client() as *const reqwest::Client;
        let c2 = parquet_http_client() as *const reqwest::Client;
        assert_eq!(c1, c2);
    }

    #[test]
    fn http_parquet_reader_stores_fields() {
        let client = parquet_http_client().clone();
        let reader = HttpParquetReader::new(
            client,
            "https://example.com/test.parquet".to_string(),
            12345,
        );
        assert_eq!(reader.url, "https://example.com/test.parquet");
        assert_eq!(reader.file_size, 12345);
    }
}
