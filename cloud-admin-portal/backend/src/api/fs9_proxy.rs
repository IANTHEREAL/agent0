use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::auth::CustomerAuth;
use crate::db;
use crate::AppState;

/// Reverse-proxy /fs9/{db_id}/{*rest} to the configured fs9-server.
///
/// Authenticates the customer, fetches the stored fs9 JWT for that database,
/// and forwards the request to `{FS9_SERVER_URL}/{db_id}/{rest}`.
/// If the stored token is expired (fs9-server returns 401), automatically
/// regenerates it via fs9-meta and retries once.
pub async fn fs9_proxy(
    State(state): State<AppState>,
    auth: CustomerAuth,
    req: Request,
) -> Response {
    // Parse db_id from /fs9/{db_id}[/...]
    let path = req.uri().path().to_string();
    let db_id = match path.strip_prefix("/fs9/") {
        Some(rest) => rest.split('/').next().unwrap_or("").to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, "Invalid /fs9/ path").into_response();
        }
    };
    if db_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing database ID in path").into_response();
    }
    let Some(fs9_url) = state.config.fs9_server_url.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "FS9 server not configured (FS9_SERVER_URL unset)",
        )
            .into_response();
    };

    // Verify the customer owns this database and it is active.
    let tenant = match db::get_tenant_for_customer(&state.db, &db_id, &auth.customer_id).await {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::NOT_FOUND, "Database not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    };

    if tenant.state != crate::tenant_state::ACTIVE {
        return (
            StatusCode::CONFLICT,
            "Database is not active",
        )
            .into_response();
    }

    // Retrieve the stored fs9 JWT for this tenant.
    let fs9_token = match db::get_credential(
        &state.db,
        &tenant.id,
        "fs9_token",
        state.config.credential_key.as_deref(),
    )
    .await
    {
        Ok(Some(cred)) => cred.password_plain,
        Ok(None) => {
            // No token stored — try to provision one now.
            match refresh_fs9_token(&state, &tenant.id, &auth.customer_id).await {
                Ok(token) => token,
                Err(e) => {
                    return (StatusCode::UNAUTHORIZED, format!("No fs9 token and auto-provision failed: {e}")).into_response();
                }
            }
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Build the upstream URL.
    let uri = req.uri();
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let prefix = format!("/fs9/{db_id}");
    let after_prefix = path_and_query
        .strip_prefix(&prefix)
        .unwrap_or(path_and_query);
    let upstream_url = format!(
        "{}/{db_id}{}",
        fs9_url.trim_end_matches('/'),
        after_prefix,
    );

    // Collect request body (10 MB limit).
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Failed to read body: {e}")).into_response();
        }
    };

    // Forward request with fs9 token.
    let resp = forward_to_fs9(
        &state.http_client, &method, &upstream_url, &headers, &body_bytes, &fs9_token,
    ).await;

    match resp {
        Ok((status, resp_headers, resp_body)) => {
            if status == StatusCode::UNAUTHORIZED {
                // Token expired — refresh and retry once.
                tracing::info!(db_id, "fs9 token expired, refreshing");
                match refresh_fs9_token(&state, &tenant.id, &auth.customer_id).await {
                    Ok(new_token) => {
                        match forward_to_fs9(
                            &state.http_client, &method, &upstream_url, &headers, &body_bytes, &new_token,
                        ).await {
                            Ok((status, headers, body)) => build_response(status, headers, body),
                            Err(e) => (StatusCode::BAD_GATEWAY, format!("Upstream error on retry: {e}")).into_response(),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(db_id, error = %e, "Failed to refresh fs9 token");
                        build_response(status, resp_headers, resp_body)
                    }
                }
            } else {
                build_response(status, resp_headers, resp_body)
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response(),
    }
}

async fn forward_to_fs9(
    client: &reqwest::Client,
    method: &axum::http::Method,
    url: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
    token: &str,
) -> Result<(StatusCode, reqwest::header::HeaderMap, bytes::Bytes), String> {
    let mut fwd = client
        .request(method.clone(), url)
        .header("Authorization", format!("Bearer {token}"));

    for (key, value) in headers.iter() {
        let key_str = key.as_str();
        if key_str != "authorization" && key_str != "host" {
            fwd = fwd.header(key, value);
        }
    }
    fwd = fwd.body(body.to_vec());

    let resp = fwd.send().await.map_err(|e| e.to_string())?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let resp_body = resp.bytes().await.map_err(|e| e.to_string())?;
    Ok((status, resp_headers, resp_body))
}

fn build_response(
    status: StatusCode,
    resp_headers: reqwest::header::HeaderMap,
    resp_body: bytes::Bytes,
) -> Response {
    let mut response = Response::new(Body::from(resp_body));
    *response.status_mut() = status;
    for (key, value) in resp_headers.iter() {
        if key.as_str() != "transfer-encoding" && key.as_str() != "connection" {
            if let Ok(name) = axum::http::HeaderName::from_bytes(key.as_str().as_bytes()) {
                if let Ok(val) = axum::http::HeaderValue::from_bytes(value.as_bytes()) {
                    response.headers_mut().insert(name, val);
                }
            }
        }
    }
    response
}

/// Regenerate the fs9 JWT token for a tenant via fs9-meta and store it.
async fn refresh_fs9_token(
    state: &AppState,
    tenant_id: &str,
    customer_id: &str,
) -> Result<String, String> {
    let fs9 = state.fs9_client.as_ref().ok_or("FS9 integration not configured")?;

    // Ensure namespace + user exist (idempotent).
    fs9.create_namespace(tenant_id).await?;
    let user_id = fs9.create_user(tenant_id, "admin").await?;
    let token = fs9.generate_token(&user_id).await?;

    // Store the new token.
    db::upsert_credential(
        &state.db,
        tenant_id,
        "fs9_token",
        customer_id,
        &token,
        state.config.credential_key.as_deref(),
    )
    .await
    .map_err(|e| format!("Failed to store refreshed token: {e}"))?;

    tracing::info!(tenant_id, "Refreshed fs9 token");
    Ok(token)
}
