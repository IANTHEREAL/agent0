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
/// The `db_id` is parsed from the URI path rather than using axum Path
/// extraction, since this handler serves two routes with different arity.
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
            return (StatusCode::UNAUTHORIZED, "No fs9 token found for this database. Re-create the database or wait for provisioning.").into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Build the upstream URL.
    // Incoming path:  /fs9/{db_id}[/rest][?query]
    // Upstream path:  {fs9_url}/{db_id}[/rest][?query]
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

    // Build the forwarded request, replacing Authorization with the fs9 token.
    let mut fwd = state
        .http_client
        .request(method, &upstream_url)
        .header("Authorization", format!("Bearer {fs9_token}"));

    for (key, value) in headers.iter() {
        let key_str = key.as_str();
        if key_str != "authorization" && key_str != "host" {
            fwd = fwd.header(key, value);
        }
    }
    fwd = fwd.body(body_bytes);

    match fwd.send().await {
        Ok(resp) => {
            let status = resp.status();
            let resp_headers = resp.headers().clone();
            let resp_body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
                }
            };

            let mut response = Response::new(Body::from(resp_body));
            *response.status_mut() = status;
            for (key, value) in resp_headers.iter() {
                // Skip hop-by-hop headers that must not be forwarded.
                if key.as_str() != "transfer-encoding" && key.as_str() != "connection" {
                    response.headers_mut().insert(key, value.clone());
                }
            }
            response
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response(),
    }
}
