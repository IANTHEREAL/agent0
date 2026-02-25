use axum::extract::State;
use axum::Json;

use crate::auth::ApiKeyAuth;
use crate::error::AppError;
use crate::models::HealthResponse;
use crate::services::pd_client::PdClient;
use crate::AppState;

pub async fn health_check(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
) -> Result<Json<HealthResponse>, AppError> {
    let pd = PdClient::new(&state.config.pd_endpoints, &state.http_client);
    let pd_healthy = pd.check_health().await;

    Ok(Json(HealthResponse {
        status: if pd_healthy {
            "healthy".into()
        } else {
            "degraded".into()
        },
        pd_healthy,
    }))
}

pub async fn api_info(_auth: ApiKeyAuth) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "db9-server Admin API",
        "version": "2.0.0",
        "docs": "/api/docs",
    }))
}

pub async fn credential_migration_status(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
) -> Result<Json<serde_json::Value>, AppError> {
    let status = crate::db::credential_migration_status(&state.db).await?;
    Ok(Json(serde_json::json!({
        "total_credentials": status.total_credentials,
        "encrypted": status.encrypted,
        "plaintext": status.plaintext,
        "unclassified": status.unclassified,
        "migration_complete": status.migration_complete,
    })))
}

pub async fn migrate_credentials(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
) -> Result<Json<serde_json::Value>, AppError> {
    let key = state.config.credential_key.as_deref().ok_or_else(|| {
        AppError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "DB9_CREDENTIAL_KEY is not configured — cannot re-encrypt credentials",
        )
    })?;

    let migrated = crate::db::migrate_credentials(&state.db, key).await?;
    Ok(Json(serde_json::json!({
        "migrated": migrated,
    })))
}
