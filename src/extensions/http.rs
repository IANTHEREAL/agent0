use crate::extensions::context;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::{Client, Method, Url};
use serde::Serialize;
use serde::ser::{SerializeSeq, Serializer};
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::net::lookup_host;
use tokio::sync::Semaphore;

/// Check if insecure HTTP (non-HTTPS) requests are allowed.
/// Controlled by `PGTIKV_HTTP_ALLOW_INSECURE` environment variable.
/// Default: false (only HTTPS allowed).
fn allow_insecure_http() -> bool {
    static ALLOW_INSECURE: OnceLock<bool> = OnceLock::new();
    *ALLOW_INSECURE.get_or_init(|| {
        std::env::var("PGTIKV_HTTP_ALLOW_INSECURE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

const MAX_REQUESTS_PER_STATEMENT: u32 = 5;
const MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE: usize = 20;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);
const TIMEOUT: Duration = Duration::from_millis(5000);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 3;

pub(crate) enum HttpTableFunctionCall {
    Get { url: String },
    Head { url: String },
    Delete { url: String },
    Post {
        url: String,
        body: String,
        content_type: String,
    },
    Put {
        url: String,
        body: String,
        content_type: String,
    },
}

struct TenantLimiters {
    by_tenant: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl TenantLimiters {
    fn semaphore(&self, tenant: &str) -> Arc<Semaphore> {
        let mut guard = self.by_tenant.lock().expect("http tenant semaphore lock");
        if let Some(existing) = guard.get(tenant) {
            return existing.clone();
        }
        let sem = Arc::new(Semaphore::new(MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE));
        guard.insert(tenant.to_string(), sem.clone());
        sem
    }
}

static LIMITERS: OnceLock<TenantLimiters> = OnceLock::new();
static CLIENT: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client init")
    })
}

fn limiters() -> &'static TenantLimiters {
    LIMITERS.get_or_init(|| TenantLimiters {
        by_tenant: Mutex::new(HashMap::new()),
    })
}

fn http_response_schema(name: &str) -> TableSchema {
    TableSchema {
        table_id: 0,
        name: name.to_string(),
        columns: vec![
            ColumnDef {
                name: "status".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "content_type".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "headers".to_string(),
                data_type: DataType::Jsonb,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
            ColumnDef {
                name: "content".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            },
        ],
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
    }
}

#[derive(Serialize)]
struct HeaderEntry<'a> {
    field: &'a str,
    value: Cow<'a, str>,
}

fn header_value_to_cow(value: &reqwest::header::HeaderValue) -> Cow<'_, str> {
    match value.to_str() {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => String::from_utf8_lossy(value.as_bytes()),
    }
}

fn headers_to_jsonb(headers: &reqwest::header::HeaderMap) -> Result<String> {
    let mut out = Vec::new();
    {
        let mut ser = serde_json::Serializer::new(&mut out);
        let mut seq = ser.serialize_seq(None)?;
        for (name, value) in headers.iter() {
            let entry = HeaderEntry {
                field: name.as_str(),
                value: header_value_to_cow(value),
            };
            seq.serialize_element(&entry)?;
        }
        seq.end()?;
    }
    Ok(String::from_utf8(out).expect("serde_json wrote valid utf8"))
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

async fn validate_url(url: &Url) -> Result<()> {
    let scheme = url.scheme();
    let is_https = scheme == "https";
    let is_http = scheme == "http";

    if !is_https && !is_http {
        return Err(anyhow!("http: only http and https schemes are allowed"));
    }

    if is_http && !allow_insecure_http() {
        return Err(anyhow!(
            "http: insecure http requests are disabled (set PGTIKV_HTTP_ALLOW_INSECURE=true to enable)"
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!("http: userinfo in url is not allowed"));
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("http: url port is missing"))?;

    let default_port = if is_https { 443 } else { 80 };
    if port != default_port {
        return Err(anyhow!(
            "http: only port {} is allowed for {} scheme",
            default_port,
            scheme
        ));
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("http: url host is missing"))?;
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
    {
        return Err(anyhow!("http: host is not allowed"));
    }

    let host_for_parse = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
        if is_ip_forbidden(ip) {
            return Err(anyhow!("http: ip is not allowed"));
        }
        return Ok(());
    }

    let addrs = lookup_host((host, port))
        .await
        .map_err(|e| anyhow!("http: dns lookup failed: {}", e))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if is_ip_forbidden(addr.ip()) {
            return Err(anyhow!("http: resolved ip is not allowed"));
        }
    }
    if !any {
        return Err(anyhow!("http: dns lookup returned no addresses"));
    }

    Ok(())
}

fn redirect_target(base: &Url, location: &str) -> Result<Url> {
    let loc = location.trim();
    if loc.is_empty() {
        return Err(anyhow!("http: empty redirect location"));
    }
    Ok(base.join(loc)?)
}

async fn read_response(mut resp: reqwest::Response) -> Result<(i32, Option<String>, String, String)> {
    let status = resp.status().as_u16() as i32;
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let headers_json = headers_to_jsonb(resp.headers())?;

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow!("http: response stream error: {}", e))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(anyhow!("http: response too large (max {} bytes)", MAX_RESPONSE_BYTES));
        }
        body.extend_from_slice(&chunk);
    }

    let content = String::from_utf8(body).map_err(|_| anyhow!("http: response is not valid UTF-8"))?;
    Ok((status, content_type, headers_json, content))
}

async fn execute_request(
    tenant: &str,
    mut method: Method,
    mut url: Url,
    mut body: Option<bytes::Bytes>,
    mut content_type: Option<String>,
    follow_redirects: bool,
) -> Result<(i32, Option<String>, String, String)> {
    context::try_consume_http_request(MAX_REQUESTS_PER_STATEMENT)?;

    let semaphore = limiters().semaphore(tenant);
    let _permit = semaphore.acquire().await.map_err(|_| anyhow!("http: limiter closed"))?;

    for redirect_count in 0..=MAX_REDIRECTS {
        validate_url(&url).await?;

        let mut req = client().request(method.clone(), url.clone());
        if let Some(ref ct) = content_type {
            req = req.header(CONTENT_TYPE, ct);
        }
        if let Some(ref b) = body {
            if b.len() > MAX_REQUEST_BYTES {
                return Err(anyhow!(
                    "http: request body too large (max {} bytes)",
                    MAX_REQUEST_BYTES
                ));
            }
            req = req.body(b.clone());
        }

        let resp = req.send().await.map_err(|e| anyhow!("http: request failed: {}", e))?;

        if follow_redirects && resp.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(anyhow!("http: too many redirects (max {})", MAX_REDIRECTS));
            }
            let Some(loc) = resp.headers().get(LOCATION) else {
                return Err(anyhow!("http: redirect missing location header"));
            };
            let loc = loc
                .to_str()
                .map_err(|_| anyhow!("http: invalid redirect location"))?;
            let next_url = redirect_target(&url, loc)?;

            match resp.status().as_u16() {
                301 | 302 | 303 => {
                    method = Method::GET;
                    body = None;
                    content_type = None;
                }
                307 | 308 => {}
                _ => {}
            }

            url = next_url;
            continue;
        }

        return read_response(resp).await;
    }

    Err(anyhow!("http: redirect loop"))
}

pub(crate) async fn execute_table_function(
    tenant: &str,
    call: HttpTableFunctionCall,
) -> Result<(TableSchema, Vec<Row>)> {
    if !context::is_superuser() {
        return Err(anyhow!("permission denied for extension \"http\""));
    }

    let (schema_name, method, url, body, content_type, follow_redirects) = match call {
        HttpTableFunctionCall::Get { url } => ("http_get", Method::GET, url, None, None, true),
        HttpTableFunctionCall::Head { url } => ("http_head", Method::HEAD, url, None, None, false),
        HttpTableFunctionCall::Delete { url } => ("http_delete", Method::DELETE, url, None, None, true),
        HttpTableFunctionCall::Post {
            url,
            body,
            content_type,
        } => (
            "http_post",
            Method::POST,
            url,
            Some(bytes::Bytes::from(body.into_bytes())),
            Some(content_type),
            true,
        ),
        HttpTableFunctionCall::Put {
            url,
            body,
            content_type,
        } => (
            "http_put",
            Method::PUT,
            url,
            Some(bytes::Bytes::from(body.into_bytes())),
            Some(content_type),
            true,
        ),
    };

    let url = Url::parse(&url).map_err(|e| anyhow!("http: invalid url: {}", e))?;

    let (status, content_type_out, headers_json, content) = execute_request(
        tenant,
        method,
        url,
        body,
        content_type,
        follow_redirects,
    )
    .await?;

    let schema = http_response_schema(schema_name);
    let row = Row::new(vec![
        Value::Int32(status),
        content_type_out.map(Value::Text).unwrap_or(Value::Null),
        Value::Jsonb(headers_json),
        Value::Text(content),
    ]);

    Ok((schema, vec![row]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ip_forbidden() {
        assert!(is_ip_forbidden(IpAddr::from([127, 0, 0, 1])));
        assert!(is_ip_forbidden(IpAddr::from([10, 0, 0, 1])));
        assert!(is_ip_forbidden(IpAddr::from([169, 254, 1, 2])));
        assert!(is_ip_forbidden(
            "::ffff:127.0.0.1".parse::<IpAddr>().unwrap()
        ));
    }

    #[tokio::test]
    async fn test_validate_url_https_allowed() {
        let url = Url::parse("https://example.com/api").unwrap();
        assert!(validate_url(&url).await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_url_https_port_443_only() {
        let url = Url::parse("https://example.com:8443/api").unwrap();
        let result = validate_url(&url).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("only port 443"));
    }

    #[tokio::test]
    async fn test_validate_url_http_blocked_by_default() {
        let url = Url::parse("http://example.com/api").unwrap();
        let result = validate_url(&url).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("insecure http requests are disabled"));
    }

    #[tokio::test]
    async fn test_validate_url_invalid_scheme() {
        let url = Url::parse("ftp://example.com/file").unwrap();
        let result = validate_url(&url).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only http and https schemes are allowed"));
    }

    #[tokio::test]
    async fn test_validate_url_userinfo_not_allowed() {
        let url = Url::parse("https://user:pass@example.com/api").unwrap();
        let result = validate_url(&url).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("userinfo"));
    }

    #[tokio::test]
    async fn test_validate_url_localhost_blocked() {
        let url = Url::parse("https://localhost/api").unwrap();
        let result = validate_url(&url).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("host is not allowed"));
    }
}
