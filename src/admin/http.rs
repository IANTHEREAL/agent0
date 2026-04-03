//! Break-glass admin HTTP server for emergency session management.
//!
//! This is the **break-glass** surface only (Option C secondary path).
//! It is NOT the primary admin management path — that goes through the
//! backend proxy, which is a separate surface with its own auth model.
//!
//! Design constraints:
//! - Localhost-only (hardcoded `127.0.0.1` — not configurable)
//! - Disabled unless explicitly configured (`BREAK_GLASS_PORT` > 0)
//! - Requires `BREAK_GLASS_SECRET` — fail-closed
//! - All write operations audit-logged with actor `"break-glass"`
//! - Uses raw TCP + manual HTTP parsing — no framework dependencies
//!
//! Endpoints:
//! - `GET  /admin/sessions`                — list sessions (filtered, paginated)
//! - `POST /admin/sessions/:id/cancel`     — cancel current query (57014)
//! - `POST /admin/sessions/:id/terminate`  — kill connection
//! - `POST /admin/tenants/:id/terminate-all` — bulk terminate by tenant

use super::control::{AdminControlService, ControlError};
use super::session_registry::{SessionFilter, SessionState};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

const MAX_HEADER_SIZE: usize = 8192;
const MAX_BODY_SIZE: usize = 4096;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_CONCURRENT_CONNECTIONS: usize = 16;
const MAX_LIST_LIMIT: usize = 1000;
/// Minimum length for BREAK_GLASS_SECRET to prevent trivially guessable secrets.
pub const MIN_SECRET_LENGTH: usize = 16;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the break-glass admin HTTP accept loop. Call from `tokio::spawn`.
pub async fn start_break_glass_server(listener: TcpListener, secret: String) {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        warn!("break-glass connection limit reached, rejecting {}", peer);
                        // Drop the stream immediately — no permit available.
                        drop(stream);
                        continue;
                    }
                };
                let s = secret.clone();
                tokio::spawn(async move {
                    let _permit = permit; // held until task completes
                    if let Err(e) =
                        tokio::time::timeout(REQUEST_TIMEOUT, handle_connection(stream, &s, peer))
                            .await
                    {
                        debug!("break-glass connection from {} timed out: {}", peer, e);
                    }
                });
            }
            Err(e) => {
                warn!("break-glass accept error: {}", e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(mut stream: TcpStream, admin_secret: &str, peer: std::net::SocketAddr) {
    let req = match read_request(&mut stream).await {
        Ok(r) => r,
        Err(status) => {
            let msg = match status {
                413 => "payload too large",
                _ => "bad request",
            };
            let _ = write_response(&mut stream, status, &error_json(status, msg)).await;
            return;
        }
    };

    // Auth check.
    if !check_auth(&req, admin_secret) {
        warn!(
            "break-glass auth failure from {} — {} {}",
            peer, req.method, req.path
        );
        let _ = write_response(&mut stream, 401, &error_json(401, "unauthorized")).await;
        return;
    }

    let (status, body) = route(&req);
    let _ = write_response(&mut stream, status, &body).await;
}

// ---------------------------------------------------------------------------
// HTTP request parsing
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    query_string: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, u16> {
    let mut buf = vec![0u8; MAX_HEADER_SIZE];
    let mut filled = 0usize;

    // Read until we find \r\n\r\n or fill the buffer.
    let header_end = loop {
        if filled >= MAX_HEADER_SIZE {
            return Err(413);
        }
        let n = stream.read(&mut buf[filled..]).await.map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        filled += n;
        if let Some(pos) = find_header_end(&buf[..filled]) {
            break pos;
        }
    };

    let header_bytes = &buf[..header_end];
    let header_str = std::str::from_utf8(header_bytes).map_err(|_| 400u16)?;
    let mut lines = header_str.lines();

    // Request line.
    let request_line = lines.next().ok_or(400u16)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(400u16)?.to_uppercase();
    let uri = parts.next().ok_or(400u16)?;

    // Parse path and query string.
    let (path, query_string) = match uri.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (uri.to_string(), String::new()),
    };

    // Parse headers.
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }

    // Read body if Content-Length is present.
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if content_length > MAX_BODY_SIZE {
        return Err(413);
    }

    let body_start = header_end + 4; // skip \r\n\r\n
    let mut body = Vec::new();
    if content_length > 0 {
        // Bytes already in the buffer after headers.
        let already_read = filled.saturating_sub(body_start);
        if already_read > 0 {
            let take = already_read.min(content_length);
            body.extend_from_slice(&buf[body_start..body_start + take]);
        }
        // Read remaining body bytes.
        while body.len() < content_length {
            let remaining = content_length - body.len();
            let mut chunk = vec![0u8; remaining.min(4096)];
            let n = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
            if n == 0 {
                return Err(400);
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(content_length);
    }

    Ok(HttpRequest {
        method,
        path,
        query_string,
        headers,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_query_params(qs: &str) -> HashMap<String, String> {
    if qs.is_empty() {
        return HashMap::new();
    }
    qs.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

fn check_auth(req: &HttpRequest, admin_secret: &str) -> bool {
    let Some(auth_header) = req.headers.get("authorization") else {
        return false;
    };
    // RFC 7235: scheme comparison is case-insensitive.
    let token = if let Some(t) = auth_header.strip_prefix("Bearer ") {
        t
    } else if let Some(t) = auth_header.strip_prefix("bearer ") {
        t
    } else if auth_header.len() > 7 && auth_header[..7].eq_ignore_ascii_case("bearer ") {
        &auth_header[7..]
    } else {
        return false;
    };
    constant_time_eq(token.as_bytes(), admin_secret.as_bytes())
}

/// Constant-time comparison that does not leak the length of `b` (the secret).
/// Both inputs are padded to the same length before comparing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff = (a.len() != b.len()) as u8;
    for i in 0..max_len {
        let x = if i < a.len() { a[i] } else { 0 };
        let y = if i < b.len() { b[i] } else { 0 };
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn route(req: &HttpRequest) -> (u16, String) {
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();

    match (req.method.as_str(), segments.as_slice()) {
        // GET /admin/sessions
        ("GET", ["admin", "sessions"]) => handle_list_sessions(req),

        // POST /admin/sessions/:id/cancel
        ("POST", ["admin", "sessions", id, "cancel"]) => match id.parse::<i64>() {
            Ok(conn_id) => handle_cancel(conn_id, req),
            Err(_) => (400, error_json(400, "invalid connection_id")),
        },

        // POST /admin/sessions/:id/terminate
        ("POST", ["admin", "sessions", id, "terminate"]) => match id.parse::<i64>() {
            Ok(conn_id) => handle_terminate(conn_id, req),
            Err(_) => (400, error_json(400, "invalid connection_id")),
        },

        // POST /admin/tenants/:id/terminate-all
        ("POST", ["admin", "tenants", id, "terminate-all"]) => handle_terminate_all(id, req),

        // Method exists but wrong HTTP method
        ("GET", ["admin", "sessions", _, "cancel"])
        | ("GET", ["admin", "sessions", _, "terminate"])
        | ("GET", ["admin", "tenants", _, "terminate-all"])
        | ("POST", ["admin", "sessions"]) => (405, error_json(405, "method not allowed")),

        _ => (404, error_json(404, "not found")),
    }
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

fn handle_list_sessions(req: &HttpRequest) -> (u16, String) {
    let params = parse_query_params(&req.query_string);

    let tenant_id = params.get("tenant_id").cloned();
    let all_tenants = params
        .get("all_tenants")
        .map(|v| v == "true")
        .unwrap_or(false);
    let principal = params.get("principal").cloned();

    // Reject unknown state filter values instead of silently ignoring them.
    let state = match params.get("state") {
        Some(s) => match parse_session_state(s) {
            Some(st) => Some(st),
            None => {
                return (
                    400,
                    error_json(400, &format!("invalid state filter: '{s}'")),
                );
            }
        },
        None => None,
    };

    let min_duration_ms = params.get("min_duration_ms").and_then(|v| v.parse().ok());
    let limit = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100usize)
        .min(MAX_LIST_LIMIT);
    let offset = params
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0usize);

    let filter = SessionFilter {
        tenant_id,
        all_tenants,
        principal,
        state,
        min_duration_ms,
        limit,
        offset,
    };

    let registry = super::global_session_registry();
    let svc = AdminControlService::new(registry);

    match svc.list_sessions(&filter) {
        Ok(resp) => {
            let sessions_json: Vec<serde_json::Value> =
                resp.sessions.iter().map(snapshot_to_json).collect();
            let body = serde_json::json!({
                "sessions": sessions_json,
                "has_more": resp.has_more,
            });
            (200, body.to_string())
        }
        Err(e) => control_error_to_response(e),
    }
}

fn handle_cancel(connection_id: i64, req: &HttpRequest) -> (u16, String) {
    let reason = match extract_reason(&req.body) {
        Ok(r) => r,
        Err(msg) => return (400, error_json(400, &msg)),
    };
    let registry = super::global_session_registry();
    let svc = AdminControlService::new(registry);

    match svc.cancel_query(connection_id, "break-glass", reason.as_deref()) {
        Ok(resp) => {
            let body = serde_json::json!({
                "connection_id": resp.connection_id,
                "result": resp.result,
                "query_was": resp.query_was,
            });
            (200, body.to_string())
        }
        Err(e) => control_error_to_response(e),
    }
}

fn handle_terminate(connection_id: i64, req: &HttpRequest) -> (u16, String) {
    let reason = match extract_reason(&req.body) {
        Ok(r) => r,
        Err(msg) => return (400, error_json(400, &msg)),
    };
    let registry = super::global_session_registry();
    let svc = AdminControlService::new(registry);

    match svc.terminate(connection_id, "break-glass", reason.as_deref()) {
        Ok(resp) => {
            let body = serde_json::json!({
                "connection_id": resp.connection_id,
                "result": resp.result,
                "query_was": resp.query_was,
            });
            (200, body.to_string())
        }
        Err(e) => control_error_to_response(e),
    }
}

fn handle_terminate_all(tenant_id: &str, req: &HttpRequest) -> (u16, String) {
    let reason = match extract_reason(&req.body) {
        Ok(r) => r,
        Err(msg) => return (400, error_json(400, &msg)),
    };
    let registry = super::global_session_registry();
    let svc = AdminControlService::new(registry);

    match svc.terminate_all(tenant_id, "break-glass", reason.as_deref()) {
        Ok(resp) => {
            let body = serde_json::json!({
                "tenant_id": resp.tenant_id,
                "requested": resp.requested,
                "terminated": resp.terminated,
                "already_closed": resp.already_closed,
            });
            (200, body.to_string())
        }
        Err(e) => control_error_to_response(e),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn snapshot_to_json(s: &super::session_registry::SessionSnapshot) -> serde_json::Value {
    serde_json::json!({
        "connection_id": s.connection_id,
        "tenant_id": s.tenant_id,
        "principal": s.principal,
        "database": s.database,
        "peer_addr": s.peer_addr,
        "connected_at_epoch_ms": s.connected_at_epoch_ms,
        "state": s.state.as_str(),
        "current_query": s.current_query,
        "query_start_epoch_ms": s.query_start_epoch_ms,
        "duration_ms": s.duration_ms,
        "query_duration_ms": s.query_duration_ms,
        "server_id": s.server_id,
    })
}

/// Parse optional `reason` from the request body.
/// Returns `Ok(None)` for empty body, `Ok(Some(reason))` for valid JSON with reason,
/// `Ok(None)` for valid JSON without reason field, `Err` for malformed JSON.
fn extract_reason(body: &[u8]) -> Result<Option<String>, String> {
    if body.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    Ok(value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

fn parse_session_state(s: &str) -> Option<SessionState> {
    match s {
        "idle" => Some(SessionState::Idle),
        "active" => Some(SessionState::Active),
        "idle_in_transaction" => Some(SessionState::IdleInTransaction),
        "idle_in_failed_transaction" => Some(SessionState::IdleInFailedTransaction),
        _ => None,
    }
}

fn control_error_to_response(e: ControlError) -> (u16, String) {
    match e {
        ControlError::MissingTenantScope => (400, error_json(400, &e.to_string())),
        ControlError::EmptyTenantId => (400, error_json(400, &e.to_string())),
        ControlError::NotFound => (404, error_json(404, &e.to_string())),
        ControlError::NoActiveQuery => (409, error_json(409, &e.to_string())),
    }
}

fn error_json(status: u16, message: &str) -> String {
    serde_json::json!({
        "error": message,
        "status": status,
    })
    .to_string()
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nDate: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_text(status),
        body.len(),
        date,
        body
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"wrong!"));
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(!constant_time_eq(b"longer", b"short"));
        assert!(constant_time_eq(b"", b""));
        // Different lengths should still compare all bytes (no length leak).
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
    }

    #[test]
    fn parse_query_params_basic() {
        let params = parse_query_params("tenant_id=acme&limit=50&all_tenants=true");
        assert_eq!(params.get("tenant_id").unwrap(), "acme");
        assert_eq!(params.get("limit").unwrap(), "50");
        assert_eq!(params.get("all_tenants").unwrap(), "true");
    }

    #[test]
    fn parse_query_params_empty() {
        let params = parse_query_params("");
        assert!(params.is_empty());
    }

    #[test]
    fn parse_session_state_roundtrip() {
        assert_eq!(parse_session_state("idle"), Some(SessionState::Idle));
        assert_eq!(parse_session_state("active"), Some(SessionState::Active));
        assert_eq!(
            parse_session_state("idle_in_transaction"),
            Some(SessionState::IdleInTransaction)
        );
        assert_eq!(
            parse_session_state("idle_in_failed_transaction"),
            Some(SessionState::IdleInFailedTransaction)
        );
        assert_eq!(parse_session_state("unknown"), None);
    }

    #[test]
    fn find_header_end_found() {
        let input = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\nbody";
        let pos = find_header_end(input);
        assert_eq!(pos, Some(31));
    }

    #[test]
    fn find_header_end_not_found() {
        let input = b"GET / HTTP/1.1\r\nHost: localhost\r\n";
        let pos = find_header_end(input);
        assert_eq!(pos, None);
    }

    #[test]
    fn extract_reason_from_json() {
        let body = br#"{"reason": "slow query"}"#;
        assert_eq!(
            extract_reason(body).unwrap(),
            Some("slow query".to_string())
        );
    }

    #[test]
    fn extract_reason_empty_body() {
        assert_eq!(extract_reason(b"").unwrap(), None);
    }

    #[test]
    fn extract_reason_no_reason_field() {
        let body = br#"{"other": "value"}"#;
        assert_eq!(extract_reason(body).unwrap(), None);
    }

    #[test]
    fn extract_reason_malformed_json() {
        let body = b"not json at all";
        assert!(extract_reason(body).is_err());
    }

    #[test]
    fn error_json_format() {
        let json = error_json(404, "not found");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["error"], "not found");
        assert_eq!(value["status"], 404);
    }

    #[test]
    fn route_list_sessions() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/admin/sessions".to_string(),
            query_string: "tenant_id=test".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(value["sessions"].is_array());
    }

    #[test]
    fn route_list_sessions_missing_tenant() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/admin/sessions".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 400);
    }

    #[test]
    fn route_cancel_not_found() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/admin/sessions/999999/cancel".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 404); // not found since no session exists
    }

    #[test]
    fn route_terminate_not_found() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/admin/sessions/999999/terminate".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 404);
    }

    #[test]
    fn route_terminate_all_empty_tenant() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/admin/tenants//terminate-all".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        // Empty segment matches route with empty id → fail-closed 400.
        let (status, _) = route(&req);
        assert_eq!(status, 400); // fail-closed: empty tenant_id
    }

    #[test]
    fn route_invalid_connection_id() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/admin/sessions/notanumber/cancel".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 400);
    }

    #[test]
    fn route_wrong_method() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/admin/sessions/1/cancel".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 405);
    }

    #[test]
    fn route_not_found() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/unknown/path".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 404);
    }

    #[test]
    fn check_auth_valid() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer my-secret".to_string());
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert!(check_auth(&req, "my-secret"));
    }

    #[test]
    fn check_auth_invalid() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer wrong".to_string());
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert!(!check_auth(&req, "my-secret"));
    }

    #[test]
    fn check_auth_missing_header() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        assert!(!check_auth(&req, "my-secret"));
    }

    #[test]
    fn check_auth_wrong_scheme() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Basic my-secret".to_string());
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert!(!check_auth(&req, "my-secret"));
    }

    #[test]
    fn check_auth_case_insensitive_bearer() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "bearer my-secret".to_string());
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert!(check_auth(&req, "my-secret"));
    }

    #[test]
    fn check_auth_mixed_case_bearer() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "BEARER my-secret".to_string());
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert!(check_auth(&req, "my-secret"));
    }

    #[test]
    fn route_invalid_state_filter() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/admin/sessions".to_string(),
            query_string: "tenant_id=test&state=actve".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 400);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("invalid state filter"));
    }

    #[test]
    fn route_list_sessions_limit_capped() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/admin/sessions".to_string(),
            query_string: "tenant_id=test&limit=999999999".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _body) = route(&req);
        // Should succeed (limit silently capped to MAX_LIST_LIMIT), not error.
        assert_eq!(status, 200);
    }

    #[test]
    fn terminate_all_success_empty_tenant() {
        // terminate-all with an actual tenant that has no sessions
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/admin/tenants/nonexistent_tenant/terminate-all".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 200); // empty tenant returns success with 0 counts
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["requested"], 0);
        assert_eq!(value["terminated"], 0);
    }
}
