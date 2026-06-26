//! Internal control endpoint for backend admin proxy.
//!
//! This is the Option C **primary path** server surface — the endpoint that
//! the backend admin proxy calls to execute session management operations.
//!
//! Distinct from the break-glass surface (`http.rs`):
//! - Non-loopback capable (`INTERNAL_CONTROL_LISTEN_ADDR`, default `0.0.0.0`)
//! - Service-to-service auth via `INTERNAL_CONTROL_SECRET`
//! - Actor identity passthrough via `X-Admin-Actor` header (for audit logs)
//! - Route prefix `/internal/` (vs `/admin/` for break-glass)
//!
//! Endpoints:
//! - `GET  /internal/sessions`                  — list sessions (filtered, paginated)
//! - `POST /internal/sessions/:id/cancel`       — cancel current query (57014)
//! - `POST /internal/sessions/:id/terminate`    — kill connection
//! - `POST /internal/tenants/:id/terminate-all` — bulk terminate by tenant

use super::control::{AdminControlService, ControlError};
use super::session_registry::{SessionFilter, SessionSnapshot, SessionState};
use crate::pool::TikvClientPool;
use crate::storage::FencedDatabase;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

const MAX_HEADER_SIZE: usize = 8192;
const MAX_BODY_SIZE: usize = 4096;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(75);
const DEFAULT_FENCE_DELETE_WAIT_MS: u64 = 60_000;
const FENCE_DELETE_POLL_MS: u64 = 250;

/// Required header for actor identity passthrough from the backend proxy.
const ACTOR_HEADER: &str = "x-admin-actor";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the internal control HTTP accept loop. Call from `tokio::spawn`.
pub async fn start_internal_control_server(
    listener: TcpListener,
    secret: String,
    client_pool: Arc<TikvClientPool>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let s = secret.clone();
                let pool = client_pool.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        tokio::time::timeout(REQUEST_TIMEOUT, handle_connection(stream, &s, pool))
                            .await
                    {
                        debug!("internal-control connection from {} timed out: {}", peer, e);
                    }
                });
            }
            Err(e) => {
                warn!("internal-control accept error: {}", e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(mut stream: TcpStream, secret: &str, client_pool: Arc<TikvClientPool>) {
    let req = match read_request(&mut stream).await {
        Ok(r) => r,
        Err(status) => {
            let _ = write_response(&mut stream, status, &error_json(status, "bad request")).await;
            return;
        }
    };

    // Auth check — service-to-service bearer token.
    if !check_auth(&req, secret) {
        let _ = write_response(&mut stream, 401, &error_json(401, "unauthorized")).await;
        return;
    }

    let (status, body) = if is_fence_delete_request(&req) {
        handle_fence_delete(&req, client_pool).await
    } else {
        route(&req)
    };
    let _ = write_response(&mut stream, status, &body).await;
}

// ---------------------------------------------------------------------------
// HTTP request parsing (same structure as break-glass, separate instance)
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

    let request_line = lines.next().ok_or(400u16)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(400u16)?.to_uppercase();
    let uri = parts.next().ok_or(400u16)?;

    let (path, query_string) = match uri.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (uri.to_string(), String::new()),
    };

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }

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
        let already_read = filled.saturating_sub(body_start);
        if already_read > 0 {
            let take = already_read.min(content_length);
            body.extend_from_slice(&buf[body_start..body_start + take]);
        }
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
// Auth — service-to-service bearer token
// ---------------------------------------------------------------------------

fn check_auth(req: &HttpRequest, secret: &str) -> bool {
    let Some(auth_header) = req.headers.get("authorization") else {
        return false;
    };
    let Some(token) = auth_header.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.as_bytes(), secret.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Actor extraction
// ---------------------------------------------------------------------------

/// Extract the admin actor from the `X-Admin-Actor` header.
/// Returns 400 if the header is missing or empty — actor identity is
/// required on the internal control path for audit integrity.
fn extract_actor(req: &HttpRequest) -> Result<String, (u16, String)> {
    match req.headers.get(ACTOR_HEADER) {
        Some(actor) if !actor.is_empty() => Ok(actor.clone()),
        _ => Err((
            400,
            error_json(400, "missing or empty X-Admin-Actor header"),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct FenceDeleteRequest {
    keyspace: Option<String>,
    reason: Option<String>,
    wait_ms: Option<u64>,
}

fn is_fence_delete_request(req: &HttpRequest) -> bool {
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    matches!(
        (req.method.as_str(), segments.as_slice()),
        ("POST", ["internal", "tenants", _, "fence-delete"])
    )
}

fn keyspace_from_tenant_segment(segment: &str) -> String {
    if segment == "default" || segment.starts_with("db9_tenant_") {
        segment.to_string()
    } else {
        format!("db9_tenant_{segment}")
    }
}

fn resolve_fence_delete_keyspace(
    tenant_segment: &str,
    body: &FenceDeleteRequest,
) -> Result<String, (u16, String)> {
    let path_keyspace =
        crate::worker::canonical_registry_keyspace(&keyspace_from_tenant_segment(tenant_segment));
    let Some(body_keyspace) = body.keyspace.as_ref().filter(|s| !s.trim().is_empty()) else {
        return Ok(path_keyspace);
    };

    let body_keyspace = crate::worker::canonical_registry_keyspace(body_keyspace.trim());
    if body_keyspace != path_keyspace {
        return Err((400, error_json(400, "body keyspace must match URL tenant")));
    }
    Ok(path_keyspace)
}

fn fence_delete_response_status(resp: &serde_json::Value) -> u16 {
    match resp.get("drained").and_then(serde_json::Value::as_bool) {
        Some(false) => 409,
        _ => 200,
    }
}

fn annotate_fence_delete_not_drained(resp: &mut serde_json::Value) {
    if let Some(obj) = resp.as_object_mut() {
        obj.entry("error").or_insert_with(|| {
            serde_json::json!(
                "Database deletion is in progress; waiting for active connections to drain"
            )
        });
    }
}

async fn handle_fence_delete(req: &HttpRequest, client_pool: Arc<TikvClientPool>) -> (u16, String) {
    let actor = match extract_actor(req) {
        Ok(a) => a,
        Err(resp) => return resp,
    };

    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let tenant_segment = match segments.as_slice() {
        ["internal", "tenants", id, "fence-delete"] => *id,
        _ => return (404, error_json(404, "not found")),
    };

    let body = if req.body.is_empty() {
        FenceDeleteRequest {
            keyspace: None,
            reason: None,
            wait_ms: None,
        }
    } else {
        match serde_json::from_slice::<FenceDeleteRequest>(&req.body) {
            Ok(body) => body,
            Err(_) => return (400, error_json(400, "invalid JSON body")),
        }
    };

    let keyspace = match resolve_fence_delete_keyspace(tenant_segment, &body) {
        Ok(keyspace) => keyspace,
        Err(resp) => return resp,
    };
    let reason = body
        .reason
        .as_deref()
        .unwrap_or("tenant delete fence")
        .to_string();
    let wait_ms = body.wait_ms.unwrap_or(DEFAULT_FENCE_DELETE_WAIT_MS);

    match fence_delete_impl(&keyspace, &actor, &reason, wait_ms, client_pool).await {
        Ok(mut resp) => {
            let status = fence_delete_response_status(&resp);
            if status == 409 {
                annotate_fence_delete_not_drained(&mut resp);
            }
            (status, resp.to_string())
        }
        Err((status, message)) => (status, error_json(status, &message)),
    }
}

async fn fence_delete_impl(
    keyspace: &str,
    actor: &str,
    reason: &str,
    wait_ms: u64,
    client_pool: Arc<TikvClientPool>,
) -> Result<serde_json::Value, (u16, String)> {
    let system_store = crate::worker::system_store().map_err(|e| (503, e.to_string()))?;
    let tenant_store = client_pool
        .open_keyspace_without_bootstrap(keyspace.to_string())
        .await
        .map_err(|e| {
            (
                502,
                format!("failed to open tenant keyspace '{keyspace}': {e}"),
            )
        })?;

    let fenced = tenant_store
        .fence_all_databases_for_tenant_delete()
        .await
        .map_err(|e| (500, format!("failed to fence tenant databases: {e}")))?;

    crate::worker::database_lifecycle::request_database_drain_once(system_store, keyspace, &fenced)
        .await
        .map_err(|e| {
            (
                500,
                format!("failed to publish database drain request: {e}"),
            )
        })?;

    let registry = super::global_session_registry();
    let control = AdminControlService::new(registry);
    let terminate_result = control
        .terminate_all(keyspace, actor, Some(reason))
        .map(|r| {
            serde_json::json!({
                "requested": r.requested,
                "terminated": r.terminated,
                "already_closed": r.already_closed,
            })
        })
        .unwrap_or_else(|e| {
            warn!(
                keyspace = %keyspace,
                "tenant fence-delete failed to terminate local sessions: {e}"
            );
            serde_json::json!({
                "requested": 0,
                "terminated": 0,
                "already_closed": 0,
                "error": e.to_string(),
            })
        });

    if let Err(e) = crate::worker::database_lifecycle::publish_requested_database_drain_states_once(
        system_store,
        &client_pool,
    )
    .await
    {
        warn!(
            keyspace = %keyspace,
            "tenant fence-delete failed to publish immediate local drain state: {e}"
        );
    }

    let started = Instant::now();
    let timeout = Duration::from_millis(wait_ms);
    let mut drained = crate::worker::database_lifecycle::fenced_databases_read_drain_allows_delete(
        system_store,
        keyspace,
        &fenced,
    )
    .await
    .map_err(|e| (500, format!("failed to evaluate database drain: {e}")))?;

    while !drained && started.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(FENCE_DELETE_POLL_MS)).await;
        if let Err(e) =
            crate::worker::database_lifecycle::publish_requested_database_drain_states_once(
                system_store,
                &client_pool,
            )
            .await
        {
            warn!(
                keyspace = %keyspace,
                "tenant fence-delete failed to refresh local drain state while waiting: {e}"
            );
        }
        drained = crate::worker::database_lifecycle::fenced_databases_read_drain_allows_delete(
            system_store,
            keyspace,
            &fenced,
        )
        .await
        .map_err(|e| (500, format!("failed to evaluate database drain: {e}")))?;
    }

    if drained {
        if let Err(e) = crate::worker::database_lifecycle::clear_database_drain_requests_once(
            system_store,
            keyspace,
            &fenced,
        )
        .await
        {
            warn!(
                keyspace = %keyspace,
                "tenant fence-delete drained but failed to clear drain requests: {e}"
            );
        }
    }

    let fenced_json: Vec<serde_json::Value> = fenced
        .iter()
        .map(|FencedDatabase { db_id, epoch }| {
            serde_json::json!({
                "db_id": db_id,
                "epoch": epoch,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "keyspace": keyspace,
        "fenced_databases": fenced_json,
        "drained": drained,
        "waited_ms": started.elapsed().as_millis() as u64,
        "local_terminate": terminate_result,
    }))
}

// ---------------------------------------------------------------------------
// Routing — /internal/ prefix
// ---------------------------------------------------------------------------

fn route(req: &HttpRequest) -> (u16, String) {
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();

    match (req.method.as_str(), segments.as_slice()) {
        // GET /internal/sessions
        ("GET", ["internal", "sessions"]) => handle_list_sessions(req),

        // POST /internal/sessions/:id/cancel
        ("POST", ["internal", "sessions", id, "cancel"]) => match id.parse::<i64>() {
            Ok(conn_id) => handle_cancel(conn_id, req),
            Err(_) => (400, error_json(400, "invalid connection_id")),
        },

        // POST /internal/sessions/:id/terminate
        ("POST", ["internal", "sessions", id, "terminate"]) => match id.parse::<i64>() {
            Ok(conn_id) => handle_terminate(conn_id, req),
            Err(_) => (400, error_json(400, "invalid connection_id")),
        },

        // POST /internal/tenants/:id/terminate-all
        ("POST", ["internal", "tenants", id, "terminate-all"]) => handle_terminate_all(id, req),

        // Method exists but wrong HTTP method
        ("GET", ["internal", "sessions", _, "cancel"])
        | ("GET", ["internal", "sessions", _, "terminate"])
        | ("GET", ["internal", "tenants", _, "terminate-all"])
        | ("POST", ["internal", "sessions"]) => (405, error_json(405, "method not allowed")),

        _ => (404, error_json(404, "not found")),
    }
}

// ---------------------------------------------------------------------------
// Route handlers — actor from X-Admin-Actor header
// ---------------------------------------------------------------------------

fn handle_list_sessions(req: &HttpRequest) -> (u16, String) {
    let params = parse_query_params(&req.query_string);

    let tenant_id = params.get("tenant_id").cloned();
    let all_tenants = params
        .get("all_tenants")
        .map(|v| v == "true")
        .unwrap_or(false);
    let principal = params.get("principal").cloned();
    let state = match params.get("state") {
        Some(s) => match parse_session_state(s) {
            Some(st) => Some(st),
            None => {
                return (
                    400,
                    error_json(400, &format!("invalid state filter: {}", s)),
                )
            }
        },
        None => None,
    };
    let min_duration_ms = params.get("min_duration_ms").and_then(|v| v.parse().ok());
    let limit = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100usize);
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
    let actor = match extract_actor(req) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let reason = match extract_reason(&req.body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let registry = super::global_session_registry();
    let svc = AdminControlService::new(registry);

    match svc.cancel_query(connection_id, &actor, reason.as_deref()) {
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
    let actor = match extract_actor(req) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let reason = match extract_reason(&req.body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let registry = super::global_session_registry();
    let svc = AdminControlService::new(registry);

    match svc.terminate(connection_id, &actor, reason.as_deref()) {
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
    let actor = match extract_actor(req) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let reason = match extract_reason(&req.body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let registry = super::global_session_registry();
    let svc = AdminControlService::new(registry);

    match svc.terminate_all(tenant_id, &actor, reason.as_deref()) {
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

fn snapshot_to_json(s: &SessionSnapshot) -> serde_json::Value {
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

fn extract_reason(body: &[u8]) -> Result<Option<String>, (u16, String)> {
    if body.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| (400, error_json(400, "invalid JSON body")))?;
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
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_text(status),
        body.len(),
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

    // -- Auth tests --

    #[test]
    fn auth_valid_bearer() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer svc-secret".to_string());
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert!(check_auth(&req, "svc-secret"));
    }

    #[test]
    fn auth_wrong_token() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer wrong".to_string());
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert!(!check_auth(&req, "svc-secret"));
    }

    #[test]
    fn auth_missing_header() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        assert!(!check_auth(&req, "svc-secret"));
    }

    // -- Actor extraction tests --

    #[test]
    fn actor_present() {
        let mut headers = HashMap::new();
        headers.insert(ACTOR_HEADER.to_string(), "api_key:key_123".to_string());
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        assert_eq!(extract_actor(&req).unwrap(), "api_key:key_123");
    }

    #[test]
    fn actor_missing() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = extract_actor(&req).unwrap_err();
        assert_eq!(status, 400);
    }

    #[test]
    fn actor_empty() {
        let mut headers = HashMap::new();
        headers.insert(ACTOR_HEADER.to_string(), "".to_string());
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/".to_string(),
            query_string: String::new(),
            headers,
            body: Vec::new(),
        };
        let (status, _) = extract_actor(&req).unwrap_err();
        assert_eq!(status, 400);
    }

    // -- Routing tests --

    #[test]
    fn route_list_sessions() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/internal/sessions".to_string(),
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
            path: "/internal/sessions".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 400);
    }

    #[test]
    fn route_cancel_missing_actor() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/sessions/999999/cancel".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(), // no X-Admin-Actor
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 400);
        assert!(body.contains("X-Admin-Actor"));
    }

    #[test]
    fn route_cancel_with_actor_not_found() {
        let mut headers = HashMap::new();
        headers.insert(ACTOR_HEADER.to_string(), "api_key:k1".to_string());
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/sessions/999999/cancel".to_string(),
            query_string: "".to_string(),
            headers,
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 404); // session not found
    }

    #[test]
    fn route_terminate_missing_actor() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/sessions/999999/terminate".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 400);
        assert!(body.contains("X-Admin-Actor"));
    }

    #[test]
    fn route_terminate_with_actor_not_found() {
        let mut headers = HashMap::new();
        headers.insert(ACTOR_HEADER.to_string(), "api_key:k1".to_string());
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/sessions/999999/terminate".to_string(),
            query_string: "".to_string(),
            headers,
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 404);
    }

    #[test]
    fn route_terminate_all_missing_actor() {
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/tenants/t1/terminate-all".to_string(),
            query_string: "".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 400);
        assert!(body.contains("X-Admin-Actor"));
    }

    #[test]
    fn route_terminate_all_with_actor() {
        let mut headers = HashMap::new();
        headers.insert(ACTOR_HEADER.to_string(), "api_key:k1".to_string());
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/tenants/nonexistent/terminate-all".to_string(),
            query_string: "".to_string(),
            headers,
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["requested"], 0);
    }

    #[test]
    fn route_terminate_all_empty_tenant() {
        let mut headers = HashMap::new();
        headers.insert(ACTOR_HEADER.to_string(), "api_key:k1".to_string());
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/tenants//terminate-all".to_string(),
            query_string: "".to_string(),
            headers,
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 400); // fail-closed: empty tenant_id
    }

    #[test]
    fn route_invalid_connection_id() {
        let mut headers = HashMap::new();
        headers.insert(ACTOR_HEADER.to_string(), "api_key:k1".to_string());
        let req = HttpRequest {
            method: "POST".to_string(),
            path: "/internal/sessions/notanumber/cancel".to_string(),
            query_string: "".to_string(),
            headers,
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 400);
    }

    #[test]
    fn fence_delete_keyspace_comes_from_url_when_body_omits_keyspace() {
        let body = FenceDeleteRequest {
            keyspace: None,
            reason: None,
            wait_ms: None,
        };

        assert_eq!(
            resolve_fence_delete_keyspace("tenant_a", &body).unwrap(),
            "db9_tenant_tenant_a"
        );
    }

    #[test]
    fn fence_delete_rejects_body_keyspace_that_disagrees_with_url() {
        let body = FenceDeleteRequest {
            keyspace: Some("db9_tenant_b".to_string()),
            reason: None,
            wait_ms: None,
        };

        let (status, response) = resolve_fence_delete_keyspace("tenant_a", &body).unwrap_err();
        assert_eq!(status, 400);
        assert!(response.contains("body keyspace must match URL tenant"));
    }

    #[test]
    fn fence_delete_response_is_conflict_until_drain_completes() {
        let mut response = serde_json::json!({
            "keyspace": "db9_tenant_a",
            "fenced_databases": [{"db_id": 7, "epoch": 3}],
            "drained": false,
            "waited_ms": 0,
        });

        assert_eq!(fence_delete_response_status(&response), 409);
        annotate_fence_delete_not_drained(&mut response);
        assert_eq!(response["drained"], false);
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("waiting for active connections to drain"));
    }

    #[test]
    fn fence_delete_response_is_success_after_drain_completes() {
        let response = serde_json::json!({
            "keyspace": "db9_tenant_a",
            "fenced_databases": [{"db_id": 7, "epoch": 3}],
            "drained": true,
            "waited_ms": 10,
        });

        assert_eq!(fence_delete_response_status(&response), 200);
        assert!(response.get("error").is_none());
    }

    #[test]
    fn route_wrong_method() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/internal/sessions/1/cancel".to_string(),
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
    fn route_admin_prefix_not_matched() {
        // /admin/ routes should NOT match on this surface
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/admin/sessions".to_string(),
            query_string: "tenant_id=test".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, _) = route(&req);
        assert_eq!(status, 404);
    }

    // -- Helper tests --

    #[test]
    fn extract_reason_from_json() {
        let body = br#"{"reason": "runaway query"}"#;
        assert_eq!(
            extract_reason(body).unwrap(),
            Some("runaway query".to_string())
        );
    }

    #[test]
    fn extract_reason_empty_body() {
        assert_eq!(extract_reason(b"").unwrap(), None);
    }

    #[test]
    fn extract_reason_invalid_json() {
        assert!(extract_reason(b"not json").is_err());
    }

    #[test]
    fn route_list_sessions_invalid_state() {
        let req = HttpRequest {
            method: "GET".to_string(),
            path: "/internal/sessions".to_string(),
            query_string: "tenant_id=test&state=bogus".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let (status, body) = route(&req);
        assert_eq!(status, 400);
        assert!(body.contains("invalid state filter"));
    }

    #[test]
    fn parse_session_state_roundtrip() {
        assert_eq!(parse_session_state("idle"), Some(SessionState::Idle));
        assert_eq!(parse_session_state("active"), Some(SessionState::Active));
        assert_eq!(
            parse_session_state("idle_in_transaction"),
            Some(SessionState::IdleInTransaction)
        );
        assert_eq!(parse_session_state("unknown"), None);
    }

    #[test]
    fn error_json_format() {
        let json = error_json(400, "bad request");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["error"], "bad request");
        assert_eq!(value["status"], 400);
    }
}
