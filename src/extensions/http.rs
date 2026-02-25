use crate::extensions::context;
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::{Client, Method, Url};
use serde::ser::{SerializeSeq, Serializer};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::net::lookup_host;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Check if insecure HTTP (non-HTTPS) requests are allowed.
/// Controlled by `DB9_HTTP_ALLOW_INSECURE` environment variable.
/// Default: false (HTTPS-only, locked down ports/hosts).
fn allow_insecure_http() -> bool {
    static ALLOW_INSECURE: OnceLock<bool> = OnceLock::new();
    *ALLOW_INSECURE.get_or_init(|| {
        std::env::var("DB9_HTTP_ALLOW_INSECURE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

const MAX_REQUESTS_PER_STATEMENT: u32 = 100;
const MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE: usize = 20;
const RESERVED_FOR_INTERACTIVE: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);
const TIMEOUT: Duration = Duration::from_millis(5000);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 3;

pub(crate) enum HttpTableFunctionCall {
    /// http(method, uri, headers_jsonb, content_type, content) — universal function
    /// Headers format: JSON array of {"field":"...", "value":"..."} objects (pgsql-http compatible)
    Universal {
        method: String,
        url: String,
        headers: Option<String>,
        content_type: Option<String>,
        body: Option<String>,
    },
    Get {
        url: String,
        headers: Option<String>,
    },
    Head {
        url: String,
        headers: Option<String>,
    },
    Delete {
        url: String,
        headers: Option<String>,
    },
    Post {
        url: String,
        body: String,
        content_type: String,
        headers: Option<String>,
    },
    Put {
        url: String,
        body: String,
        content_type: String,
        headers: Option<String>,
    },
}

struct TenantQuota {
    /// Reserved exclusively for interactive requests
    interactive_pool: Arc<Semaphore>,
    /// Shared pool usable by both interactive and cron
    shared_pool: Arc<Semaphore>,
}

impl TenantQuota {
    fn new() -> Self {
        let shared_capacity = MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE - RESERVED_FOR_INTERACTIVE;
        Self {
            interactive_pool: Arc::new(Semaphore::new(RESERVED_FOR_INTERACTIVE)),
            shared_pool: Arc::new(Semaphore::new(shared_capacity)),
        }
    }
}

struct TenantLimiters {
    by_tenant: Mutex<HashMap<String, Arc<TenantQuota>>>,
}

impl TenantLimiters {
    fn quota(&self, tenant: &str) -> Arc<TenantQuota> {
        let mut guard = self.by_tenant.lock().expect("http tenant limiter lock");
        if let Some(existing) = guard.get(tenant) {
            return existing.clone();
        }
        let quota = Arc::new(TenantQuota::new());
        guard.insert(tenant.to_string(), quota.clone());
        quota
    }
}

static LIMITERS: OnceLock<TenantLimiters> = OnceLock::new();
static CLIENT: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    CLIENT.get_or_init(|| {
        Client::builder()
            .no_proxy()
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
                collation: None,
            },
            ColumnDef {
                name: "content_type".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            ColumnDef {
                name: "headers".to_string(),
                data_type: DataType::Jsonb,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
            ColumnDef {
                name: "content".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            },
        ],
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

pub(crate) fn table_function_schema(func_name: &str) -> Option<TableSchema> {
    let name = func_name.trim().to_ascii_lowercase();
    match name.as_str() {
        "http" | "http_get" | "http_head" | "http_delete" | "http_post" | "http_put"
        | "http_patch" => Some(http_response_schema(&name)),
        _ => None,
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
    validate_url_with_policy(url, allow_insecure_http()).await
}

async fn validate_url_with_policy(url: &Url, allow_insecure: bool) -> Result<()> {
    let scheme = url.scheme();
    let is_https = scheme == "https";
    let is_http = scheme == "http";

    if !is_https && !is_http {
        return Err(anyhow!("http: only http and https schemes are allowed"));
    }

    if is_http && !allow_insecure {
        return Err(anyhow!(
            "http: insecure http requests are disabled (set DB9_HTTP_ALLOW_INSECURE=true to enable)"
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!("http: userinfo in url is not allowed"));
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("http: url port is missing"))?;

    let default_port = if is_https { 443 } else { 80 };
    if port != default_port && !allow_insecure {
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

    // In insecure mode, allow localhost and private IPs for local development.
    if allow_insecure {
        return Ok(());
    }

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

async fn read_response(
    mut resp: reqwest::Response,
) -> Result<(i32, Option<String>, String, String)> {
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
            return Err(anyhow!(
                "http: response too large (max {} bytes)",
                MAX_RESPONSE_BYTES
            ));
        }
        body.extend_from_slice(&chunk);
    }

    let content =
        String::from_utf8(body).map_err(|_| anyhow!("http: response is not valid UTF-8"))?;
    Ok((status, content_type, headers_json, content))
}

fn parse_custom_headers(
    headers_json: &str,
) -> Result<Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>> {
    let parsed: serde_json::Value = serde_json::from_str(headers_json)
        .map_err(|e| anyhow!("http: invalid headers JSON: {}", e))?;
    match &parsed {
        serde_json::Value::Array(arr) => {
            let mut result = Vec::with_capacity(arr.len());
            for item in arr {
                let obj = item.as_object().ok_or_else(|| {
                    anyhow!("http: each header must be an object with \"field\" and \"value\"")
                })?;
                let field = obj
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("http: header missing \"field\""))?;
                let value = obj
                    .get("value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("http: header missing \"value\""))?;
                let header_name = reqwest::header::HeaderName::from_bytes(field.as_bytes())
                    .map_err(|_| anyhow!("http: invalid header name: {}", field))?;
                let header_value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| anyhow!("http: invalid header value for {}", field))?;
                result.push((header_name, header_value));
            }
            Ok(result)
        }
        serde_json::Value::Object(obj) => {
            let mut result = Vec::with_capacity(obj.len());
            for (key, value) in obj {
                let header_name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|_| anyhow!("http: invalid header name: {}", key))?;
                let val_str = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let header_value = reqwest::header::HeaderValue::from_str(&val_str)
                    .map_err(|_| anyhow!("http: invalid header value for {}", key))?;
                result.push((header_name, header_value));
            }
            Ok(result)
        }
        _ => Err(anyhow!(
            "http: headers must be a JSON array of {{\"field\":...,\"value\":...}} or a JSON object"
        )),
    }
}

async fn execute_request(
    tenant: &str,
    mut method: Method,
    mut url: Url,
    mut body: Option<bytes::Bytes>,
    mut content_type: Option<String>,
    custom_headers: Option<&str>,
    follow_redirects: bool,
) -> Result<(i32, Option<String>, String, String)> {
    context::try_consume_http_request(MAX_REQUESTS_PER_STATEMENT)?;

    let parsed_headers = match custom_headers {
        Some(h) => Some(parse_custom_headers(h)?),
        None => None,
    };

    let quota = limiters().quota(tenant);
    let _permit = acquire_quota_permit(quota, context::execution_kind()).await?;

    for redirect_count in 0..=MAX_REDIRECTS {
        validate_url(&url).await?;

        let mut req = client().request(method.clone(), url.clone());
        if let Some(ref ct) = content_type {
            req = req.header(CONTENT_TYPE, ct);
        }
        if let Some(ref hdrs) = parsed_headers {
            for (name, value) in hdrs {
                req = req.header(name.clone(), value.clone());
            }
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

        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("http: request failed: {}", e))?;

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

async fn acquire_quota_permit(
    quota: Arc<TenantQuota>,
    execution_kind: context::ExecutionKind,
) -> Result<OwnedSemaphorePermit> {
    match execution_kind {
        context::ExecutionKind::Cron => quota
            .shared_pool
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("http: limiter closed")),
        context::ExecutionKind::Interactive => {
            // Fast path: avoid await if either pool currently has free capacity.
            if let Ok(permit) = quota.interactive_pool.clone().try_acquire_owned() {
                return Ok(permit);
            }
            if let Ok(permit) = quota.shared_pool.clone().try_acquire_owned() {
                return Ok(permit);
            }

            // Wait on both pools so interactive traffic can wake when reserved capacity frees up.
            tokio::select! {
                permit = quota.interactive_pool.clone().acquire_owned() => {
                    permit.map_err(|_| anyhow!("http: limiter closed"))
                }
                permit = quota.shared_pool.clone().acquire_owned() => {
                    permit.map_err(|_| anyhow!("http: limiter closed"))
                }
            }
        }
    }
}

pub(crate) async fn execute_table_function(
    tenant: &str,
    call: HttpTableFunctionCall,
) -> Result<(TableSchema, Vec<Row>)> {
    if !context::is_superuser() {
        return Err(SqlError::PermissionDenied {
            object_type: "extension".into(),
            object_name: "\"http\"".into(),
        }
        .into());
    }

    let (schema_name, method, url, body, content_type, custom_headers, follow_redirects) =
        match call {
            HttpTableFunctionCall::Get { url, headers } => {
                ("http_get", Method::GET, url, None, None, headers, true)
            }
            HttpTableFunctionCall::Head { url, headers } => {
                ("http_head", Method::HEAD, url, None, None, headers, false)
            }
            HttpTableFunctionCall::Delete { url, headers } => (
                "http_delete",
                Method::DELETE,
                url,
                None,
                None,
                headers,
                true,
            ),
            HttpTableFunctionCall::Post {
                url,
                body,
                content_type,
                headers,
            } => (
                "http_post",
                Method::POST,
                url,
                Some(bytes::Bytes::from(body.into_bytes())),
                Some(content_type),
                headers,
                true,
            ),
            HttpTableFunctionCall::Put {
                url,
                body,
                content_type,
                headers,
            } => (
                "http_put",
                Method::PUT,
                url,
                Some(bytes::Bytes::from(body.into_bytes())),
                Some(content_type),
                headers,
                true,
            ),
            HttpTableFunctionCall::Universal {
                method,
                url,
                headers,
                content_type,
                body,
            } => {
                let m = match method.to_ascii_uppercase().as_str() {
                    "GET" => Method::GET,
                    "POST" => Method::POST,
                    "PUT" => Method::PUT,
                    "DELETE" => Method::DELETE,
                    "HEAD" => Method::HEAD,
                    "PATCH" => Method::PATCH,
                    _ => return Err(anyhow!("http: unsupported method: {}", method)),
                };
                let follow = !matches!(m, Method::HEAD);
                (
                    "http",
                    m,
                    url,
                    body.map(|b| bytes::Bytes::from(b.into_bytes())),
                    content_type,
                    headers,
                    follow,
                )
            }
        };

    let url = Url::parse(&url).map_err(|e| anyhow!("http: invalid url: {}", e))?;

    let (status, content_type_out, headers_json, content) = execute_request(
        tenant,
        method,
        url,
        body,
        content_type,
        custom_headers.as_deref(),
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
        assert!(validate_url_with_policy(&url, false).await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_url_https_port_443_only() {
        let url = Url::parse("https://example.com:8443/api").unwrap();
        let result = validate_url_with_policy(&url, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("only port 443"));
    }

    #[tokio::test]
    async fn test_validate_url_http_blocked_by_default() {
        let url = Url::parse("http://example.com/api").unwrap();
        let result = validate_url_with_policy(&url, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("insecure http requests are disabled"));
    }

    #[tokio::test]
    async fn test_validate_url_invalid_scheme() {
        let url = Url::parse("ftp://example.com/file").unwrap();
        let result = validate_url_with_policy(&url, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only http and https schemes are allowed"));
    }

    #[tokio::test]
    async fn test_validate_url_userinfo_not_allowed() {
        let url = Url::parse("https://user:pass@example.com/api").unwrap();
        let result = validate_url_with_policy(&url, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("userinfo"));
    }

    #[tokio::test]
    async fn test_validate_url_localhost_blocked() {
        let url = Url::parse("https://localhost/api").unwrap();
        let result = validate_url_with_policy(&url, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("host is not allowed"));
    }

    #[tokio::test]
    async fn test_validate_url_http_allowed_in_insecure_mode() {
        let url = Url::parse("http://example.com:8080/api").unwrap();
        assert!(validate_url_with_policy(&url, true).await.is_ok());
    }

    #[test]
    fn test_parse_custom_headers_array_format() {
        let json = r#"[{"field":"Authorization","value":"Bearer sk-test"},{"field":"X-Custom","value":"foo"}]"#;
        let headers = parse_custom_headers(json).unwrap();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0.as_str(), "authorization");
        assert_eq!(headers[0].1.to_str().unwrap(), "Bearer sk-test");
        assert_eq!(headers[1].0.as_str(), "x-custom");
        assert_eq!(headers[1].1.to_str().unwrap(), "foo");
    }

    #[test]
    fn test_parse_custom_headers_object_format() {
        let json = r#"{"Authorization":"Bearer sk-test","X-Custom":"bar"}"#;
        let headers = parse_custom_headers(json).unwrap();
        assert_eq!(headers.len(), 2);
        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"authorization"));
        assert!(names.contains(&"x-custom"));
    }

    #[test]
    fn test_parse_custom_headers_empty_array() {
        let headers = parse_custom_headers("[]").unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn test_parse_custom_headers_empty_object() {
        let headers = parse_custom_headers("{}").unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn test_parse_custom_headers_invalid_json() {
        assert!(parse_custom_headers("not json").is_err());
    }

    #[test]
    fn test_parse_custom_headers_missing_field() {
        let json = r#"[{"value":"Bearer sk-test"}]"#;
        assert!(parse_custom_headers(json).is_err());
    }

    #[test]
    fn test_parse_custom_headers_missing_value() {
        let json = r#"[{"field":"Authorization"}]"#;
        assert!(parse_custom_headers(json).is_err());
    }

    #[test]
    fn test_parse_custom_headers_scalar_rejected() {
        assert!(parse_custom_headers(r#""just a string""#).is_err());
    }

    #[tokio::test]
    async fn test_quota_interactive_gets_reserved_pool() {
        let quota = TenantQuota::new();

        // Interactive should be able to acquire from reserved pool
        let permits: Vec<_> = (0..RESERVED_FOR_INTERACTIVE)
            .map(|_| quota.interactive_pool.clone().try_acquire_owned().unwrap())
            .collect();

        // Reserved pool exhausted
        assert!(quota.interactive_pool.clone().try_acquire_owned().is_err());

        // But shared pool should still have capacity
        let _shared_permit = quota.shared_pool.clone().try_acquire_owned().unwrap();

        drop(permits);
    }

    #[tokio::test]
    async fn test_quota_cron_cannot_starve_interactive() {
        let quota = TenantQuota::new();
        let shared_capacity = MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE - RESERVED_FOR_INTERACTIVE;

        // Cron fills the shared pool
        let _cron_permits: Vec<_> = (0..shared_capacity)
            .map(|_| quota.shared_pool.clone().try_acquire_owned().unwrap())
            .collect();

        // Shared pool is now exhausted
        assert!(quota.shared_pool.clone().try_acquire_owned().is_err());

        // But interactive can still acquire from its reserved pool
        let _interactive_permit = quota.interactive_pool.clone().try_acquire_owned().unwrap();
    }

    #[tokio::test]
    async fn test_quota_interactive_waits_on_reserved_when_shared_is_saturated() {
        let quota = Arc::new(TenantQuota::new());
        let shared_capacity = MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE - RESERVED_FOR_INTERACTIVE;

        let _shared_permits: Vec<_> = (0..shared_capacity)
            .map(|_| quota.shared_pool.clone().try_acquire_owned().unwrap())
            .collect();
        let mut interactive_permits: Vec<_> = (0..RESERVED_FOR_INTERACTIVE)
            .map(|_| quota.interactive_pool.clone().try_acquire_owned().unwrap())
            .collect();

        let waiter_quota = quota.clone();
        let waiter = tokio::spawn(async move {
            acquire_quota_permit(waiter_quota, context::ExecutionKind::Interactive)
                .await
                .expect("interactive acquire should eventually succeed")
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        drop(interactive_permits.pop());

        let permit = tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("interactive waiter should wake after reserved permit is released")
            .expect("waiter task should not panic");
        drop(permit);
    }

    #[test]
    fn test_quota_constants() {
        assert!(
            RESERVED_FOR_INTERACTIVE > 0,
            "Must reserve some capacity for interactive"
        );
        assert!(
            RESERVED_FOR_INTERACTIVE < MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE,
            "Reserved pool cannot be the entire capacity"
        );
        assert_eq!(
            RESERVED_FOR_INTERACTIVE
                + (MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE - RESERVED_FOR_INTERACTIVE),
            MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE,
            "Pools must sum to total capacity"
        );
    }
}
