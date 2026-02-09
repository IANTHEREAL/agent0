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
        "name": "pg-tikv Admin API",
        "version": "2.0.0",
        "docs": "/api/docs",
    }))
}
