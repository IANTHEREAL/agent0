use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::db;
use crate::error::AppError;
use crate::AppState;

pub struct ApiKeyAuth;

impl FromRequestParts<AppState> for ApiKeyAuth {
    type Rejection = AppError;

    fn from_request_parts<'life0, 'life1, 'async_trait>(
        parts: &'life0 mut Parts,
        state: &'life1 AppState,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self, Self::Rejection>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if !state.config.auth_enabled() {
                return Ok(Self);
            }

            let key = parts.headers.get("X-API-Key").and_then(|v| v.to_str().ok());

            match key {
                None => Err(AppError::unauthorized(
                    "API key required. Pass X-API-Key header.",
                )),
                Some(k) => {
                    if state.config.api_keys.iter().any(|allowed| allowed == k) {
                        Ok(Self)
                    } else {
                        Err(AppError::unauthorized("Invalid API key"))
                    }
                }
            }
        })
    }
}

pub struct CustomerAuth {
    pub customer_id: String,
}

impl FromRequestParts<AppState> for CustomerAuth {
    type Rejection = AppError;

    fn from_request_parts<'life0, 'life1, 'async_trait>(
        parts: &'life0 mut Parts,
        state: &'life1 AppState,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self, Self::Rejection>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let auth_header = parts
                .headers
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    AppError::unauthorized("Authorization header required. Use Bearer <token>.")
                })?;

            let token_str = if auth_header.len() > 7
                && auth_header[..7].eq_ignore_ascii_case("bearer ")
            {
                &auth_header[7..]
            } else {
                return Err(AppError::unauthorized(
                    "Invalid Authorization header. Expected: Bearer <token>",
                ));
            };

            if token_str.is_empty() {
                return Err(AppError::unauthorized("Bearer token must not be empty"));
            }

            let hash_bytes = Sha256::digest(token_str.as_bytes());
            let token_hash = hash_bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();

            let token_row = db::get_customer_token(&state.db, &token_hash)
                .await
                .map_err(|_| AppError::internal("Failed to validate token"))?
                .ok_or_else(|| AppError::unauthorized("Invalid or expired token"))?;

            // Check expiry if set
            if let Some(ref expires_at) = token_row.expires_at {
                if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                    if exp < chrono::Utc::now() {
                        return Err(AppError::unauthorized("Token has expired"));
                    }
                }
            }

            Ok(CustomerAuth {
                customer_id: token_row.customer_id,
            })
        })
    }
}

pub struct TenantSessionExtractor {
    pub session_id: String,
    pub tenant_id: String,
    pub admin_user: String,
    pub admin_password: String,
}

impl TenantSessionExtractor {
    pub fn from_headers(
        headers: &HeaderMap,
        tenant_id: &str,
        state: &AppState,
    ) -> Result<Self, AppError> {
        let session_id = headers
            .get("X-Tenant-Session")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                AppError::unauthorized(
                    "Tenant session required. Use POST /api/tenants/{id}/connect first.",
                )
            })?;

        let session = state
            .sessions
            .validate_session(session_id, tenant_id)
            .ok_or_else(|| AppError::unauthorized("Invalid or expired session"))?;

        Ok(Self {
            session_id: session.session_id.clone(),
            tenant_id: session.tenant_id.clone(),
            admin_user: session.admin_user.clone(),
            admin_password: session.admin_password.clone(),
        })
    }
}
